# Anthropic API gaps: fixes to work through

Source: three read-only research passes over `src/` (request side, response
side, endpoints and tools). Nothing was run against a live upstream.

**Before you start any item:**

- Line references come from the research and may have drifted. Re-check each
  one in the code first.
- Anything marked *unverified* is a hypothesis. Confirm it before changing code.
- The Anthropic-side facts in the endpoint pass were not checked against the
  live docs.
- Run the gate for each fix: `cargo fmt --all -- --check`,
  `cargo clippy --all-targets`, `cargo test`.
- Unit tests go in the sibling `tests.rs` file for the module (see `CLAUDE.md`).

Severity is for Claude Code usage. "Inherent" means OpenAI's API cannot do it,
so the proxy can only approximate it.

---

## A. Quick wins (small, low risk)

- [x] **A1. Fix the stale module comment**
  - Where: `src/anthropic.rs:24-26`
  - Problem: it says out-of-scope fields "will produce a serde deserialization
    error". Unknown request fields are in fact silently ignored (no
    `deny_unknown_fields`).
  - Fix: reword the comment to match the behaviour.

- [x] **A2. Reconcile `pause_turn` docs**
  - Where: `REFERENCE.md` (~line 344 vs ~lines 134-153)
  - Problem: the out-of-scope list says server-tool continuation is
    unsupported, but a `server_tools.pause_turn` option is documented.
  - Fix: check the code, then make the docs agree.

- [x] **A3. Request a reasoning summary**
  - Where: `src/translate.rs:340-343` (`ReasoningConfig::default()`)
  - Problem: `reasoning.summary` is never requested, so thinking text depends on
    the upstream's default.
  - Fix: set `summary` (for example `auto`) when thinking is enabled. Consider
    making it configurable.
  - Severity: medium. Check upstream compatibility, because some
    OpenAI-compatible servers may reject the field.

- [x] **A4. Match server tools by schema, not name only**
  - Where: `src/translate.rs:276` (`web_search`) and `:208-210` (`advisor`)
  - Problem: a custom tool named `web_search` that has an `input_schema` is
    replaced by the built-in search.
  - Fix: use `Tool::is_server_tool` (`src/anthropic.rs:480`), which checks the
    schema.

- [x] **A5. Honour the client's `strict` flag**
  - Where: `src/translate.rs:289`, strict budget at `:295-297` and `:454`
  - Problem: every function tool is forced to `strict: true`, even when the
    client sent `strict: false`.
  - Fix: read `strict` from the tool definition and respect it. Keep the strict
    budget downgrade for tools that asked for strict.

- [x] **A6. Reject or clamp `max_tokens: 0`**
  - Where: `src/translate.rs:347`
  - Problem: `0` is forwarded as `max_output_tokens: 0`, which the upstream will
    probably reject (*unverified*).
  - Fix: decide on the behaviour (clear 400, or a minimal cache-warm response).

- [ ] **A7. Send `parallel_tool_calls` without `tool_choice`**
  - Where: `src/translate.rs:299-308`, `:2657-2700`
  - Problem: it is only sent when `tool_choice` is present. `disable_parallel_tool_use`
    is ignored when the choice is `none`.
  - Severity: low.

---

## B. Response correctness (highest value)

- [x] **B1. Non-streaming failed response returns HTTP 200**
  - Where: `src/translate.rs:2742-2749`, `:2835`
  - Problem: a `status: "failed"` response becomes a `[error] …` text block with
    `end_turn`. Claude Code never retries and never sees an error. The
    streaming path already does this properly.
  - Fix: return an `AppError`. Map `error.code` the same way streaming does
    (`stream_error_kind`).
  - Severity: medium.

- [x] **B2. Truncated tool call at `max_tokens` is sent as `{}`**
  - Where: `src/repair.rs` (`read_arguments`), `src/stream.rs` (`repair_arguments`),
    `src/translate.rs` (`build_content_blocks`), `src/proxy.rs` (corrective retry)
  - Problem: when the response is `incomplete` for `max_output_tokens` and the
    arguments JSON is cut off, the proxy sent `tool_use` with input `{}` — a
    shape that looks like a valid empty call.
  - Fix: recover the parameters that were *finished* and drop the one that was
    still being written, then ship them with `stop_reason: max_tokens`.
  - Note: the original premise ("Anthropic sends partial JSON") is wrong for
    Anthropic's standard mode — it buffers each parameter and drops the
    incomplete one, so the completed parameters *are* shipped. Sending raw
    partial JSON would also violate `tool_use.input: map[unknown]` and make the
    wire result depend on which SDK partial parser the client uses.
  - Severity: medium.

- [ ] **B3. Produce `refusal` stop reason**
  - Where: `src/stream.rs:1156-1172`, `src/translate.rs:2830-2833`
  - Problem: `incomplete_details.reason = content_filter` becomes `end_turn`.
    `StopReason::Refusal` exists but is never produced.
  - Fix: map the content-filter reason to `Refusal`.

- [ ] **B4. Make refusal text consistent**
  - Where: `src/translate.rs:2968-2975` vs `src/stream.rs:317-327`
  - Problem: non-streaming prefixes `[refusal] `, streaming sends raw text.
  - Fix: pick one behaviour. Do this together with B3.

- [ ] **B5. Send `ping` events on the plain streaming path**
  - Where: `src/proxy.rs:1425-1569` (only `server_loop/stream.rs:224` pings)
  - Problem: a reasoning model that goes quiet sends nothing, so an intermediary
    may drop the connection.
  - Fix: interleave a timed `ping` in `TranslatorStream`.
  - Unverified: Claude Code's idle timeout.
  - Severity: medium.

- [ ] **B6. Don't send `message_stop` after an `error` event**
  - Where: `src/stream.rs:383`, `:404`, `:489`, `src/proxy.rs:1503`
  - Problem: Anthropic's docs show `error` as the terminal event.
  - Note: existing tests may assume the current behaviour.
  - Severity: low.

- [ ] **B7. Terminal events can be dropped on a deserialise failure**
  - Where: `src/responses.rs:395-405`, `src/proxy.rs:1464-1475`
  - Problem: `model: String` and `output: Vec` have no `#[serde(default)]`. A
    terminal `response.created` / `completed` that lacks them is dropped with a
    warning, so the stream can look like it ended without a terminal event.
  - Fix: add defaults. Unverified against real upstreams.

---

## C. Thinking

- [ ] **C1. Request encrypted reasoning content**
  - Where: `src/responses.rs`, `src/translate.rs` (no `include` field found)
  - Problem: without `include: ["reasoning.encrypted_content"]` the thinking
    `signature` is probably always empty, and native replay only works on
    upstreams that return the blob unprompted.
  - Fix: request it when thinking replay mode is `native`. Consider `store`.
  - Unverified against a live upstream.

- [ ] **C2. Map `thinking.budget_tokens` to an effort bucket**
  - Where: `src/proxy.rs:535-541`, `src/config.rs:1437-1494`
  - Problem: only `type: disabled` and `output_config.effort` affect behaviour.
    The budget is parsed but ignored, and `display` is not read.
  - Fix: map budget ranges to effort, and honour `display: omitted`.
  - Real budget enforcement is inherent.

- [ ] **C3. Model more reasoning events**
  - Where: `src/responses.rs:621-724`, `src/translate.rs:3041-3046`
  - Problem: `response.reasoning_text.delta` and `reasoning_summary_part.added`
    are dropped as `Unknown`. Multiple summary parts are joined with no
    separator.
  - Severity: low.

---

## D. Request features that are silently dropped

- [ ] **D1. Structured outputs (`output_config.format` / `output_format`)**
  - Where: `src/anthropic.rs:91-94` (`OutputConfig` has only `effort`),
    `src/translate.rs:344` (`text` always `None`), `src/responses.rs:292`
  - Fix: map to the Responses `text.format` json_schema.
  - Severity: low for the interactive CLI, high for `--json-schema` and SDK use.

- [ ] **D2. Warn when beta-only fields are dropped**
  - Where: `src/anthropic.rs:38-86`
  - Problem: `context_management`, `mcp_servers`, `container`, `service_tier`,
    `speed`, `inference_geo`, `diagnostics`, `cache_control` (top level) and the
    `anthropic-beta` header are ignored with no log.
  - Fix: log once per field, or declare them and warn.

- [ ] **D3. Schema-less Anthropic tools are dropped without an error**
  - Where: `src/translate.rs:277-282` (the `?` in the `filter_map`)
  - Affects: `bash_*`, `text_editor_*`, `memory_*`, `computer_*`,
    `code_execution_*`, `tool_search_*`, `mcp_toolset`, `browser_toolset`.
  - Fix, minimum: log a warning. Better: synthesise the known schemas for
    `bash`, `text_editor` and `memory` (client-executed, so safe).
  - Low for Claude Code, medium for SDK users. `code_execution` and
    `tool_search` are inherent.
  - Unverified: what happens when `tool_choice` names a dropped tool.

- [ ] **D4. Tool-definition extras**
  - Where: `src/anthropic.rs:424-439`
  - Problem: `input_examples`, `defer_loading`, `allowed_callers`,
    `eager_input_streaming` are ignored.
  - Fix: fold `input_examples` into the description. `defer_loading` is
    inherent without a tool-search loop.

- [ ] **D5. `search_result` blocks are lost**
  - Where: `src/translate.rs:1667`, `:2453-2458`, `:1963-2062`
  - Problem: top-level `search_result` and `container_upload` are dropped, and
    a block inside `tool_result` with no `text` is dropped.
  - Fix: flatten a `search_result`'s text into the tool result.

- [ ] **D6. `stop_sequences`**
  - Where: `src/translate.rs:319-326`, `src/anthropic.rs:560`, `src/stream.rs:454`
  - Problem: dropped with a warning, so `stop_reason: stop_sequence` and the
    `stop_sequence` field are never set.
  - Fix: inherent upstream. A proxy-side scan that aborts the upstream stream
    is possible but awkward with streaming.
  - Severity: low (Claude Code rarely sets them).

- [ ] **D7. Native `web_search` options**
  - Where: `src/translate.rs:172-185`, `:2689-2694`
  - Problem: with the upstream's native search, `max_uses` and `blocked_domains`
    are not applied, and `tool_choice` naming `web_search` becomes `auto`.
  - The proxy-native search applies both.

---

## E. Models and endpoints

- [ ] **E1. Add `max_input_tokens`, `max_tokens` and `capabilities` to `/v1/models`**
  - Where: `src/anthropic.rs:814-819` (`AnthropicModelInfo`), `src/proxy.rs:240-247`
  - Problem: only type, id, display name and `created_at` are returned. Newer
    Claude Code may read the missing fields to size context (*unverified*, the
    live docs were not checked). Today the workaround is
    `CLAUDE_CODE_MAX_CONTEXT_TOKENS`.
  - Fix: add config-driven values per model. OpenAI's own `/v1/models` has no
    context-window or capability data.
  - Also: honour `after_id` / `before_id` / `limit`, and give alias entries a
    real `created_at` instead of epoch 0.
  - Severity: medium.

- [ ] **E2. Add `GET /v1/models/{id}`**
  - Where: routes in `src/proxy.rs:180-194`
  - Problem: returns 404. Build it from the list and aliases.

- [ ] **E3. Improve `count_tokens` accuracy**
  - Where: `src/count_tokens.rs:28-37`, `:69-76`
  - Problem: when the upstream has no `/responses/input_tokens`, it falls back to
    4 characters per token and a flat 1,600 tokens per image. The 10 s timeout
    is fixed.
  - Severity: low-medium. Partly inherent.

- [ ] **E4. Batches, Files, Skills, Admin, `/v1/complete`**
  - Claude Code does not use them. Batches and Files could map to OpenAI's
    equivalents. Skills and Admin are inherent. Likely leave out of scope.

---

## F. Errors and headers

- [ ] **F1. Add a `request-id` header and `request_id` in error bodies**
  - Where: `src/error.rs:216-226`, `:229-264`
  - Fix: generate a `req_…` id and attach it to every response and error.

- [ ] **F2. Fuller error-type mapping**
  - Where: `src/error.rs:92-105`
  - Problem: 402 is not `billing_error`, 409 is not `conflict_error`, and
    504 or a timeout is not `timeout_error`.

- [ ] **F3. Map rate-limit headers**
  - Where: `src/error.rs:249-262`
  - Problem: no `anthropic-ratelimit-*` headers. Only worth doing if the
    upstream sends `x-ratelimit-*`. The SDK works without them.

---

## G. Fidelity polish (low priority)

- [ ] **G1. Prompt caching coverage**
  - Where: `src/translate.rs:1550-1630`, `:363-372`, `:2777`;
    `src/anthropic.rs:393-396`, `:657-682`
  - Problem: markers apply only to user text, image and document blocks (and
    `tool_result` images), and only for allowlisted models. System, tool and
    assistant markers and `ttl` are ignored. The usage `cache_creation` object
    is not emitted; non-standard flat `cache_creation_input_tokens_5m/_1h`
    fields are sent instead, and `cache_creation_input_tokens_1h` is hard-coded
    to 0.
  - Mostly inherent, since OpenAI caches the prefix automatically.

- [ ] **G2. Citations**
  - Where: `src/stream.rs:331-343`, `:1028-1043`; `src/translate.rs:2945-2949`
  - Problem: no `citations_delta` or `citations` on text blocks. The synthetic
    `server_tool_use.input` is a fake `{"query":"Web search"}`.

- [ ] **G3. Model echo and id formats**
  - Where: `src/stream.rs:515-517`, `src/translate.rs:2809`
  - Problem: `message.model` is the upstream's name, not the requested one. Ids
    are `msg_resp_…` and tool ids are `call_…`, not `toolu_…`.
  - Unverified: whether any client logic keys on these.

- [ ] **G4. Parallel tool-call blocks overlap**
  - Where: `src/stream.rs:642-683`, `:943-960`
  - Problem: a later tool's `content_block_start` is sent while the earlier block
    is still open, and stops are batched at the end. Anthropic has one open
    block at a time. SDKs probably cope (*unverified* for Claude Code).

- [ ] **G5. Remaining content-block gaps**
  - `redacted_thinking` is dropped (inherent).
  - `image` blocks with `source.type: file` are dropped; `transformations` is
    ignored.
  - PDFs become a text note unless `forward_documents` is on; document
    `citations` are ignored.
  - `metadata` keeps only `user_id`, truncated to 64 characters.

- [ ] **G6. Streaming granularity with server tools**
  - Where: `REFERENCE.md:143-146`
  - Problem: with server tools active, a plain answer arrives in one burst.
    Non-trivial to fix.

---

## Not planned (inherent or out of scope)

`top_k`, real thinking-budget enforcement, `code_execution`, `tool_search`,
programmatic tool calling, the MCP connector (`mcp_servers`),
`context_management`, `container`, `model_context_window_exceeded`, and
`pause_turn` outside the opt-in. If one of these becomes a requirement, it needs
its own design rather than a quick fix.

## Suggested order

A1 to A7 first, then B1 to B3, then B5, C1, D1, E1, and the rest as needed.
