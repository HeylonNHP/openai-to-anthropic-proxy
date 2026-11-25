# Reference

Protocol, architecture and behaviour notes that were trimmed from the main
README. Configuration lives in [`CONFIGURATION.md`](./CONFIGURATION.md) and
[`proxy.json-guide.md`](./proxy.json-guide.md).

## Features

- `POST /v1/messages` translation from Anthropic Messages to OpenAI Responses.
- Streaming via SSE end to end.
- Tool-use round trips: `tool_use` ↔ `tool_calls`, `tool_result` ↔ `tool` messages.
- Stop reason mapping: `stop` → `end_turn`, `length` → `max_tokens`, `tool_calls` → `tool_use`, `content_filter` → `refusal`.
- Error translation from OpenAI `{error: {...}}` envelopes and HTTP statuses into Anthropic-shaped errors.
- Usage translation from OpenAI `usage` into Anthropic `usage`.
- Optional prompt-caching translation from Anthropic `cache_control` to OpenAI `prompt_cache_breakpoint`.
- The server-side `web_fetch` tool, implemented by the proxy: it fetches the URL itself, extracts the readable text, and returns it as an Anthropic `web_fetch_tool_result` (see "Server-side `web_fetch`" below).
- Optional proxy-native `web_search` (DuckDuckGo scraping + page fetching + compaction) as a swap-in for the upstream's built-in search — see "Proxy-native `web_search`" below.
- `POST /v1/messages/count_tokens`, answered from the upstream's own token-counting endpoint when it has one, else a local estimate (see "Token counting" below).
- `GET /healthz` for liveness checks.

## Configuration

Every configuration option - the `proxy.json` fields, the environment variable
that overrides each one, and the Claude Code settings that point Claude Code at
the proxy - is documented in [`CONFIGURATION.md`](./CONFIGURATION.md).

The deep dives (edge cases, invalid-value handling, per-tool semantics) are in
[`proxy.json-guide.md`](./proxy.json-guide.md).

The rules that never change:

- **Precedence.** Environment variables, then `proxy.json`, then built-in
  defaults. An env var is the field path in upper snake case -
  `advisor.max_uses` is `ADVISOR_MAX_USES`.
- **The file is strict JSON** - no comments, no trailing commas. Unknown fields
  are rejected at startup with a list of accepted names, so a typo fails fast
  rather than being silently ignored. See `proxy.json.example` for a starter.
- **Field names are lower-case snake_case.** Pasting an env var name such as
  `UPSTREAM_BASE_URL` as a JSON key makes the proxy reject the file.
- **An invalid value is a warning, not a startup failure.** Negative,
  fractional, zero or out-of-range limits fall back to the shipped default and
  the proxy still starts.
- **`0` means "no limit"** for the limit-style options. An **absent** key keeps
  the shipped default, which is usually not "no limit".

## Server-side `web_fetch`

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
| `tool.rs` | the `ServerTool` the shared loop calls: fetch the URL, return the page text |

**The loop.** `prepare` forces `stream: false` on the outbound request, because a
fetch means the executor MUST be re-run with the page text before the turn can
end. The re-run is done by the shared server-tool loop (see
[Server tools together](#server-tools-together-the-shared-loop)); this module
only supplies the `ServerTool` that fetches a page.

**Activation.** `web_fetch.enabled` (default `false`) AND an inbound tool named
`web_fetch` with no `input_schema`. A client function tool that happens to be
named `web_fetch` (with a schema) is an ordinary tool and is forwarded upstream
untouched.

**Failure.** A failed fetch never fails the client request: it becomes a
`web_fetch_tool_result` error block with an Anthropic error code
(`invalid_tool_input`, `url_not_allowed`, `url_not_accessible`, `max_uses_exceeded`,
`unavailable`),
the executor is told the fetch failed, and the turn continues.

**SSRF.** The URL comes from a model, so the fetch layer treats it as hostile:
`http`/`https` only, no credentials in the URL, every resolved address checked
against the private/reserved ranges, the checked address pinned for the
connection (so DNS rebinding cannot slip a different address past the check),
and every redirect hop re-validated. The operator sets no domain allow-list
and no URL-length limit; domain rules come only from the client's own tool
options (above).

**The client's options.** The options the client sets on its declaration are
applied:

| option | effect |
|---|---|
| `max_uses` | A fetch over the limit is refused (`max_uses_exceeded`) and not counted. With **no** `max_uses`, fetches are unlimited, as configured (the shared loop's round limit still applies). |
| `allowed_domains` / `blocked_domains` | A URL outside the rules is refused (`url_not_allowed`) before any network access, and every redirect hop is checked too. Setting both is a 400. |
| `max_content_tokens` | Tightens the extracted text to about 4 characters per token. It can only tighten the operator's `max_content_bytes`, never loosen it. |

A turn that passes ten fetches is still logged with a warning.

> **Claude Code CLI does not send this tool.** It ships a *client-side*
> `WebFetch` tool (capitalised, with an `input_schema`) and does the HTTP GET
> itself; it never declares the server-side `web_fetch`. This feature is for
> Anthropic-API clients that DO declare the server tool — the Claude Agent SDK,
> direct Messages API calls, and gateways that add the server tool on the
> client's behalf. Claude Code's own `WebFetch` continues to work through the
> ordinary client-tool path and is unaffected by this feature.

## Proxy-native `web_search`

By default the client's server-side `web_search` tool is mapped to the
upstream's built-in search. With `web_search.enabled`, the proxy answers it
**itself** instead: the executor calls an internal `proxy_web_search` function
tool with a list of queries; the proxy scrapes DuckDuckGo for each, fetches
every result page (through the same SSRF-hardened fetcher as `web_fetch`),
summarises or truncates long pages, and feeds the text back. The client sees the
usual `server_tool_use` / `web_search_tool_result` pairs.

Every limit (`max_queries`, `max_results_per_query`, the compaction threshold, the
timeouts, the page size) has a default **and** a ceiling; there is no unlimited
setting. A blocked search (CAPTCHA) is reported as an error in the tool result,
not silently routed to the upstream's search.

The client's tool options are applied (`max_uses` counts queries,
`allowed_domains` / `blocked_domains` narrow the search and are enforced on the
results, `user_location` picks the search region), and what the model read is
carried into later turns through each result's `encrypted_content`, so the model
still knows on the next turn what it found.

Full details — how it works, design decisions, known limitations, and how to
remove it — are in [`src/tools/web_search/README.md`](./src/tools/web_search/README.md).

## Server tools together: the shared loop

`web_search`, `web_fetch` and the advisor all run on one loop,
`src/tools/server_loop/`, so a request can declare any mix of them and the model
can use all of them in one turn, in any order. Details are in
[`src/tools/server_loop/README.md`](./src/tools/server_loop/README.md); in short:

- **Rounds.** One round is one executor run whose calls were served. A request
  runs at most `server_tools.max_rounds` rounds (default 10, like Anthropic's
  loop; `0` = no limit). At the limit the model is told and gets one last run to
  answer. With `server_tools.pause_turn` the response is returned as it stands
  with `stop_reason: "pause_turn"` instead, for a client that resumes (the
  Anthropic SDKs do; Claude Code does not, so it is off by default).
- **Streaming.** A streaming client is sent each round as it finishes, with
  `ping` events while a later run is in flight. A plain answer with no tool call
  is still delivered in one burst (each executor run is fetched whole).
- **Usage.** `usage.server_tool_use` reports `web_search_requests` and
  `web_fetch_requests` (a zero count is left out); the advisor's tokens are in
  `iterations[]`.
- **Failure.** A tool never fails the request. If the *first* executor run
  fails, the error is returned with its HTTP status; if a later run fails, the
  work already done is returned.

## Launching Claude Code

`scripts/start-claude-code.sh` (macOS/Linux) and `scripts/start-claude-code.ps1`
(Windows) start Claude Code against a running proxy. See
[CONFIGURATION.md](./CONFIGURATION.md#launch-scripts) for what they set and when
to use them.

## Context-window workaround

Claude Code decides when to auto-compact based on the model's perceived context window. It derives that window from Anthropic model names and the first-party Anthropic API, not from API responses. When the proxy rewrites `claude-sonnet-5` → `gpt-5.4-mini` (or any other alias), Claude Code does not know the upstream model's real context window and falls back to a hardcoded 200K. That causes premature auto-compaction for 1M-context models.

Tell Claude Code the real window with `CLAUDE_CODE_MAX_CONTEXT_TOKENS` (or the
`autoCompactWindow` setting) - see
[CONFIGURATION.md](./CONFIGURATION.md#claude-code-settings-claudesettingsjson).
Use the value your upstream model actually supports (e.g., `1000000` for
GPT-5.4-mini / GPT-5.6-luna, `200000` for older 200K models).

### Token counting

`POST /v1/messages/count_tokens` translates the request exactly as
`/v1/messages` would (same alias resolution, same tool and image handling) and
asks the upstream to count it at `{upstream_path}/input_tokens` (OpenAI's
`/v1/responses/input_tokens`). Only the input fields are sent: no `stream`,
`max_output_tokens`, `store`, `user`, `prompt_cache_key` or `reasoning`.

If the upstream has no such endpoint (404, 405 or 501, other than a "no such
model" 404), the proxy remembers that for the life of the process and
estimates locally from then on. Any other failure estimates for that one call,
including a count that takes longer than 10 seconds (a fixed limit, not
configurable, so a slow upstream never stalls the client).
The estimate is about 4 characters per token over the translated request
(tool schemas included), with each base64 image counted as a flat 1,600
tokens. A count never fails the client because of the upstream, and is not
added to the TUI token totals.

### Context-overflow errors

When the upstream rejects a prompt as too long for its context window, the
proxy answers HTTP 400 `invalid_request_error` with Anthropic's own wording,
`prompt is too long: <actual> tokens > <max> maximum` (just
`prompt is too long` when the upstream gave no numbers). Claude Code
recognises that message and compacts the conversation and retries; the
upstream's own wording would not be recognised. An overflow reported inside a
stream becomes an SSE `error` event with the same type and message. Detection
is conservative and lives in `src/context_overflow.rs`: output-limit,
image/file and token-per-minute errors are not treated as overflows, and an
overflow never triggers the `default_model` fallback or a parameter-strip
retry.

### Upstream errors, overload and rate limits

An upstream HTTP error keeps its status code and gets the Anthropic error
`type` for it, so Claude Code's own retry logic recognises it:

| Upstream status | `error.type` |
|---|---|
| 400 | `invalid_request_error` |
| 401 | `authentication_error` |
| 403 | `permission_error` |
| 404 | `not_found_error` |
| 413 | `request_too_large` |
| 429 | `rate_limit_error` |
| 503, 529 | `overloaded_error` |
| anything else | `api_error` |

The proxy's own connect failures (502) and timeouts (504) are `api_error`, not
overloaded. On a 429 or a 5xx the upstream's `Retry-After` (seconds or an HTTP
date) or `retry-after-ms` is passed on as both `retry-after` and
`retry-after-ms`, rounded up and capped at 60 s, the longest the Anthropic SDK
honours. The proxy never retries a 429 or 5xx itself; Claude Code does, and a
second layer of retries would stack the delays.

An error inside a stream cannot carry a status or header, only a type, taken
from the upstream's error code: `rate_limit_*` / `insufficient_quota` →
`rate_limit_error`; `*overloaded*` / `service_unavailable` →
`overloaded_error`; `invalid_api_key` → `authentication_error`; anything
unrecognised → `api_error`. A context overflow is still checked first and
keeps its own wording (see above). An `insufficient_quota` 429 is not
transient, but is still a `rate_limit_error`, as it is upstream.

## Development

- `cargo test` runs unit and e2e tests.
- `cargo clippy --all-targets` is the lint gate and must stay **warning-free**.
- `cargo fmt --all -- --check` checks formatting.
- CI (`.github/workflows/ci.yml`) runs all three on every push and pull request.

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
- `stop_reason` and token usage are mapped into Anthropic shape. `input_tokens` is reported **net of cache**: OpenAI's gross `input_tokens` minus `cached_tokens` and `cache_write_tokens`, which become `cache_read_input_tokens` and `cache_creation_input_tokens`. Claude Code adds the three, so reporting the gross figure would count cached tokens twice and trigger auto-compaction early. The proxy's own log lines and TUI still show the upstream's gross figure.
- A tool call the upstream cut short — it ran out of output tokens mid-JSON — keeps the parameters that were finished and drops the one that was still being written, the way Anthropic's own API buffers each parameter. The reply then carries `stop_reason: "max_tokens"`, which is the signal the client keys on to retry the turn with a bigger budget.

### Streaming

The stream translator keeps track of open text and tool-use blocks, emits the right Anthropic content-block events as OpenAI deltas arrive, then closes the message when the upstream finishes.

A quiet upstream would leave the client with nothing to hear, so a `ping`
event goes out every 10 seconds of silence — the same event Anthropic's own
API sends, and the same cadence the server-tool path uses. Claude Code counts
the bytes of a `ping` as activity: without them a reasoning model that goes
quiet for five minutes trips Claude Code's stream-idle watchdog, which aborts
the turn and retries the whole request. The keep-alive starts once
`message_start` has been sent (a `ping` before it would break the event
order), restarts whenever the client hears something else, and stops with the
message. It does not cover an upstream that has not sent its response headers
yet: the proxy's own headers go out only once the upstream's do, so that
window is a first-byte problem, not a keep-alive one.

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
- Anthropic endpoints other than Messages, `count_tokens`, `GET /v1/models` and `/healthz` (such as batches/files)
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
- Server-side tools beyond OpenAI's `web_search` and the proxy's own `web_search`, `web_fetch` and `advisor` (no `code_execution`, `tool_search`, etc.)
- Proxy-driven resumption of a paused server-tool turn. The proxy can end a turn with `stop_reason: "pause_turn"` (opt-in via `server_tools.pause_turn`, see [Server tools together](#server-tools-together-the-shared-loop)) and accepts the client's resend, but it never re-sends the conversation itself: resuming is the client's job, and Claude Code does not do it
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

> **Shipped since this section was written:** the server-side `web_fetch` tool,
> the proxy's own `web_search`, the experimental `advisor` tool and the
> `GET /v1/models` route — see "Server-side `web_fetch`" above and
> [Gateway model discovery](./README.md#gateway-model-discovery-v1models).

### Open

- **A binary `web_fetch` response is downloaded in full before it is
  recognised.** A body served as `application/octet-stream` (which servers
  also use for plain text) is classified `Unknown`, so the proxy reads the
  whole body and only then sniffs for a NUL byte to decide it is binary. With
  `web_fetch.max_bytes` unset — the default — that means an arbitrarily large
  file is fetched in full. Fixing it means sniffing the first bytes as they
  arrive and stopping early, rather than reading the body and then deciding.

Everything else is either a user-facing feature (see the "out of scope" list
above) or deployment work on the Pi, tracked in
[`ADVISOR_FAULTS.md`](./ADVISOR_FAULTS.md#still-open).

### De-prioritise for now
- TLS termination: better handled by a reverse proxy (Caddy/nginx/Traefik).
- Anthropic's remaining server-side tools (`code_execution`, `tool_search`, …):
  they need upstream loops the proxy does not have.
- `redacted_thinking`, `stop_sequences`: need upstream semantics OpenAI-compatible
  APIs mostly do not expose.

## License

GPL-3.0 — see [`LICENSE`](./LICENSE). You may use, modify and redistribute the
code; any derivative must also be distributed under GPL-3.0 with its source
available. Contributions are welcome as pull requests.
