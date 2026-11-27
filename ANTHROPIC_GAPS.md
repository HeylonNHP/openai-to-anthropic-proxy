# Anthropic API gaps: fixes to work through

Source: read-only research passes over `src/` (request side, response side,
endpoints and tools). A later pass re-checked the current code and the live
Anthropic schema for the entries added since, and checked a few of them
against the installed Claude Code client. Nothing was run against a live
upstream.

**Before you start any item:**

- Line references come from the research and may have drifted. Re-check each
  one in the code first.
- Anything marked *unverified* is a hypothesis. Confirm it before changing code.
- The Anthropic-side facts in the original three passes were not checked
  against the live docs. Entries added later (A8, A9, B8, B9, E5, E6, F4,
  G9, G10 and section H) were checked against the live schema; the ones
  that name a client impact were checked against the installed Claude Code
  CLI (v2.1.332), by reading its embedded JavaScript. That is a static
  reading of one client version, not a behavioural test — treat it as
  strong evidence, not proof.
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

- [ ] **A8. The `min_output_tokens` floor can exceed the client's `max_tokens`**
  - Where: `src/proxy.rs:921-931` (the floor is applied to the outbound
    request); `src/proxy.rs:1391-1397` (only `max_tokens == 0` is pre-warmed)
  - Problem: a client `max_tokens` of 1..15 is silently sent as 16 upstream,
    and the reply comes back unmodified — so `usage.output_tokens` can
    exceed the client's `max_tokens`, with `stop_reason: end_turn` when the
    model finishes naturally inside 16. Anthropic treats `max_tokens` as
    "the absolute maximum number of tokens to generate" and never returns
    more than it.
  - Client impact (read from Claude Code 2.1.332's embedded JS): Claude Code does
    send `max_tokens: 1` probes in at least five places (a `count_tokens`
    fallback, a `model_validation` probe, a quota probe), but every probe
    found reads only `usage.input_tokens` or the HTTP status — never the
    output. So this is an API-contract divergence and wasted generation
    (up to 15 extra tokens per probe), not a Claude Code break.
  - Note: `proxy.json-guide.md` already documents that "a probe may carry up
    to 16 tokens instead of 1", so the behaviour is visible to the operator.
  - Fix: inherent to an OpenAI-style upstream's floor. Optionally log the
    raise at `warn` when the client cap is 0 or 1 — the values Claude Code
    actually sends.
  - Severity: low-medium.

- [x] **A9. `max_tokens: 0` + `stream: true` is rejected — the rule is real**
  - Where: `src/proxy.rs` (the 400 in `handle_messages`)
  - The entry suspected the proxy 400s a documented-valid request, because
    the Messages parameter reference says `max_tokens` has `minimum: 0` and
    states no `stream` incompatibility. Settled 2026-11-27 against the live
    prompt-caching docs (two independent fetches, identical wording): "A
    `max_tokens: 0` request is rejected with an `invalid_request_error` if
    any of the following are set … `stream: true`". The rule lives on the
    prompt-caching page, not the parameter reference — which is why it
    looked missing.
  - Fix: none needed; the proxy's 400 is correct. The comment at the check
    now cites the doc instead of asserting it bare. (Doc reading, not a
    live API call; the docs are explicit enough to stand on.)
  - Severity: low (closed).

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

- [x] **B3. Produce `refusal` stop reason**
  - Where: `src/translate.rs` (`stop_reason_for_reply`, `reply_is_refusal`),
    `src/stream.rs` (the terminal capture)
  - Problem: `incomplete_details.reason = content_filter` becomes `end_turn`.
    `StopReason::Refusal` exists but is never produced.
  - Fix: map the content-filter reason to `Refusal`.
  - Also: the upstream's other refusal — a `refusal` content part on a
    `completed` reply — now produces `Refusal` too. A refusal outranks the
    tool-call upgrade, because Anthropic's clients must not run the tools
    of a refused turn.
  - Note: the `refusal` content part never deserialised before this: its
    serde tag was `output_refusal` where the upstream sends `refusal`, so
    every refusal fell into `Unknown` and was dropped.

- [x] **B4. Make refusal text consistent**
  - Where: `src/translate.rs` (`REFUSAL_PREFIX`), `src/stream.rs`
  - Problem: non-streaming prefixes `[refusal] `, streaming sends raw text.
  - Fix: both paths prefix the refusal text with `[refusal] `.

- [x] **B5. Send `ping` events on the plain streaming path**
  - Where: `src/proxy.rs` (`TranslatorStream`), `src/proxy/tests.rs`
  - Problem: a reasoning model that goes quiet sends nothing, so the
    connection goes silent.
  - Fix: interleave a timed `ping` in `TranslatorStream`.
  - Note: the gap file said "an intermediary may drop the connection"; it is
    in fact **Claude Code** that drops it. Its streaming watchdogs abort a
    turn after 300 s without bytes (the byte-level watchdog, which runs on a
    custom `ANTHROPIC_BASE_URL`), after 5 minutes without bytes (the body idle
    timeout), and after 300 s without a parsed event (the event-level
    watchdog, which arriving bytes — pings included — hold off for as long as
    they arrive at least every 10 s). The symptom is
    `API Error: Stream idle timeout`, and Claude Code retries the whole
    request, so the tokens already spent are paid for twice.
  - A `ping` cannot cover an upstream that never sends its response headers:
    the proxy only sends its own headers once the upstream's arrive, so that
    window is a first-byte problem, not a keep-alive one.
  - Severity: medium.

- [ ] **B6. Don't send `message_stop` after an `error` event**
  - Where: `src/stream.rs:383`, `:404`, `:489`, `src/proxy.rs:1494`
  - Problem: Anthropic's docs show `error` as the terminal event.
  - Note: existing tests may assume the current behaviour.
  - Severity: low.

- [x] **B7. Terminal events can be dropped on a deserialise failure**
  - Where: `src/responses.rs` (`ResponsesResponse`), drop sites
    `src/proxy.rs` ("dropping malformed upstream SSE event") and
    `src/tools/server_loop/live.rs`
  - Problem: `model: String` and `output: Vec` had no `#[serde(default)]`.
    A terminal `response.created` / `completed` that lacks them was dropped
    with a warning, so the stream looked like it ended without a terminal
    event: the client got a spurious `api_error` for a turn that actually
    succeeded, and the response's stats line was suppressed.
  - Fix: default both fields, tolerating an absent key **and** an explicit
    `null` (plain `#[serde(default)]` only covers absence — serde rejects
    `null` for `String`/`Vec`). `status` is deliberately left required: it
    decides the stop reason (`incomplete` → `max_tokens` / `refusal`), so
    an empty default would silently mislabel a truncated turn.
  - Note: verified against the code and serde semantics, not against a live
    upstream that actually omits these fields (none was found in the wild).
    Both drop sites (plain stream and server-tool loop) are covered by the
    shared struct fix.

- [x] **B8. The upstream gate's queue wait and 429 backoff sit outside every timeout**
  - Where: `src/upstream_gate.rs:80` (`acquire().await`), `:109`
    (`tokio::time::sleep(wait)`), `:88` (`request.send()`); the client-level
    timeout is at `src/main.rs:173`
  - Problem: both the semaphore wait and the exponential backoff sleep run
    BEFORE `request.send()` is invoked, so neither the client-level
    `request_timeout` nor any per-request `.timeout()` covers them. Queue
    wait is unbounded, and backoff can total minutes when the upstream sends
    a `Retry-After`. During all of it the proxy has sent the client zero
    bytes, so a queued or backing-off request could outlast Claude Code's
    byte watchdog and abort the turn — the pre-first-byte version of the
    failure B5 fixed mid-stream.
  - Fix: streaming requests now take the **early start** (shared module
    `src/early_start.rs`): the upstream work (queue, backoff, retries — all
    of it) runs in a spawned task, and a reply that takes longer than
    `START_AFTER` (10 s = one ping interval) gets HTTP 200 + a placeholder
    `message_start` + `ping`s every 10 s until the real reply splices in
    behind it (the spliced translator suppresses its own `message_start`;
    the real usage lands in the closing `message_delta`). The server-tool
    streaming path's existing placeholder shell was hoisted into the same
    module, so both paths share one keep-alive body.
  - The deadline doubles as the error channel, and that is load-bearing:
    a static reading of Claude Code 2.1.332 (decompiled, not
    behaviourally tested) shows an in-stream `error` event does **not**
    get the HTTP status' backoff treatment — `rate_limit_error` in-stream
    classifies as `badRequest` → one non-streaming fallback then fail,
    versus ~10 backoff retries for an HTTP 429. So a failure the upstream
    reports *within* the deadline stays a real HTTP status; only the
    long-wait tail (the B8 case, where the old behaviour was a turn abort
    and a full re-send) degrades to an in-stream error, after the gate has
    already retry-with-backoff'd internally.
  - Non-streaming requests are unchanged (no channel for mid-wait bytes);
    their queue/backoff silence remains, with `rate_limit_retries` as the
    operator's dial. Queue time still does not count against
    `request_timeout_secs` (documented in `proxy.json-guide.md`).
  - Severity: medium. Was the only unchecked entry that could end a turn.

- [ ] **B9. A streamed server-tool run gets no corrective schema retry**
  - Where: `src/tools/server_loop/README.md:191-193`
  - Problem: a buffered server-tool run that violates the tool's schema gets
    one corrective re-send after the arguments are repaired. A streamed run
    cannot: its bytes are already with the client.
  - Fix: inherent with streaming. Declare it and move on.
  - Severity: low.

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

- [x] **E1. Add `max_input_tokens`, `max_tokens` and `capabilities` to `/v1/models`**
  - Where: `src/anthropic.rs` (`AnthropicModelInfo`), `src/proxy.rs`
    (`handle_models`, `page_models`), `src/config.rs` (`model_info`)
  - Fix: a `model_info` config section (per id: `max_input_tokens`,
    `max_tokens`, `capabilities`, `description`, `created_at`; config-file
    only) feeds optional fields that are omitted when unset, as Anthropic's
    own responder does. Pagination is honoured: `limit` (default 20,
    1..=1000, else 400), exclusive `after_id` / `before_id`, page-boundary
    `first_id` / `last_id` with explicit `null` on an empty page. Alias rows
    inherit the release date of the upstream model they map to.
  - Note (decompiled Claude Code 2.1.330): the live consumer of this
    endpoint is gateway discovery, which reads `id`, `display_name` and
    `description` and strips the rest; the SDK paths that parse
    `max_input_tokens` / `max_tokens` exist but are hard-gated off in that
    build, and the context window never comes from `/v1/models` (catalog +
    settings + environment). So this is Anthropic-shape fidelity plus
    future-readiness — `description` is the one field the current build
    reads — not a change to auto-compact timing;
    `CLAUDE_CODE_MAX_CONTEXT_TOKENS` remains the dial. `capabilities` is not
    read from `/v1/models` by any client (it lives in the client's model
    catalog) but is part of Anthropic's documented model object, so it is
    passed through from config.
  - Severity: medium.

- [x] **E2. Add `GET /v1/models/{id}`**
  - Where: `src/proxy.rs` (`handle_model`, `collect_model_rows`,
    `find_model_row`), `src/error.rs` (`AppError::NotFound`)
  - Fix: the single-model route serves the same rows as the list
    (upstream mirror + alias/default entries + `model_info`) as a bare
    object — no envelope — behind the same `proxy_key` layer. A miss is
    the new `AppError::NotFound`: 404 with Anthropic's `not_found_error`
    envelope. Lookup is exact id first (alias keys are rows), then the
    raw upstream id behind a `prefix_models_for_discovery` rewrite.
  - Note (decompiled Claude Code 2.1.330): the retrieve path parses no
    schema at all — plain `JSON.parse`, bare object, JSON content-type
    load-bearing — reads `id` / `max_input_tokens` / `max_tokens`, sends
    no query string (the `?beta=true` route belongs to
    `client.beta.models`, which has zero callers), percent-decodes ids
    (axum's `Path<String>` does this), and treats 404 as status-only
    (`not_found` → cache null, no retry, no retry storm). Still
    hard-gated off (`NWe()` returns false) in this build, so this is
    future-readiness, like E1's fields.
  - Severity: low-medium.

- [ ] **E3. Improve `count_tokens` accuracy**
  - Where: `src/count_tokens.rs:28-37`, `:69-76`
  - Problem: when the upstream has no `/responses/input_tokens`, it falls back to
    4 characters per token and a flat 1,600 tokens per image. The 10 s timeout
    is fixed.
  - Severity: low-medium. Partly inherent.

- [ ] **E4. Batches, Files, Skills, Admin, `/v1/complete`**
  - Claude Code does not use them. Batches and Files could map to OpenAI's
    equivalents. Skills and Admin are inherent. Likely leave out of scope.

- [ ] **E5. `GET /v1/models/{model_id}` can answer with a different `id` than requested**
  - Where: `src/proxy.rs:403-410` (`find_model_row` falls back to
    `display_name`), used by `handle_model`
  - Problem: the second pass matches on `display_name` (for an upstream row
    `display_name` is the raw upstream name, while `id` may be a
    `prefix_models_for_discovery` `claude-` rewrite), so
    `GET /v1/models/<display-name>` returns a bare Model object whose `id` is
    not the requested path — a shape Anthropic never produces, and a case
    where Anthropic would 404. The docs do allow "Model identifier or
    alias", but alias keys are already rows of their own, so the exact-match
    pass covers the legitimate case.
  - Client impact (read from Claude Code 2.1.332's embedded JS): the retrieve path
    is hard-gated off, and clients normally look up ids obtained from the
    list — so display-name lookup is unlikely to be exercised.
  - Fix: match `id` only, or rewrite the returned `id` to the requested name.
  - Severity: low.

- [ ] **E6. `/v1/models` rows omit four documented fields and add an undocumented one**
  - Where: `src/anthropic.rs:833-846` (`AnthropicModelInfo`),
    `src/proxy.rs:260-266` (`ModelsQuery` — only `after_id`/`before_id`/`limit`)
  - Problem: the live ModelInfo schema is `type`, `id`, `capabilities`,
    `created_at`, `deprecated_at`, `display_name`, `lifecycle`, `line`,
    `max_input_tokens`, `max_tokens`, `retires_at` — with `lifecycle` always
    present and `line` nullable, and the list endpoint additionally
    accepting a `lifecycle` query filter. The proxy never emits
    `lifecycle`, `line`, `deprecated_at` or `retires_at`, silently ignores
    the `lifecycle` filter (a client asking for `lifecycle=["retired"]` gets
    active models), and serves a `description` field that the public schema
    does not have at all.
  - Client impact (read from Claude Code 2.1.332's embedded JS): gateway discovery
    GETs `/v1/models?limit=1000` and `safeParse`s the response, so a
    missing REQUIRED field would fail discovery outright — but the element
    schema could not be extracted from the binary, so whether the four
    omitted fields are optional there is unverified. Discovery is also
    hard-gated off by default.
  - Fix: emit `lifecycle: "active"`, `line: null`, `deprecated_at: null`,
    `retires_at: null` by default; either apply the `lifecycle` filter or
    document that it is ignored; keep `description` but note it is not in
    the public schema (E1's note says the current build reads it).
  - Severity: low.

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

- [ ] **F4. The upstream `/v1/models` GET bypasses the upstream gate**
  - Where: `src/proxy.rs:425` (`state.client.get(&url)` inside
    `collect_model_rows`), against the contract stated at
    `src/proxy.rs:75-76` and `src/upstream_gate.rs:1`
  - Problem: every POST goes through the gate — messages
    (`src/proxy.rs:811`), `count_tokens` and the advisor via
    `send_upstream_once` — but the models-list mirror does a bare
    `state.client.get`. With `max_concurrent_requests` set it is neither
    queued nor concurrency-limited, and with `rate_limit_retries` (default
    3) a 429 on model listing is reported immediately while message traffic
    is retried: a silent inconsistency between two client-visible paths.
  - Fix: route the GET through `state.gate.send(req)`, or weaken the two
    "every request" doc comments to name the exception.
  - Severity: low.

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
  - Verified against the Messages API request schema: a `tool_use` id and a
    `tool_result.tool_use_id` only have to match `^[a-zA-Z0-9_-]+$` — no
    prefix and no fixed length — so the `call_…` and `msg_resp_…` ids are
    **accepted**. The `toolu_` prefix is what Anthropic mints, not what it
    requires. The `message.model` echo is still unaddressed.
  - Unverified: whether any client logic keys on `message.model`.

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

- [x] **G7. `server_tool_use` ids must match Anthropic's `srvtoolu_` pattern**
  - Where: `src/server_tool_id.rs` (the shared generator),
    `src/stream.rs` (`emit_server_tool_use`), `src/translate.rs`
    (`build_content_blocks`)
  - Problem: the upstream-signalled web-search path minted
    `server_tool_use` ids of the form `stoolu_web_search_01`. Anthropic's
    Messages API constrains a `server_tool_use` id to
    `^srvtoolu_[a-zA-Z0-9_]+$` and validates it on **inbound** history, so
    a conversation recorded through the proxy could not be resumed against
    a real Anthropic subscription: the replayed history was rejected with
    `messages.N.content.M.server_tool_use.id: String should match pattern
    ^srvtoolu_[a-zA-Z0-9_-]+$`. The counter was also per-response, so the
    same id recurred across many turns of one conversation.
  - Fix: every server-tool id now comes from one shared generator
    (`src/server_tool_id.rs`) that mints `srvtoolu_<hex>_<counter>` from a
    process-wide counter, so the shape is right by construction and no id
    repeats inside a conversation. The `web_fetch` and `advisor`
    generators delegate to it, so all server-tool ids share one counter.
  - Note: an id already written into a client's transcript stays wrong —
    the API has no leniency for ids it did not mint. Existing transcripts
    need a new session, a `/compact` past the affected turns, or a
    hand-edit of the JSONL.
  - Severity: high (the conversation became unresumable against Anthropic).

- [ ] **G8. Server-tool blocks in replayed history need the tool declared**
  - Where: `src/translate.rs` (the echoed `server_tool_use` /
    `*_tool_result` blocks are folded into the upstream request)
  - Problem: a `server_tool_use` / `web_search_tool_result` pair recorded
    through the proxy is replayed to Anthropic by the client on a later
    turn. Anthropic pairs server-tool blocks to their results by id and
    checks the pairing against the request's `tools` array; a client that
    never declared the `web_search` **server** tool (Claude Code's own
    WebSearch is a client-side tool) may still be rejected, even with the
    ids fixed.
  - Unverified: the docs are explicit only about a *still-waiting* server
    tool (which must stay declared); whether a *completed* pair replayed
    without the tool declared is accepted is not documented. Assume it is
    rejected until proven otherwise.
  - Why the client replays them at all (read from Claude Code 2.1.332's
    embedded JS): its recognised content-block types include
    `server_tool_use`, `web_search_tool_result`, `web_fetch_tool_result`,
    `advisor_tool_result`, `mcp_tool_use` and `mcp_tool_result`, so those
    blocks are preserved and sent back verbatim — the mechanism behind G7
    as well.
  - Fix (larger): record the upstream's search as an ordinary
    `tool_use` / `tool_result` pair with a `toolu_`-shaped id, rather than
    dressing it up as a server tool — that is what Claude Code's own
    WebSearch does.
  - Severity: high, if confirmed.

- [ ] **G9. Upstream-signalled `web_search_result` entries have no `encrypted_content`**
  - Where: `src/translate.rs:3067`, `src/stream.rs:1095`
    (`encrypted_content: String::new()`); `src/anthropic.rs:630-634` skips the
    field when empty
  - Problem: Anthropic's schema marks `encrypted_content` as **required** on
    every `web_search_result` entry (only `page_age` is optional). The
    upstream-signalled path leaves it empty, so the field is omitted from
    the wire entirely — a result block that does not match the documented
    shape. The proxy-native search sets it (its own `oap1:` format, see
    G10), so this is specific to the upstream-signalled path.
  - Note: this is the same code path that minted the malformed
    `server_tool_use` id in G7.
  - Fix: fill it with something (the proxy-native path's own encoding
    would do), or drop the entries and keep only the citations.
  - Severity: low-medium.

- [ ] **G10. The proxy-native search's `encrypted_content` is not an Anthropic blob**
  - Where: `src/tools/web_search/protocol.rs:46`, `src/tools/web_search/history.rs`
  - Problem: the proxy writes `oap1:` + base64 of the page text, which is
    opaque rather than encrypted — documented as such in
    `src/tools/web_search/README.md`. Anthropic's own blob is opaque to the
    client and meaningful to the server, so a conversation moved to the real
    Anthropic API loses the page content.
  - Unverified: whether Anthropic rejects a non-Anthropic blob on replay.
  - Fix: strip the field on the way out to Anthropic, or record the search
    as an ordinary tool pair (the G8 sketch) so no server-tool block needs
    to survive the move.
  - Severity: low.

---

## H. Response and error shape (checked against the live schema and the client)

These were found by a later research pass that checked the current code
against the live Anthropic schema, and the client impact against the
installed Claude Code CLI (v2.1.332) by reading its embedded JavaScript.
That is a static reading of one client version, not a behavioural test —
strong evidence, not proof.

### H.a. Affects what the client shows

- [x] **H1. `usage.output_tokens_details.thinking_tokens` is not emitted**
  - Where: `src/anthropic.rs:684-685` (`Usage.thinking_tokens`), populated at
    `src/stream.rs:1217` and `src/translate.rs:2803`
  - Problem: Anthropic nests the thinking-token count under
    `output_tokens_details: { thinking_tokens }`. The proxy emits a flat
    `thinking_tokens` instead, and never emits `output_tokens_details`.
    The upstream's `output_tokens_details.reasoning_tokens` is flattened
    into it.
  - Client impact (read from Claude Code 2.1.332's embedded JS): the CLI reads ONLY
    the nested form — `o.output_tokens_details?.thinking_tokens` — and there
    is no unprefixed `.thinking_tokens` read in the binary. The flat field is
    stripped on parse, so the client's thinking-token accounting always
    rolls up **0**. A wrong displayed metric, not a failure.
  - Fix: `Usage` now serialises through a wire struct (`src/anthropic.rs`)
    that nests the count as `output_tokens_details: { thinking_tokens }`,
    only when non-zero. The flat `thinking_tokens` alias is dropped: the
    client strips it and Anthropic never sent it. The internal field is
    unchanged, so the TUI and stats still read it.
  - Severity: low-medium.

- [ ] **H2. The response `Message` omits `stop_details`**
  - Where: `src/anthropic.rs:565-576` (the `Message` struct)
  - Problem: Anthropic's response `Message` carries `stop_details`
    (`type: "refusal"`, `category`, `explanation`), plus `container` and
    `diagnostics`. The proxy declares none of them. Since B3 is fixed the
    proxy does emit `stop_reason: "refusal"`, but with no `stop_details`
    companion.
  - Client impact (read from Claude Code 2.1.332's embedded JS): every read is
    null-tolerant (`qo.stop_details?.category ?? null`), so the refusal
    itself — driven purely by `stop_reason` — still works. What degrades is
    the category/explanation display, the category-gated refusal-fallback
    lane, and delta-borne `resumption` tokens (an Anthropic-lane feature
    that cannot exist through the proxy anyway).
  - Fix: declare the fields so a refusal can carry its details.
  - Severity: low.

### H.b. Cosmetic for Claude Code (shape fidelity only)

- [ ] **H3. `message_delta` omits `stop_sequence` (always) and `stop_reason` (abnormal paths)**
  - Where: `src/anthropic.rs:817-822` (`MessageDeltaPayload`), `src/stream.rs:477-483`
  - Problem: Anthropic's `message_delta.delta` declares four non-optional
    fields — `container`, `stop_details`, `stop_reason`, `stop_sequence` —
    and its examples show `"stop_sequence": null` present in every
    `message_delta`. The proxy's payload omits `stop_sequence` entirely
    (both fields are `skip_serializing_if = "Option::is_none"`, and
    `stop_sequence` is hard-wired `None` on both the streaming and the
    advisor path).
  - Client impact (read from Claude Code 2.1.332's embedded JS): `stop_sequence` and
    `stop_details` are `.optional()` and every read is `??`-guarded, so
    absent keys yield `undefined` and are tolerated — the proxy already
    streams to this client with `stop_sequence` absent every turn.
  - Fix: drop `skip_serializing_if` from `stop_sequence` (emit null when
    unset); consider always emitting `stop_reason` too.
  - Severity: low (cosmetic for Claude Code).

- [ ] **H4. Tool-call arguments are buffered into one `input_json_delta`**
  - Where: `src/stream.rs:835-850` (`emit_tool_args_delta` accumulates and
    discards its events), `src/stream.rs:888-912` (`emit_repaired_args`
    emits the whole blob at block close); same shape in
    `src/tools/advisor/sse.rs:134-152`
  - Problem: Anthropic streams `input_json_delta` incrementally as each
    key/value completes. The proxy sends nothing until the tool block
    closes, then one delta.
  - Client impact (read from Claude Code 2.1.332's embedded JS): the client parses
    partial JSON per delta with a try/catch fallback and accumulates
    per event, so one blob at block close parses fine. The loss is
    progressive rendering and spinner-byte granularity during long argument
    generation — the tool cannot execute before `content_block_stop`
    regardless.
  - Note: the buffering is deliberate — it is what enables argument repair
    (B2). This is a fidelity cost, not a bug.
  - Fix: stream syntactically-committed JSON prefixes incrementally and
    keep the repair pass authoritative; or accept and document it.
  - Severity: low (cosmetic for Claude Code).

- [ ] **H5. Unknown routes and methods answer with axum's default 404/405**
  - Where: `src/proxy.rs:193-215` (the router has no `.fallback()`)
  - Problem: a request to any other Anthropic endpoint (`/v1/batches`,
    `/v1/files`, `/v1/complete`, …) or a wrong method on `/v1/messages`
    gets axum's stock 404/405 rather than Anthropic's
    `{"type":"error","error":{"type":"not_found_error"}}` envelope.
  - Client impact (read from Claude Code 2.1.332's embedded JS): the error path is
    parse-safe — a non-JSON body yields the right status with
    `error.type === undefined` instead of throwing — and Claude Code never
    calls the routes that would miss (per E4). An SDK user that JSON-parses
    every error body would see a parse failure instead of a typed error.
  - Fix: add a `fallback` handler returning an Anthropic-shaped envelope.
  - Severity: low.

- [ ] **H6. Upstream error bodies are re-wrapped verbatim into `error.message`**
  - Where: `src/error.rs:266-269` (`AppError::Upstream`)
  - Problem: the client-visible `error.message` is
    `"upstream returned {status}: {entire raw upstream body}"` — raw
    OpenAI-shaped JSON embedded inside an Anthropic envelope. Anthropic's
    own messages are human-readable sentences, and the embedded body can
    leak upstream-internal detail.
  - Client impact (read from Claude Code 2.1.332's embedded JS): behaviour keys on
    `status` and `error.type`, never on free text; the text matchers that
    do exist are `$`-anchored, so the embedded body cannot false-match
    them. Context-overflow errors never take this path (they keep
    Anthropic's wording).
  - Fix: map common upstream error codes to Anthropic-worded messages and
    keep the raw body in a log field, not the client's `message`.
  - Severity: low.

---

## Not planned (inherent or out of scope)

`top_k`, real thinking-budget enforcement, `code_execution`, `tool_search`,
programmatic tool calling, the MCP connector (`mcp_servers`),
`context_management`, `container`, `model_context_window_exceeded`, and
`pause_turn` outside the opt-in. If one of these becomes a requirement, it needs
its own design rather than a quick fix.

Also inherent, and listed in `REFERENCE.md`'s "Out of scope" section:

- **Model-generated images** — the proxy is text-only in both directions.
- **Assistant-turn `image` blocks** — dropped with a warning, because the
  Responses API has no assistant-image content part to map them to. (User
  and `tool_result` images ARE supported.)
- **Native reasoning replay mints a synthetic `rs_` id** — the upstream's
  own reasoning-item id is lost at the Anthropic wire boundary.
- **Claude Code's standby advisor path** — only an explicitly configured
  `advisorModel` works behind a proxy.
- **Proxy-driven resumption of a paused server-tool turn** — the proxy can
  end a turn with `stop_reason: "pause_turn"` (opt-in) and accepts the
  client's resend, but it never re-sends the conversation itself, and
  Claude Code does not resume it.

## Suggested order

A7 and B6 first, then C1, D1, and the rest as needed.

The server-tool id work (G7, G8, G9, G10) belongs together: G7 fixed the
`server_tool_use` id shape, and the other three are what remains of the same
"a conversation recorded through the proxy must survive being replayed
against Anthropic" problem. G8's fix — recording the upstream's search as an
ordinary `tool_use` / `tool_result` pair — would also settle G9 and G10.

B8 is fixed: a queued or backing-off streaming request now gets the early
start (placeholder `message_start` + pings) instead of silence before the
first byte.
