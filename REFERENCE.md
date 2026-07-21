# Reference

Detailed config, script, and protocol notes that were trimmed from the main README.

## Features

- `POST /v1/messages` translation from Anthropic Messages to OpenAI Responses.
- Streaming via SSE end to end.
- Tool-use round trips: `tool_use` ↔ `tool_calls`, `tool_result` ↔ `tool` messages.
- Stop reason mapping: `stop` → `end_turn`, `length` → `max_tokens`, `tool_calls` → `tool_use`, `content_filter` → `end_turn`.
- Error translation from OpenAI `{error: {...}}` envelopes and HTTP statuses into Anthropic-shaped errors.
- Usage translation from OpenAI `usage` into Anthropic `usage`.
- Optional prompt-caching translation from Anthropic `cache_control` to OpenAI `prompt_cache_breakpoint`.
- `GET /healthz` for liveness checks.

## Configuration

The proxy reads environment variables first, then an optional `proxy.toml`, then built-in defaults. Environment variables win on conflict.

| Variable | Default | Required? | Notes |
|---|---|---|---|
| `UPSTREAM_BASE_URL` | - | yes | e.g. `https://api.openai.com` |
| `UPSTREAM_API_KEY` | - | yes | bearer token for the upstream |
| `LISTEN_ADDR` | `0.0.0.0:8085` | no | socket address to bind |
| `UPSTREAM_PATH` | `/v1/responses` | no | appended to the base URL |
| `REQUEST_TIMEOUT_SECS` | `600` | no | per-request timeout |
| `PROMPT_CACHING_ENABLED` | `false` | no | set `true` to forward OpenAI prompt-cache hints |
| `PROMPT_CACHE_KEY` | - | no | optional stable key for `prompt_cache_key` |
| `PROXY_KEY` | - | no | shared secret for `X-Proxy-Key` auth (see "Client auth" below) |
| `LOG_TO_DISK` | `false` | no | set `1`/`true`/`yes` to write `target/logs/proxy.log` |

`proxy.toml` uses the same field names in snake_case. See `proxy.toml.example` for a starter.

### Client auth (`proxy_key` / `PROXY_KEY`)

The proxy binds `0.0.0.0:8085` by default, which means anyone on the network can reach it. If you don't set `proxy_key`, every reachable client can use your upstream API key. To lock it down, set a shared secret:

```toml
# proxy.toml
proxy_key = "any-shared-secret-string"
```

or via env:

```bash
export PROXY_KEY="any-shared-secret-string"
```

When `proxy_key` is set, every `/v1/messages` request must include a matching `X-Proxy-Key` header. Without it, the proxy returns `401 Unauthorized` with an Anthropic-shaped error envelope. The `/healthz` endpoint is always open for liveness checks. The header value is compared in constant time to avoid timing leaks. The startup output prints a warning when `proxy_key` is unset.

**Wiring it into Claude Code.** Claude Code and the Anthropic SDK inject custom HTTP headers via the `ANTHROPIC_CUSTOM_HEADERS` env var, in `Name: Value` format. Add it to your `~/.claude/settings.json`:

```json
{
  "env": {
    "ANTHROPIC_BASE_URL": "http://localhost:8085",
    "ANTHROPIC_API_KEY": "any",
    "ANTHROPIC_CUSTOM_HEADERS": "X-Proxy-Key: your-shared-secret"
  }
}
```

For multiple custom headers, separate them with newlines (a single line is enough for `X-Proxy-Key`).

**Wiring it into the helper scripts.** The `scripts/start-claude-code.sh` and `scripts/start-claude-code.ps1` launchers detect `PROXY_KEY` in the calling shell and forward it to the child as `ANTHROPIC_CUSTOM_HEADERS=X-Proxy-Key: ...` automatically. If you launch through the helper, you don't need to edit `settings.json`.

### Advanced config

- `REASONING_EFFORT` sets the legacy default effort when no per-model override applies.
- The `[reasoning]` table lets you set a default effort plus per-model overrides.
- The `[model_aliases]` table maps inbound model names to upstream model names.
- `model_aliases.default_model` is an optional fallback if the upstream rejects an alias or passthrough model.
- The `[prompt_caching]` table lets you enable translation of Anthropic `cache_control: {type: "ephemeral"}` into OpenAI `prompt_cache_breakpoint` markers.
- `prompt_caching.enabled` defaults to `false`; when disabled, no OpenAI-specific cache fields are emitted, keeping non-OpenAI upstreams unaffected.
- `prompt_caching.cache_key` is optional and forwarded as `prompt_cache_key`.

## Launching Claude Code

- `scripts/start-claude-code.sh` is the Linux/macOS launcher.
- `scripts/start-claude-code.ps1` is the Windows launcher.
- Both scripts set `ANTHROPIC_BASE_URL` to the proxy, set `ANTHROPIC_API_KEY=any`, and scrub leaked Anthropic/Claude Code env vars so the child stays routed through the proxy.
- Both default to `http://localhost:8085` and pass `--setting-sources=project,local` unless you override it.

## Context-window workaround

Claude Code decides when to auto-compact based on the model's perceived context window. It derives that window from Anthropic model names and the first-party Anthropic API, not from API responses. When the proxy rewrites `claude-sonnet-5` → `gpt-5.4-mini` (or any other alias), Claude Code does not know the upstream model's real context window and falls back to a hardcoded 200K. That causes premature auto-compaction for 1M-context models.

Set the upstream model's real context window explicitly:

- Windows PowerShell: `$env:CLAUDE_CODE_MAX_CONTEXT_TOKENS = 1000000`
- Linux/macOS shell: `export CLAUDE_CODE_MAX_CONTEXT_TOKENS=1000000`
- Claude Code `~/.claude/settings.json`: add `"CLAUDE_CODE_MAX_CONTEXT_TOKENS": "1000000"` inside the `env` object.

Use the value your upstream model actually supports (e.g., `1000000` for GPT-5.4-mini / GPT-5.6-luna, `200000` for older 200K models).

## Development

- `cargo test` runs unit and e2e tests.
- `cargo clippy --all-targets --all-features -- -D warnings` keeps the code lint-clean.
- `cargo fmt --all -- --check` checks formatting.

## Translation overview

### Request: Anthropic → OpenAI Responses

- `system` becomes the first system message.
- User text becomes user messages.
- `tool_result` blocks become OpenAI `tool` messages.
- Assistant text plus `tool_use` blocks become an assistant message with `tool_calls`.
- `tool_use.input` is serialized to JSON for OpenAI `arguments`.
- `tools[]` is mapped through with JSON Schema parameters preserved.
- `tool_choice`, `temperature`, `top_p`, `max_tokens`, and `stop_sequences` are forwarded.
- `stream: true` also enables usage reporting from the upstream.
- `cache_control` on user/system text and image blocks is translated to OpenAI `prompt_cache_breakpoint` when `[prompt_caching]` is enabled.
- `cache_control` on system prompts, tools, and assistant content is currently not translated; OpenAI caches eligible prefixes automatically.

### Response: OpenAI Responses → Anthropic

- `id` is prefixed with `msg_` when needed.
- `role` is always `assistant`.
- Text becomes `text` blocks.
- Each `tool_call` becomes a `tool_use` block.
- `stop_reason` and token usage are mapped into Anthropic shape.

### Streaming

The stream translator keeps track of open text and tool-use blocks, emits the right Anthropic content-block events as OpenAI deltas arrive, then closes the message when the upstream finishes.

## Architecture

```text
Claude Code -> axum :8085 -> translate::request (Anthropic -> OpenAI Responses) -> reqwest -> upstream
upstream SSE/response -> translate::response or ::stream (OpenAI -> Anthropic) -> axum -> Claude Code
```

## Out of scope

- Vision / image inputs
- Extended thinking blocks
- Tool-level and assistant-level prompt-cache control markers
- Server-side tools (`web_search`, `web_fetch`, `code_execution`)
- `/v1/models` listing
- TLS termination

## License

This is a personal/professional project; no license is granted by default. Add one if you intend to distribute.
