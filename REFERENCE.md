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
- The server-side `web_fetch` tool, implemented by the proxy: it fetches the URL itself, extracts the readable text, and returns it as an Anthropic `web_fetch_tool_result` (see "Server-side `web_fetch`" below).
- `GET /healthz` for liveness checks.

## Configuration

The proxy reads environment variables first, then an optional `proxy.json`, then built-in defaults. Environment variables win on conflict.

| Variable | Default | Required? | Notes |
|---|---|---|---|
| `UPSTREAM_BASE_URL` | - | yes | e.g. `https://api.openai.com` |
| `UPSTREAM_API_KEY` | - | yes | bearer token for the upstream |
| `LISTEN_ADDR` | `0.0.0.0:8085` | no | socket address to bind |
| `UPSTREAM_PATH` | `/v1/responses` | no | appended to the base URL |
| `REQUEST_TIMEOUT_SECS` | `600` | no | per-request timeout |
| `MAX_REQUEST_BODY_BYTES` | `33554432` (32 MiB) | no | positive whole-number byte cap for `/v1/messages`; invalid numeric values warn and use the default |
| `PROMPT_CACHING_ENABLED` | `false` | no | set `true` to forward OpenAI prompt-cache hints |
| `PROMPT_CACHE_KEY` | - | no | optional stable key for `prompt_cache_key` |
| `PROXY_KEY` | - | no | shared secret for `X-Proxy-Key` auth (see "Client auth" below) |
| `LOG_TO_DISK` | `false` | no | set `1`/`true`/`yes` to write `target/logs/proxy.log` |
| `WEB_FETCH_ENABLED` | `false` | no | let the proxy serve the server-side `web_fetch` tool |

`proxy.json` uses the same field names in snake_case. See `proxy.json.example` for a starter. The file is strict JSON — no comments and no trailing commas. Unknown fields are rejected at startup, so a typo will fail fast with a list of accepted keys.

`max_request_body_bytes` (overridden by `MAX_REQUEST_BODY_BYTES`) is a positive whole number of bytes; its default is 33,554,432 bytes (32 MiB). Numeric values that are zero, negative, fractional, or out of range produce a warning and fall back to the default without failing startup. Non-numeric `MAX_REQUEST_BODY_BYTES` text is ignored, so the file value or default applies.

### Client auth (`proxy_key` / `PROXY_KEY`)

The proxy binds `0.0.0.0:8085` by default, which means anyone on the network can reach it. If you don't set `proxy_key`, every reachable client can use your upstream API key. To lock it down, set a shared secret in `proxy.json`:

```json
{
  "proxy_key": "any-shared-secret-string"
}
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
- The `reasoning` object lets you set a default effort plus per-model overrides.
- `reasoning.effort_map` translates Claude Code's `output_config.effort`
  (`low`/`medium`/`high`/`xhigh`/`max`) into the upstream model's effort
  vocabulary. Has `default` (all models) and per-model `models` maps; a
  `null` value omits the upstream `reasoning` object.
- `reasoning.thinking_disabled` translates Claude Code's
  `thinking: {"type": "disabled"}` into the upstream no-reasoning
  representation (e.g. `reasoning.effort = "none"`). Disabled thinking
  takes precedence over `output_config.effort` and the fixed config.
- The `model_aliases` object maps inbound model names to upstream model names.
- `model_aliases.default_model` is an optional fallback if the upstream rejects an alias or passthrough model. The proxy recognises a model rejection from the upstream's error body — a message that names the model as the subject of an absence ("model \"claude-sonnet-5\" not found"), or a `model_not_found` / `model_not_supported` code — and retries once, before any bytes reach the client. Unrelated rejections — a tool the upstream does not support, a bad request, a schema error — are not retried, including ones that merely mention the model in passing.
- The `prompt_caching` object lets you enable translation of Anthropic `cache_control: {type: "ephemeral"}` into OpenAI `prompt_cache_breakpoint` markers.
- `prompt_caching.models` defaults to `[]`; when empty, no OpenAI-specific cache fields are emitted, keeping non-OpenAI upstreams unaffected.
- `prompt_caching.cache_key` is optional and forwarded as `prompt_cache_key`.
- The `advisor` object enables the server-side advisor: a second, cheaper
  model is consulted mid-turn and its advice is fed back to the executor. The
  feature is **off by default** (`enabled` defaults to `false`), and when it
  is off the request path is byte-for-byte unchanged.
- **`advisor.max_uses`, `advisor.max_tokens` and `advisor.timeout_secs` are
  removed by setting them to `0`.** A **`0` means "no limit"**; an
  **absent key keeps the shipped default** (`max_uses` 2, `max_tokens` 2048,
  `timeout_secs` 45). The same rule applies to the `ADVISOR_MAX_USES`,
  `ADVISOR_MAX_TOKENS` and `ADVISOR_TIMEOUT_SECS` env vars.

  ```json
  "advisor": {
    "enabled": true,
    "model": "beam",
    "max_uses": 0,
    "max_tokens": 0,
    "timeout_secs": 0
  }
  ```

  "No limit" differs per field: `max_uses: 0` removes the consultation
  loop's only backstop (each iteration costs two upstream calls);
  `max_tokens: 0` means the *upstream's* default answer cap applies rather
  than the proxy's; `timeout_secs: 0` means no per-request override, so the
  global `request_timeout_secs` still bounds the call. An invalid limit
  (negative, fractional, too large) is a warning, not a startup failure —
  the proxy starts and falls back to the default. Full detail in
  [`proxy.json-guide.md`](./proxy.json-guide.md#advisor-advisor).
- `advisor.model` is an **explicit upstream model name**, not an inbound
  alias; `model_aliases.map` is not applied to it.
- `tool_result_images` controls how `image` blocks inside a `tool_result`
  are forwarded upstream (`array`, `lift` or `drop`; default `array`). See
  [`proxy.json-guide.md`](./proxy.json-guide.md#tool-result-images-tool_result_images).

A small example of these in `proxy.json`:

```json
{
  "upstream_base_url": "https://api.example.com",
  "upstream_api_key":  "sk-...",
  "reasoning": {
    "default": "none",
    "models": {
      "gpt-5.4-mini": "high",
      "gpt-5.6-luna":  "medium"
    },
    "effort_map": {
      "default": {
        "low": "none",
        "medium": "low",
        "high": "medium",
        "xhigh": "high",
        "max": "high"
      }
    },
    "thinking_disabled": {
      "default": { "disabled": "none" }
    }
  },
  "model_aliases": {
    "default_model": "gpt-4o-mini",
    "map": {
      "claude-sonnet-5": "gpt-5.4-mini"
    }
  },
  "prompt_caching": {
    "models": ["gpt-5.6-luna", "gpt-5.6-terra"],
    "cache_key": "my-app"
  }
}
```

### Server-side `web_fetch`

Anthropic's `web_fetch` is a **server** tool: the client declares it in `tools[]`
with a versioned `type` (`web_fetch_20250910` and later) and **no**
`input_schema`, and the SERVER performs the HTTP GET and feeds the extracted
text back to the model. The upstream this proxy talks to is an OpenAI-compatible
Responses API, which has no `web_fetch` of its own — so the proxy is that server.

The implementation lives in `src/tools/web_fetch/`:

| module | what it owns |
|---|---|
| `mod.rs` | activation (`has_server_side_web_fetch`, `active`) and `prepare` (synthesize the function tool, force buffering) |
| `fetch.rs` | the outbound HTTP request: URL validation, SSRF guard, DNS pinning, redirect re-validation, streaming read with a byte cap |
| `extract.rs` | `dom_smoothie` (Readability) primary extraction, `fast_html2md` full-page fallback |
| `protocol.rs` | the pure wire builders (`server_tool_use`, `web_fetch_tool_result`, the error result) |
| `session.rs` | the executor loop: detect the call, fetch, inject the result, re-run |

**The loop.** `prepare` forces `stream: false` on the outbound request, because a
fetch means the executor MUST be re-run with the page text before the turn can
end. The chain is buffered and, for a streaming client, converted to a single
synthetic Anthropic SSE sequence (the same `advisor::sse::message_to_events`
adapter the advisor uses). `src/stream.rs` is untouched.

**Activation.** `web_fetch.enabled` (default `false`) AND an inbound tool named
`web_fetch` with no `input_schema`. A client function tool that happens to be
named `web_fetch` (with a schema) is an ordinary tool and is forwarded upstream
untouched.

**Failure.** A failed fetch never fails the client request: it becomes a
`web_fetch_tool_result` error block with an Anthropic error code
(`invalid_tool_input`, `url_not_allowed`, `url_not_accessible`, `unavailable`),
the executor is told the fetch failed, and the turn continues.

**SSRF.** The URL comes from a model, so the fetch layer treats it as hostile:
`http`/`https` only, no credentials in the URL, every resolved address checked
against the private/reserved ranges, the checked address pinned for the
connection (so DNS rebinding cannot slip a different address past the check),
and every redirect hop re-validated. There is deliberately **no** domain
allow-list and **no** URL-length limit.

**Not enforced by the proxy:** the `max_uses` the client declares — the proxy
fetches as many URLs as the executor asks for, and a turn that passes ten
fetches is logged with a warning rather than stopped.

> **Claude Code CLI does not send this tool.** It ships a *client-side*
> `WebFetch` tool (capitalised, with an `input_schema`) and does the HTTP GET
> itself; it never declares the server-side `web_fetch`. This feature is for
> Anthropic-API clients that DO declare the server tool — the Claude Agent SDK,
> direct Messages API calls, and gateways that add the server tool on the
> client's behalf. Claude Code's own `WebFetch` continues to work through the
> ordinary client-tool path and is unaffected by this feature.

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
- `cargo clippy --all-targets` is the lint gate. There are **5 known
  pre-existing warnings** (in the test modules); no new ones are allowed.
- `cargo fmt --all -- --check` checks formatting.

### Test layout (project standard)

**Every unit-test module lives in its own file.** The source file declares the
module; the body lives next door:

```text
src/translate.rs          ← the code
src/translate/tests.rs    ← the unit tests
```

```rust
// end of src/translate.rs
#[cfg(test)]
mod tests;
```

- A module `NAME` declared in `src/thing.rs` or `src/thing/mod.rs` lives in
  `src/thing/NAME.rs` — Rust's own module layout.
- A module declared in a crate root (`src/lib.rs`, `src/main.rs`) lives in
  `src/NAME.rs`.
- The test module is still a **child** module, so `use super::*;` works and
  private items stay testable. That is why unit tests do **not** go in
  `tests/` (those are separate crates and can only see the public API).
- `tests/` holds integration tests only — separate crates, public API only
  (`tests/proxy_e2e.rs`). `tests/project_layout.rs` is a repo-hygiene test
  that checks this convention.

There is no "is the file big enough to split?" judgement: the rule is uniform,
so the layout is predictable, `git blame` points at the test file rather than
a 7,000-line source file, and a normal build never parses the test code.
`tests/project_layout.rs` enforces it — an inline `#[cfg(test)] mod X { … }`
fails the test run.

## Translation overview

### Request: Anthropic → OpenAI Responses

- `system` becomes the first system message.
- User text becomes user messages.
- `tool_result` blocks become `function_call_output` input items. Text-only
  results keep the plain-string `output`; results carrying `image` blocks are
  forwarded as an array of `input_text`/`input_image` parts by default
  (see `tool_result_images` in
  [proxy.json-guide.md](./proxy.json-guide.md#tool-result-images-tool_result_images)).
- Assistant text becomes `output_text`, and `tool_use` blocks become
  `function_call` items emitted after the assistant message.
- `tool_use.input` is serialized to JSON for OpenAI `arguments`.
- `tools[]` is mapped through with JSON Schema parameters preserved.
- `tool_choice`, `temperature`, `top_p`, and `max_tokens` are forwarded.
- `stop_sequences` are **not** forwarded — the Responses API has no `stop` field. The proxy logs a warning when it drops them.
- Assistant `thinking` blocks (extended thinking) are replayed upstream according to `[reasoning_replay]`: by default the reasoning text is folded into the assistant message, so the model keeps its memory of its own prior reasoning. See [proxy.json-guide.md](./proxy.json-guide.md#reasoning-replay).
- `stream: true` also enables usage reporting from the upstream.
- `cache_control` on user text and image blocks is translated to OpenAI `prompt_cache_breakpoint` when `[prompt_caching]` is enabled (an `ephemeral` marker on an image inside a `tool_result` is honoured too).
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

The proxy implements the core Anthropic Messages API path for Claude Code: streaming,
client tool use, stop-reason/usage remapping, model aliases, reasoning-effort
mapping, extended-thinking replay, prompt caching (partial), the experimental
advisor tool, and the server-side `web_fetch` tool. The following Claude Code /
Anthropic API capabilities are **not** supported because they depend on
Anthropic-specific behaviour the proxy cannot recreate against an
OpenAI-compatible upstream.

See also [`README.md`](./README.md#features) for the shorter visitor-facing list.

### Endpoints and transport
- `/v1/models` listing (and other Anthropic endpoints such as batches/files)
- TLS termination (bind a plain HTTP listener; put a reverse proxy in front)
- One upstream target per instance: no multi-tenant or per-user routing

### Vision / image inputs
- Images in *user* messages and inside `tool_result` blocks are translated to OpenAI `input_image` parts (base64 data URIs or passthrough URLs); a vision-capable upstream is still required. `tool_result` images are sent in the `function_call_output` array form by default — see [`proxy.json-guide.md`](./proxy.json-guide.md#tool-result-images-tool_result_images).
- `image` blocks in *assistant* turns are dropped with a warning; the Responses API has no assistant-image content part to map them to.
- No image output / vision generation handling (text-only responses from the upstream)

### Extended thinking
- `redacted_thinking` blobs are dropped (cannot be re-encoded for a non-Anthropic upstream)
- `thinking.budget_tokens` is parsed but not enforced or forwarded

### Server-side / built-in tools
- Server-side tools beyond OpenAI's `web_search` and the proxy's own `web_fetch` (no `code_execution`, `tool_search`, etc.)
- Server-tool continuation loops (`pause_turn` re-sends, mixed server/client turns)
- Claude Code's standby advisor path (only an explicitly configured `advisorModel` works behind a proxy)

### Request parameters and caching
- `stop_sequences` are dropped (Responses API has no stop field)
- `top_k` is parsed but never forwarded; `metadata` carries only `user_id`
- Tool-level, system-level, and assistant-level `cache_control` markers are not translated (user text/image blocks only, and only for allowlisted models)

### Beta / protocol features
- Anthropic beta headers are dropped, so beta-gated features cannot rely on them
- Anthropic-specific server-side tool envelopes and iteration billing beyond what the proxy synthesises

> Extended thinking blocks **are** supported on the response side (the proxy
> generates them from the upstream's `reasoning` output item) and are replayed
> back upstream on the next request - see
> [Reasoning replay](./proxy.json-guide.md#reasoning-replay). What is *not*
> supported is the Anthropic-specific `redacted_thinking` blob, which cannot be
> re-encoded for a non-Anthropic upstream and is dropped with a warning.

## Suggested next work (not committed)

This is a planning note for future changes. It is separate from the current
"out of scope" list above, which describes hard limits of the design.

> **Done:** the server-side `web_fetch` tool (image input support landed
> earlier) — see "Server-side `web_fetch`" above.

### Best next improvement: `/v1/models`

Small, self-contained route that improves compatibility with tools and model
selectors. (Image input support has since landed: user-message images were
already forwarded, and `tool_result` images now reach the upstream too — see
[`proxy.json-guide.md`](./proxy.json-guide.md#tool-result-images-tool_result_images).)
It does not unlock a new user workflow on its own.

### De-prioritise for now
- TLS termination: better handled by a reverse proxy (Caddy/nginx/Traefik).
- Server-side tools (`web_fetch`, `code_execution`, etc.): high value, but
  require upstream loops and are not cheap.
- `redacted_thinking`, `stop_sequences`: need upstream semantics OpenAI-compatible
  APIs mostly do not expose.

## License

This is a personal/professional project; no license is granted by default. Add one if you intend to distribute.
