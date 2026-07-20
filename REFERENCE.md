# Reference

Detailed config, script, and protocol notes that were trimmed from the main README.

## Features

- `POST /v1/messages` translation from Anthropic Messages to OpenAI Responses.
- Streaming via SSE end to end.
- Tool-use round trips: `tool_use` ↔ `tool_calls`, `tool_result` ↔ `tool` messages.
- Stop reason mapping: `stop` → `end_turn`, `length` → `max_tokens`, `tool_calls` → `tool_use`, `content_filter` → `end_turn`.
- Error translation from OpenAI `{error: {...}}` envelopes and HTTP statuses into Anthropic-shaped errors.
- Usage translation from OpenAI `usage` into Anthropic `usage`.
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

`proxy.toml` uses the same field names in snake_case. See `proxy.toml.example` for a starter.

### Advanced config

- `REASONING_EFFORT` sets the legacy default effort when no per-model override applies.
- The `[reasoning]` table lets you set a default effort plus per-model overrides.
- The `[model_aliases]` table maps inbound model names to upstream model names.
- `model_aliases.default_model` is an optional fallback if the upstream rejects an alias or passthrough model.

## Launching Claude Code

- `scripts/start-claude-code.sh` is the Linux/macOS launcher.
- `scripts/start-claude-code.ps1` is the Windows launcher.
- Both scripts set `ANTHROPIC_BASE_URL` to the proxy, set `ANTHROPIC_API_KEY=any`, and scrub leaked Anthropic/Claude Code env vars so the child stays routed through the proxy.
- Both default to `http://localhost:8085` and pass `--setting-sources=project,local` unless you override it.

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
- Prompt caching
- Server-side tools (`web_search`, `web_fetch`, `code_execution`)
- `/v1/models` listing
- TLS termination

## License

This is a personal/professional project; no license is granted by default. Add one if you intend to distribute.
