# openai-to-anthropic-proxy

A small Rust proxy that lets you point [Claude Code](https://docs.claude.com/en/docs/claude-code/overview) (which speaks the Anthropic Messages API natively) at any OpenAI-compatible LLM endpoint. It accepts Anthropic-format requests on its inbound side, translates them to OpenAI Chat Completions, and translates the response (or stream) back into Anthropic shape.

> **About the name.** The package is named `openai-to-anthropic-proxy`, but the translation direction is actually **Anthropic → OpenAI** (Claude Code is the client, OpenAI-compatible upstream is the server). The name was kept for continuity; the README reflects the real direction.

## Features

- `POST /v1/messages` — full Anthropic Messages request → OpenAI Chat Completions request translation.
- **Streaming** via SSE, end-to-end. The proxy speaks Anthropic SSE on the client side and OpenAI SSE on the upstream side, with a state machine that handles content blocks, tool calls, and stop reasons.
- **Tool use** round-trips: Anthropic `tool_use` blocks become OpenAI `tool_calls`; Anthropic `tool_result` blocks become OpenAI `tool` messages.
- **Stop reason mapping**: `stop`→`end_turn`, `length`→`max_tokens`, `tool_calls`→`tool_use`, `content_filter`→`end_turn`.
- **Error translation**: OpenAI `{error: {...}}` envelopes and HTTP statuses are mapped to Anthropic-shaped errors and returned to Claude Code.
- Token usage from OpenAI's `usage` chunk is mapped to Anthropic's `usage` (`input_tokens`, `output_tokens`).
- `GET /healthz` for liveness checks.

## Out of scope (follow-ups)

- Vision / image inputs
- Extended thinking blocks
- Prompt caching
- Server-side tools (`web_search`, `web_fetch`, `code_execution`)
- `/v1/models` listing
- TLS termination (run behind a reverse proxy or use plain HTTP on localhost)

## Configuration

The proxy reads configuration from environment variables first, then from an optional `proxy.toml` in the working directory. **Environment variables win on conflict.**

| Variable | Default | Required? | Notes |
|---|---|---|---|
| `UPSTREAM_BASE_URL` | — | yes | e.g. `https://api.openai.com` |
| `UPSTREAM_API_KEY` | — | yes | bearer token for the upstream |
| `LISTEN_ADDR` | `0.0.0.0:8085` | no | socket address to bind |
| `UPSTREAM_PATH` | `/v1/chat/completions` | no | appended to the base URL |
| `REQUEST_TIMEOUT_SECS` | `600` | no | per-request timeout (matches Anthropic's default) |

`proxy.toml` is a TOML file with the same field names (snake_case). See `proxy.toml.example` for a starter.

## Build & run

```bash
cargo build --release

UPSTREAM_BASE_URL="https://api.openai.com" \
UPSTREAM_API_KEY="sk-..." \
./target/release/openai-to-anthropic-proxy
```

## Use with Claude Code

Point Claude Code at the proxy by setting `ANTHROPIC_BASE_URL` to the proxy's address. The `ANTHROPIC_API_KEY` is sent to the proxy but the proxy ignores it (use `UPSTREAM_API_KEY` for the real upstream credentials):

```bash
ANTHROPIC_BASE_URL=http://localhost:8085 \
ANTHROPIC_API_KEY=any \
claude
```

The proxy accepts whatever `ANTHROPIC_API_KEY` you set, then forwards `Authorization: Bearer $UPSTREAM_API_KEY` to the upstream.

## Development

```bash
cargo test                 # run all tests (unit + e2e)
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
```

The project uses a strict Clippy config (see `Cargo.toml` `[lints.clippy]` and `clippy.toml`):

- `correctness` and `suspicious` lint groups are denied outright.
- `complexity`, `perf`, `style`, and `nursery` are warned.
- `unsafe_derive_deserialize` is denied.
- Cognitive complexity threshold is 25; functions stay under that.
- Args-per-function threshold is 7; pass an input struct when a function grows.

New code must pass `cargo clippy --all-targets --all-features -- -D warnings`.

## How the translation works

### Request (Anthropic → OpenAI)

- `system` (string or text blocks) → first message with `role: "system"`.
- `messages[]`:
  - user text → `user` message.
  - user text + `tool_result` blocks → one `user` message per text run, one `tool` message per result, preserving order.
  - assistant text + `tool_use` blocks → `assistant` message with `content` and `tool_calls[]`.
  - `tool_use.input` is serialized to a JSON string for OpenAI's `arguments` field.
- `tools[]` → `tools[]` with `parameters` lifted from Anthropic's `input_schema` (already JSON Schema 2020-12).
- `tool_choice`:
  - `auto` → `"auto"`, `any` → `"required"`, `none` → `"none"`, `tool{name}` → `{type:"function", function:{name}}`.
- `temperature`, `top_p`, `max_tokens`, `stop_sequences` map 1:1.
- `stream: true` also sets `stream_options.include_usage: true` so the upstream sends a terminal usage chunk.
- `metadata.user_id` → `user` field.
- Out-of-scope fields (vision, thinking, cache_control, server tools) are dropped with a `tracing::warn!`.

### Response (OpenAI → Anthropic, non-streaming)

- `id` is prefixed with `msg_` if it doesn't already.
- `role` is always `"assistant"`.
- `content`: text → `text` block; each `tool_call` → `tool_use` block (text first, then tools).
- `stop_reason`: `stop`→`end_turn`, `length`→`max_tokens`, `tool_calls`→`tool_use`, `content_filter`→`end_turn`.
- `usage`: `input_tokens = prompt_tokens`, `output_tokens = completion_tokens`. Cache fields are 0.

### Response (OpenAI SSE → Anthropic SSE, streaming)

The proxy runs a `StreamTranslator` state machine. The state per in-flight request:

- Text block: open/closed + Anthropic content index.
- Tool blocks: per OpenAI tool-call index, the open `tool_use` block's id, name, accumulated argument string, and Anthropic content index.
- Stop reason and usage, set on the terminal chunk.

For each OpenAI chunk:

- First non-empty chunk → `message_start` (with a synthetic `msg_` id).
- Text delta → `content_block_start` (text) once, then `content_block_delta {text_delta}` per chunk.
- Tool call delta (with id+name) → `content_block_start` (tool_use) at the next free index (closing the text block if open), then `content_block_delta {input_json_delta}` per arguments fragment.
- `finish_reason` → `content_block_stop` for any open blocks, then `message_delta` with the mapped `stop_reason` and the `usage` (from the terminal usage chunk if present).
- After upstream sends `data: [DONE]`, the translator emits `message_stop` to close the stream.

The proxy never emits a `data: [DONE]` line; Claude Code doesn't expect one in Anthropic SSE.

## Architecture

```
Claude Code ──HTTP/SSE──▶ axum :8085
                          │
                          ▼
                  ┌───────────────────┐
                  │ translate::request│  Anthropic CreateMessageRequest
                  │  (Anthropic→OAI)  │  ───────────────────────────▶
                  └───────────────────┘           OpenAI ChatCompletionRequest
                          │                              │
                          │                              ▼
                          │                       reqwest ──HTTPS──▶ upstream
                          │                              │
                          │                              ▼
                          │                       OpenAI ChatCompletionResponse
                          │                              │ (or SSE chunk stream)
                          │                              ▼
                          │           ┌────────────────────────────────┐
                          │           │ translate::response or ::stream│ OpenAI → Anthropic
                          │           └────────────────────────────────┘
                          ▼                              │
                  ┌───────────────────┐                  │
                  │ axum response     │ ◀────────────────┘
                  └───────────────────┘
                  Anthropic Message or Anthropic SSE
                          │
                          ▼
                       Claude Code
```

## License

This is a personal/professional project; no license is granted by default. Add one if you intend to distribute.
