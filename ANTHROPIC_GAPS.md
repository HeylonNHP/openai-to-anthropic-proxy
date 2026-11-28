# Anthropic API gaps: fixes to work through

Source: read-only research passes over `src/` (request side, response side,
endpoints and tools). A later pass re-checked the current code and the live
Anthropic schema for the entries added since, and checked a few of them
against the installed Claude Code client. Nothing was run against a live
upstream.

A 2026-11-28 pass used four parallel research agents — two re-reading
`src/`, two reading the installed Claude Code CLI's (v2.1.332) embedded
JavaScript out of the native binary — and added section I, plus the
client-impact evidence that settled G3's unverified clause. Section I's
medium entries (I1–I4, I11) were spot-checked against the current source
when written; client-impact claims there are static readings of that one
build, same evidence class as section H.

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
  - Was unverified; settled by the 2026-11-28 decompilation pass: the CLI
    DOES key on `message.model` — a read of it warms
    `servedModelsByRequestedModel`, later consumed by served-model
    mismatch checks — so the echo feeds model-tracking lane logic, not
    rendering. The echo itself is still unaddressed in the proxy.

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

## I. From the 2026-11-28 pass: decompiled client + source re-read

Source: four parallel research agents — two re-read `src/`, two read the
CLI's embedded JavaScript (v2.1.332, strings dump of the native binary).
Client-impact claims are static readings of that one build; anything
neither confirmed in the JS nor in the code is marked *unverified*.
The medium entries were spot-checked against the current source when
written. Nothing was run against a live upstream.

### I.a. Response and streaming correctness

- [x] **I1. The server-tool `assemble` lets `tool_use` outrank `max_tokens` and `refusal`**
  - Where: `src/tools/server_loop/render.rs` (`assemble`, used by `render`
    and `finish_streaming`).
  - Problem: a pending client `tool_use` overrode every other stop reason, so
    a refused final run still had its tools executed, and a cut-short call
    was run with truncated input instead of retried with a bigger budget. The
    plain path ranks both the other way.
  - Fix: the last run's `refusal`, then `max_tokens`, outrank a pending tool.
    A pause with nothing pending still outranks `max_tokens`.
  - Not covered: only the LAST run's reason is read. A middle round that is
    refused or cut off, and also calls a server tool, is still served, and
    its reason is lost.
  - Severity: medium.

- [ ] **I2. The client-level `request_timeout` bounds the whole streamed body**
  - Where: `src/main.rs:171-176` (`ClientBuilder::timeout(config.request_timeout)`
    on the shared client); message sends pass `timeout: None`, so the client
    total applies; failure surfaces as an upstream error around
    `src/proxy.rs:1975-1980`.
  - Problem: reqwest's client-level timeout runs from when the request
    starts connecting until the response body has finished — so connect +
    first byte + the **entire SSE body** must complete inside
    `request_timeout_secs` (default 600 s) or reqwest aborts the read. A
    healthy streamed turn generating for >10 minutes dies mid-stream with
    `upstream SSE error: … timeout` → in-stream `api_error`, minutes of
    tokens spent, no HTTP status for Claude Code to back off on. No
    `read_timeout` (per-read stall) is configured, so bytes flowing at 10 Hz
    do not hold the deadline off. Queue/backoff sit outside every timeout by
    design (B8), and the early-start/ping machinery exists to keep long
    waits alive — this is the mid-stream mirror of B8's pre-first-byte
    problem.
  - Client impact: Claude Code's watchdogs are idle/byte-based (B5: pings
    every 10 s hold them off indefinitely), so the client would happily
    wait — the proxy is what kills the turn; the CLI's reaction to the
    in-stream `api_error` is *unverified*, at best a full re-send.
  - Fix: stop using the total deadline as a body bound for streams — a
    per-read `read_timeout` (stall detection) instead, or override
    `.timeout()` on streaming sends; keep a total cap only for buffered
    sends. Operator workaround today: raise `request_timeout_secs` (which
    also lengthens buffered 504s).
  - Severity: medium.

- [ ] **I3. The plain path never checks that the upstream actually streamed**
  - Where: `src/proxy.rs:1215-1224` (`execute` picks the streaming path
    purely from `outbound.stream`, before any body/content-type check);
    `src/proxy.rs:1396-1414` (`render_streaming` clones the upstream's
    `Content-Type`, defaulting to `text/event-stream` only when the header
    is *absent*); `parse_upstream_body` then assumes JSON.
  - Problem: two upstream mismatches are unhandled on the ordinary
    (non-server-tool) path: (a) `stream: true` answered with
    `200 application/json` — the client gets an SSE-typed response whose
    body is a JSON document the event parser never turns into events
    (in-stream `error`, "stream ended before a terminal response event"),
    with the wrong `Content-Type` on the wire; (b) buffered client +
    streaming upstream — the SSE text hits `serde_json::from_slice` and
    becomes a 502 "upstream returned non-JSON body". The server-tool loop
    already handles both directions (`src/tools/server_loop/run.rs:534-546`
    JSON sniffing, `:320-325` `UnexpectedStreaming`), and its README
    documents that an upstream ignoring `stream: true` is still served — so
    the upstream behaviour is known-real.
  - Client impact: a failed turn on the weak in-stream error lane (B8's
    note) instead of the reply the proxy held in hand. Claude Code branches
    on the response `Content-Type` (JS evidence: usage metering checks the
    header for `text/event-stream` to choose SSE vs JSON reading; a relay
    path cancels a 200 whose type lacks it — `relay_not_sse`). Which
    configured upstreams ignore `stream` is *unverified*.
  - Fix: always emit `text/event-stream` from `render_streaming`, and sniff
    content-type/shape in `execute` before choosing the path, routing each
    body through the matching renderer — as `run_streamed` already does.
  - Severity: medium.

- [ ] **I4. Silence between the committed HTTP 200 and the first SSE event**
  - Where: `src/proxy.rs:1810-1822` (`note_sent` sets `message_started` only
    when a `message_start` frame was produced); `src/proxy.rs:2000-2010`
    (the `ping` fires only when `message_started`); `src/stream.rs:266-286`
    (`message_start` is emitted only when the first upstream event arrives).
  - Problem: the early-start deadline covers only `execute()` (queue,
    backoff, headers). When the upstream's headers arrive inside
    `START_AFTER` but its first event (`response.created`) comes later,
    `render_streaming` has already committed 200 with no placeholder, and
    the ping gate stays shut until that first event — unbounded body
    silence. B5 scoped its "cannot cover" remark to the pre-headers case
    (which B8's early start then covered), and both keep-alive bodies
    (`early_start::keep_alive_stream`, the server-loop's
    `keep_alive_response`) ping from the moment `message_start` goes out —
    this post-headers window on the plain path is the one left open.
  - Client impact: *unverified* frequency — but axum commits headers with
    the response while Claude Code's watchdog counts body bytes, so an
    upstream buffering its first event past ~300 s trips the byte watchdog
    (B5's documented abort + full re-send, tokens paid twice); shorter
    stalls just sit silent against the design goal in `early_start.rs`'s
    module doc.
  - Fix: after `render_streaming` commits, if no first event arrives within
    one ping interval, send the placeholder `message_start` (as the
    early-start body does) or lift the `message_started` gate with a
    synthetic start.
  - Severity: medium.

- [x] **I5. A client `WebSearch` call is also presented as a completed server search**
  - Where: `src/translate.rs` (`is_web_search_function_call`), shared by the
    plain, buffered and server-tool paths
  - Problem: the lowercase `web_search` name was exempt when the client
    declared it as its own function tool, but `WebSearch` always matched. So
    every call to Claude Code's `WebSearch` also produced an invented
    `server_tool_use` / `web_search_tool_result` pair and counted a search
    that had not run. The `tool_use` itself was right, and ran once.
  - Fix: the declared-tool exemption now covers both names. A call to an
    *undeclared* `WebSearch` / `web_search` is still read as a search.
  - Confirmed in a 2026-11-28 proxy log and session transcript before the fix.
  - Severity: medium (every main-loop web search).

### I.b. Request features silently dropped (not in D2/D4/D5)

- [ ] **I6. `safeguards` and `restore_if_safeguards_refused` are dropped without a warning**
  - Where: no `safeguards` anywhere in `src/`; not declared in
    `CreateMessageRequest` (`src/anthropic.rs:45-93`); JS marker
    `extraBodyParams:{safeguards:` — `safeguards:[{type,classifier_context}]`
    plus betas `dangerous-tool-use-2026-09-03`,
    `auto-mode-classifier-2026-07-16`, `subagent-review-2026-11-17`.
  - Problem: the top-level `safeguards` field rides along on auto-mode
    requests (auto_mode is a common permission mode) and is silently
    ignored — not in D2's drop list. The CLI also reads back
    `restore_if_safeguards_refused === true` to drive a
    "safeguards refused → switch model" lane that can never engage through
    the proxy, with no log either way.
  - Client impact: running an upstream safety classifier is inherent to an
    OpenAI upstream; the *silence* is the gap (security-adjacent feature
    dropped invisibly). Suppression via beta policy does not apply — the
    fields arrive via `extraBodyParams`, not the org-gated
    `CLAUDE_CODE_EXTRA_BODY`.
  - Fix: add `safeguards` and `restore_if_safeguards_refused` to D2's
    warn-once list.
  - Severity: low-medium (inherent but uncatalogued, security-adjacent).

- [ ] **I7. `context_hint` and its beta are dropped without a warning**
  - Where: no `context_hint` in `src/`; JS marker
    `body:u?{context_hint:{enabled:!0,…target_tokens_saved:i}}` with beta
    `context-hint-2026-04-09`.
  - Problem: `context_hint:{enabled, target_tokens_saved}` asks the API to
    emit compaction hints; the proxy drops field and beta with no log.
  - Client impact: gated client-side; consumed via a `context_hint_sse`
    error-origin tag and a `compact_micro_keep_recent` action with
    `trigger:"context_hint"` — dropping it disables that compaction
    trigger. Emitting hints against an OpenAI upstream is inherent.
  - Fix: add `context_hint` to D2's warning list and note hints cannot be
    produced.
  - Severity: low-medium, conditional.

- [x] **I8. `tool_reference` content blocks are dropped**
  - Where: `src/translate.rs` (`tool_result_output`)
  - Problem: a tool search (Claude Code's ToolSearch) returns a
    `tool_result` holding only `tool_reference` blocks (`{tool_name}`).
    They have no `text`, so they were dropped and the model saw an empty
    result. (Not `ContentBlockParam::Unknown`, as first written: tool
    result content is raw JSON.)
  - Fix: each one becomes the text `Loaded tool: <name>`. The loaded tools'
    schemas still arrive in the request's `tools`.
  - Confirmed in a 2026-11-28 proxy log and session transcript before the fix.
  - Severity: medium (every deferred tool load).

- [ ] **I9. Tool `annotations` and `hints` are silently ignored**
  - Where: `Tool` (`src/anthropic.rs:433-454`) declares
    name/kind/description/input_schema/strict/options only; CLI JS builds
    tools with `annotations:{title,readOnlyHint,destructiveHint,openWorld}`
    and `hints:{search_hint,…,always_load}`.
  - Problem: inbound `annotations` and `hints` are dropped without a log.
    D4 lists `input_examples`, `defer_loading`, `allowed_callers` and
    `eager_input_streaming` but not these.
  - Client impact: Claude Code attaches them notably to MCP tools (where
    `readOnlyHint` etc. are standard); advisory for Anthropic tooling and
    near-neutral OpenAI-side, but the drop is silent.
  - Fix: extend D4 — fold `title`/hints into the upstream tool description
    or declare-and-warn.
  - Severity: low.

### I.c. Retry and error handling

- [ ] **I10. The learned parameter-strip retry covers only `temperature` and `top_p`**
  - Where: `src/capabilities.rs:53-62` (`RequestParam::ALL = [Temperature,
    TopP]`), `:136-148` (`unsupported_parameter` gated on `ALL`); the
    strip-retry loop is `src/proxy.rs:1067-1099`.
  - Problem: proxy-injected fields with no strip path: forced
    `strict: true` (`src/translate.rs:298`), `reasoning` incl. `summary`
    (`src/translate.rs:355-360`, `src/proxy.rs:941-943` — A3 already warned
    some servers reject it), `user` (`src/translate.rs:327-331`),
    `prompt_cache_key` (`src/proxy.rs:972-974`), `parallel_tool_calls`
    (`src/translate.rs:316-325`). A 400 naming any of them unsupported is
    passed straight to the client, while `temperature`/`top_p` self-heal
    process-wide.
  - Client impact: *unverified* — needs an upstream that rejects one of
    these keys; when it happens the proxy is unusable for that model with
    no self-heal.
  - Fix: add variants, or a generic "strip the named outbound key" fallback,
    for the proxy-injected optional fields (the store's own docstring says
    adding a variant is the only change needed).
  - Severity: low-medium.

### I.d. Response shape fidelity

- [ ] **I11. Usage keys the CLI's schema marks required-nullable are omitted when zero**
  - Where: `UsageWire` (`src/anthropic.rs:716-734`) —
    `skip_serializing_if = "is_zero_u32"` on `cache_creation_input_tokens`
    and `cache_read_input_tokens` (and the flat `_5m`/`_1h` fields);
    `cache_creation`, `service_tier`, `speed`, `inference_geo`,
    `fallback_credit` never modelled.
  - Problem: the CLI's zod usage schema marks `cache_creation_input_tokens`,
    `cache_read_input_tokens`, `cache_creation` and `service_tier`
    **nullable but not optional** — `.nullable()` ≠ `.optional()`, so a
    missing key fails `safeParse` where an explicit `null` passes. The proxy
    omits them whenever zero, i.e. on every uncached turn. At least one CLI
    site also reads unguarded (`h.cache_creation.ephemeral_1h_input_tokens`,
    no `?.`), which on a missing `cache_creation` object is a TypeError.
  - Client impact: *unverified* whether any `safeParse`/unguarded read ever
    sees a raw proxy usage object — mitigating evidence: the CLI normalises
    its own usage through defaults and merge paths that tolerate
    `undefined`, so most runtime readers degrade rather than break; the
    exposed lane is a record/resume path that parses a persisted
    `message.usage` taken straight from the proxy's `message_start` —
    missing required keys → record rejected — plus the unguarded
    `cache_creation` read.
  - Fix: always emit `cache_creation_input_tokens: 0` and
    `cache_read_input_tokens: 0`, plus `cache_creation: null` and
    `service_tier: "standard"` — matches Anthropic's own wire, costs
    nothing. Distinct from G1 (which is about the `cache_creation` TTL
    object's contents and the non-standard flat `_5m`/`_1h` fields).
  - Severity: medium, *unverified* on parse/crash reachability.

- [ ] **I12. On the streaming path the `server_tool_use` input arrives as `""` → `{}`**
  - Where: `src/stream.rs:1070-1090` — the start event carries
    `input: {"query": "Web search"}` then an immediate `content_block_stop`,
    with no `input_json_delta`; the buffered path keeps the object
    (`build_content_blocks`).
  - Problem: the client discards any object input at `content_block_start`
    (it stores `input: ""`) and only accumulates `input_json_delta` — which
    the proxy never sends for this block — so the recorded block ends as
    `input: ""`, and the stop-time JSON parse turns it into `{}`. Every
    streamed search turn therefore records/replays
    `server_tool_use.input = {}` while the same turn non-streaming delivers
    `{"query":"Web search"}`: a cross-path inconsistency in transcript,
    replay, and input-hash telemetry.
  - Client impact: verified mechanism (the start handler overwrites input,
    the validator never checks it — silent); visible in transcript/history,
    no crash. Extends G2: G2's fix alone (the fake query's content) would
    not restore the value on the streaming path.
  - Fix: follow `content_block_start` with one `input_json_delta` carrying
    the JSON (the same buffered single-delta pattern as H4), or accept `{}`
    and fold the observation into G2.
  - Severity: medium-low.

- [ ] **I13. `message_start.usage.input_tokens` is 0 on virtually every streamed turn**
  - Where: `src/stream.rs:565-595` — `build_message_start` takes the
    legacy-placeholder branch (`input_tokens: 0, output_tokens: 1`) unless
    `input_usage_seen`, which is set only at the terminal event (after
    `message_start` has gone out); only upstreams that put usage in
    `response.created`/`in_progress` get the real branch. The early-start
    placeholder is all-zero too.
  - Problem: Anthropic's `message_start` carries the real input count
    (input is known before generation); the proxy's almost never does. The
    closing `message_delta` carries real usage and the client's merge folds
    it in, so displayed/final usage is correct — but every read keyed to
    `message_start` sees 0: the CLI's `first_request_input_tokens` ("lets a
    consumer tell a cold prompt cache from a large prompt") and its
    prompt-size latency bucket both sum `message_start` usage, so proxied
    turns all bucket as 0-token prompts. B8 documents the *early-start*
    placeholder, not the always-placeholder on the normal path.
  - Client impact: telemetry/`@internal` fields; *unverified* whether any
    non-telemetry logic keys on them (the `output_tokens` read is healed by
    `Math.max`).
  - Fix: buffering `message_start` until usage arrives is impractical with
    streaming — forward `response.created`/`in_progress` usage when the
    upstream provides it (already implemented) and document the remainder as
    inherent; or accept as telemetry-only.
  - Severity: low.

- [ ] **I14. Adjacent text blocks are re-joined with inserted separators Anthropic never sent**
  - Where: `src/translate.rs:1587-1588` (consecutive user text blocks merged
    with a pushed `'\n'`), `:2282-2287` (assistant text blocks joined with
    `'\n'`), `:381-388` (system blocks joined with `"\n\n"`), `:2643-2647`
    (system-message blocks joined with `"\n"`).
  - Problem: Anthropic delivers each text block as-is; the proxy injects
    separators when merging — notably when prompt caching is unconfigured,
    so every block's `cache_control` fails the `ephemeral` gate and all
    blocks merge.
  - Client impact: *unverified* — depends on Claude Code sending 2+ text
    blocks in one message; the alteration is deterministic when it does.
  - Fix: concatenate without inserting separators (or join only where the
    client itself had separated), and note the choice in the translation
    overview.
  - Severity: low.

- [ ] **I15. With N upstream searches, only the first result block carries results, and pairs are emitted out of position**
  - Where: `src/stream.rs:1092-1120`, `src/translate.rs:3061-3072` (all
    collected citations go to search #0; searches 2..N get `content: []`);
    `src/translate.rs:3054-3084` (every pair hoisted to the FRONT of
    `content`); `src/stream.rs:657-690` (`server_tool_use` start/stop
    emitted while the preceding text block is still open — overlapping
    blocks; G4 covers only parallel tool/tool overlap).
  - Problem: later result blocks claim zero results; buffered ordering
    doesn't reflect when the search happened; streaming overlaps open
    blocks where Anthropic has one at a time.
  - Client impact: Claude Code's renderer maps result entries directly, so
    later searches show 0 results; its per-index block accumulator
    tolerates the overlap (read from JS), so ordering is shape fidelity —
    *unverified* for stricter SDKs.
  - Fix: attribute each result block its results, emit pairs at their
    chronological position, and close the text block before opening
    `server_tool_use`.
  - Severity: low.

### I.e. Found while verifying I1 (probe tests against a fake upstream)

- [x] **I16. The server-tool path loses some refusal and truncation signals**
  - Where: `src/tools/server_loop/run.rs` (`StopSignals`), `live.rs`
    (`consume`), `render.rs` (`translate_final`).
  - Problem: a pending client tool was reported as `tool_use` although the
    turn was refused or its call cut short, when (a) the round ending the turn
    was already served (a pause or a failed re-run), so its output was
    dropped; or (b) a streamed run showed it only in its events.
  - Fix: each run's refused / cut-short flags are read as it happens (from
    the response, and from the live translator) and carried into the session
    result.
  - Since fixed (with I17): a streamed paused round whose terminal `output`
    omits a client call ends `tool_use`.
  - Not covered: a re-run that fails part way through its stream keeps the
    previous round's flags, and a client `tool_use` the failed run streamed,
    even a complete one, is not counted, so the turn can end `end_turn` after
    it.
  - Severity: medium.

- [x] **I17. A plain streamed tool call missing from the terminal `output` ends `end_turn`**
  - Where: `src/stream.rs`, the terminal-event handler (`has_tool_use` was read
    from `response.output` only); `src/tools/server_loop/live.rs` (`consume`,
    `Items::add_missing_calls`).
  - Problem: when `response.completed` carries `output: []`, a call that was
    streamed to the client is followed by `stop_reason: "end_turn"` instead of
    `tool_use`. The server-tool path gets the empty case right; when the
    terminal `output` has other items but omits the call, both paths say
    `end_turn`.
  - Fix: the plain path remembers that it sent a client `tool_use` block. The
    server-tool path adds each call announced by `output_item.added` and
    missing from the terminal `output` back into the run's response, so it
    also counts toward the stop reason, and the loop's own calls are served.
  - Not covered: calls are matched by `call_id`, so an upstream that renamed
    one in its terminal `output` would have it counted twice (a loop tool
    would run twice). None is known to. Recovered calls are appended, not put
    back at their stream position, so the next run's replay can separate a
    reasoning item from its call.
  - Severity: medium if an upstream sends that shape; which do is
    *unverified*.

- [ ] **I18. An empty `response.refusal.done` or `refusal.delta` marks the turn refused**
  - Where: `src/stream.rs`, the `RefusalDelta` and `RefusalDone` arms set
    `refused` without checking that the text is non-empty.
  - Problem: an ordinary answer followed by an empty refusal event ends
    `refusal`. The plain path always did this; the I16 fix brought the
    `refusal.done` case to the server-tool path, which said `end_turn` before.
  - Fix: set the flag only for non-empty text.
  - Severity: low. No upstream is known to send it.

- [ ] **I19. Whitespace `function_call_arguments.done` overwrites the streamed arguments**
  - Where: `src/stream.rs` (`maybe_finalize_tool_args`),
    `src/tools/server_loop/live.rs` (the `FunctionCallArgumentsDone` arm):
    both guard on `is_empty`, not on blank.
  - Problem: full argument deltas followed by a `done` carrying `"   "` give
    the client `{}`, on both paths.
  - Fix: treat blank like empty.
  - Severity: low. No upstream is known to send it.


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

Section I came from the 2026-11-28 pass (decompiled client + source
re-read). Its medium entries are B-class response-correctness work and
deserve the next look alongside A7 and B6: I2 (the stream body bound), I3
(an upstream that does not actually stream), I4 (post-200 first-event
silence) and I11 (required usage keys). I6–I9 are catalogue-and-warn work in the
D2/D4/D5 vein.
