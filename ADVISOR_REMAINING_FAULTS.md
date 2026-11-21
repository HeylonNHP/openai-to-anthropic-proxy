# Advisor faults still to fix

Findings from reading the Raspberry Pi proxy log for 2026-11-19 against the code.
The advisor (`beam`, a reasoning model) kept returning "Advisor unavailable" in
Claude Code. Faults #1 to #5 are fixed; this file records the three that remain
(#6 to #8; the #5 entry is kept as the original analysis), with enough detail to pick them up cold on another machine.

Line numbers drift, so code is referred to by function and constant name.
The log lines quoted here are the evidence; the log itself is not in the repo.

## Status

| # | Fault | Status |
|---|---|---|
| 1 | Whole conversation sent to the advisor, no size limit | Fixed, `78590a9` (docs `6474790`) |
| 2 | Output cap shared with hidden reasoning; cut-off answers discarded | Fixed, `a61fac6` |
| 3 | Advisor acts as the executor and calls a tool instead of answering | Fixed, `b45d3f5` |
| 4 | Failed consults burn `max_uses`; no retry | Fixed, `1831a89` |
| 5 | Executor and UI only ever see a generic "unavailable" | Fixed (commit "report why an advisor consultation failed") |
| 6 | Retired `default_model` makes unmapped model names fail completely | Code fixed (commit "name default_model when the fallback is itself unavailable"); **config change on the Pi still needed** |
| 7 | Only the first failure per turn is logged | Fixed (commit "log every failed advisor consultation") |
| 8 | 45 s timeout vs. huge prompts; transcript re-sent uncached on every consult | Timeout removed (commit "remove the advisor's default timeout"); caching still **open** |

Ranked most severe and likely first. Fix them in order.

A correction to an earlier note: the "85" `advisor result carries no plaintext
advice` lines in the log are the proxy re-reading old error blocks from the
conversation history on each request. They are not 85 separate failures. The log
shows 6 real advisor failures, one per turn (see #7 for why only one per turn).

---

## #5. The executor and the UI only ever see a generic "unavailable" (FIXED)

> Fixed: failures are sorted into a `FailureKind` in `src/tools/advisor/tool.rs`, mapped to the client-known error codes, and the executor is told the reason. Documented in `proxy.json-guide.md` ("Why a consultation failed"). The notes below are the original analysis.

**Plain English.** Every advisor failure, whatever the cause, produces the same
`unavailable` error block for Claude Code and the same "temporarily unavailable"
text for the executor model. A prompt-too-long error, a timeout, an overloaded
upstream and a model that returned nothing are indistinguishable, so the executor
cannot adapt and the operator sees only "Advisor unavailable (unavailable)" in
Claude Code. The real reason is visible only in the proxy log.

**Where.**
- `src/tools/advisor/tool.rs`: `AdvisorTool::answer`, `Err(error)` branch. It
  always calls `protocol::unavailable(&id)` and returns the fixed `NO_ADVICE`
  text (apart from the "given up" text added in #4).
- `consult` / `consult_once` return `Result<_, String>` (via `ConsultError`), so
  the cause is a free-text message, not something that can be matched on.
- `src/tools/advisor/protocol.rs` already defines the error codes Claude Code
  understands: `unavailable`, `prompt_too_long`, `overloaded`,
  `too_many_requests`, `execution_time_exceeded`, `model_not_found`,
  `max_uses_exceeded` (see `ADVISOR_ERROR_CODES`). Only `unavailable` and
  `max_uses_exceeded` are ever emitted today.

**Evidence.** The UI screenshots show only "Advisor unavailable (unavailable)".
The real causes in the log were: `advisor returned no text`,
`advisor response status is incomplete`, and
`upstream returned 400 Bad Request: ... The prompt is too long: 1424454, model
maximum context length: 1048576`.

**Suggested fix.**
1. Give `ConsultError` a `kind` (an enum) instead of relying on the message:
   `PromptTooLong` (HTTP 400 whose body mentions "too long"/"context length"),
   `Overloaded` (5xx, 529), `TooManyRequests` (429), `Timeout`,
   `ModelNotFound` (404), `OutputLimit` (incomplete with no text),
   `NoAdvice` (no text, or a tool call), `Other`.
2. Map the kind to the matching `protocol::ERROR_CODE_*` for the client block
   instead of always `unavailable`. Check `protocol.rs` first for what each code
   is allowed to look like, and `ADVISOR_TOOL_RESEARCH.md` (the error-code
   section) for how Claude Code renders them. The comments around
   `scrub_error_body` and the "latching" note explain why advisor error text must
   not leak tokens like `advisor_20260301` to the client.
3. Put a short, honest reason in the text given to the executor (for example
   "The advisor could not be consulted because the conversation is too long for
   it"), still never worded like advice. Keep `NO_ADVICE`'s instruction to
   continue without a second opinion.
4. Tests: one per kind, asserting both the client `error_code` and the executor's
   `function_call_output` text.

**Watch out for.** `error_code` must stay one of the six client-known codes or
Claude Code may treat the block as malformed. Do not put upstream error bodies in
the executor text without `scrub_error_body`.

---

## #6. A retired `default_model` makes unmapped model names fail completely (CODE FIXED, CONFIG STILL OPEN)

> Code: when the `default_model` fallback is itself refused (a 400/404 model rejection or a 410 retirement), the client error now says the proxy's `default_model` is unavailable and to update it, followed by the upstream's reason. The request still fails, so the Pi's `proxy.json` still needs a live `default_model` and the missing `claude-sonnet-5`/`sonnet-5` aliases (step 1 below).

**Plain English.** Your `default_model` points at `deepseek-v4-flash:0731`, which
the upstream retired on 25 September 2026. When a client asks for a model name
that is not in `model_aliases.map` (here `sonnet-5`), the upstream answers 404,
the proxy falls back to the retired default, and that answers 410. The request
then fails. These requests look like Claude Code's own side calls (for example the
auto-mode permission check), not the advisor, but they fail every time they occur.

**Evidence (log, 2026-11-19).**
```
05:03:41.147 WARN upstream error status=404 body={"error":{"message":"model \"sonnet-5\" not found. Try glm-5.3-flash:cloud.", ...}}
05:03:41.150 WARN upstream rejected model; falling back to default_model
             inbound_model=claude-sonnet-5 rejected_model=sonnet-5
             fallback_model=deepseek-v4-flash:0731 suggested_model=Some("glm-5.3-flash:cloud")
05:03:41.543 WARN upstream error status=410 body={"error":{"message":"deepseek-v4-flash:0731 was retired on September 25, 2026. Try glm-5.3-flash:cloud. ..."}}
```
Five occurrences each of the 404 and the 410. The `proxy.json` alias map has
`claude-sonnet-5-5`, `sonnet`, `claude-opus-5-5`, `opus`, `claude-haiku-5-5`,
`haiku`, but not `claude-sonnet-5` / `sonnet-5`, and `default_model` is
`deepseek-v4-flash:0731`.

**Where.**
- `proxy.json` on the Pi: `model_aliases.default_model` (config, not code).
- `src/proxy.rs`: the model-rejection fallback (search for
  `upstream rejected model; falling back to default_model`), and
  `src/model_rejection.rs` (`detect_model_rejection`).
- `src/config.rs`: `model_aliases.default_model` resolution (around the
  `default_model` field of the aliases struct).

**Suggested fix.**
1. Config, immediately: change `default_model` to a live model (the upstream
   suggests `glm-5.3-flash:cloud`; your config already uses `glm-5.3-flash`), and
   add the missing inbound names to `model_aliases.map` (`claude-sonnet-5`,
   `sonnet-5`, and any other `claude-*` names that show up as "not in the alias
   map" in the log).
2. Code, optional but worthwhile: when the fallback model is itself rejected
   (404/410), do not return the fallback's error as if it were the original. Say
   in the error that `default_model` is itself unavailable, and consider using the
   upstream's `suggested_model` hint, which the log shows the proxy already parses.
3. Tests: a fallback that is rejected too must produce a clear error naming
   `default_model`.

**Watch out for.** Do not paste the real `proxy.json` (it holds an API key) into
the repo or into issues.

---

## #7. Only the first failure per turn is logged (FIXED)

> Fixed: every failed consultation is now a `warn` line in the tracing log (failure code, `failures_this_turn`, `prompt_chars`, `giving_up`, the error text), and the failed attempt that triggers the retry is logged too. Only the Claude Code/TUI line is still limited to one per turn. Notes below are the original analysis.

**Plain English.** The advisor warning is deliberately limited to one per turn, so
if a turn has several failed consultations, only the first leaves a log line. The
real failure rate is hidden, and since #4 a failed consultation is retried, so the
retry's failure is also easy to miss. This is why the log shows 6 warnings while
more turns were affected.

**Where.**
- `src/tools/advisor/tool.rs`: `warn_once` and `should_warn`; the `warned` field
  of `AdvisorTool`. The one-warning limit applies to both the `tracing::warn!` and
  the TUI line, which is sensible for the TUI but not for the log.
- Tests that pin the one-warning behaviour: `should_warn_gates_tracing_and_tui_warnings_to_once_per_turn`
  (unit) and `advisor_repeated_failure_emits_one_warning` (e2e, asserts exactly 1
  TUI warning line).

**Suggested fix.**
1. Keep the one-per-turn limit for the TUI line (it exists to avoid noise there).
2. Log every failed consultation (and the failure that triggered a retry, already
   at `debug`) at `warn` in the tracing log, with the failure kind from #5, the
   attempt number, and the transcript size. That makes the log a reliable record.
3. Update the two tests above to match: tracing logs every failure, TUI still
   shows one line.

**Watch out for.** The tracing log can be large and is full of upstream payloads;
keep the new log lines short and avoid logging the transcript.

---

## #8. Timeout is tight for huge prompts; the transcript is re-sent uncached each time (TIMEOUT REMOVED, CACHING OPEN)

> The operator chose to remove the advisor's own timeout instead of scaling it: `advisor.timeout_secs` now defaults to no limit, so only the global `request_timeout_secs` (600 s in the deployed config) bounds an advisor call. Suggested fix items 1 and 2 (scaling, shorter retry timeout) are therefore not needed. Item 3 (prompt caching) is still open.

**Plain English.** The advisor's default timeout is 45 seconds. A reasoning model
reading a 300k to 600k token transcript can be slow, and a timeout is just another
failure (now retried once, so a stalled advisor can take about twice the timeout
before the turn moves on). Every consultation also re-sends the full transcript
from scratch; the log shows `cached_tokens: 0` on a 576k-token advisor call, so the
operator pays for the whole prompt again each time.

**Evidence.** Default `timeout_secs` is 45 (`DEFAULT_ADVISOR_TIMEOUT_SECS` in
`src/config.rs`). A 296k-token advisor call at 09:54 took about 19 s; the 576k-token
one at 10:51 was larger. The input token counts for advisor calls in the log were
71k, 251k, 296k and 576k, with cached tokens 0 to 4.9k.

**Where.**
- `src/config.rs`: `DEFAULT_ADVISOR_TIMEOUT_SECS`, `advisor.timeout_secs`.
- `src/tools/advisor/tool.rs`: `advisor_timeout`, `consult`, `RETRY_DELAY`, and the
  transcript build (`build_transcript`, `MAX_TRANSCRIPT_CHARS`).

**Suggested fix.** #1 already cuts the prompt size, which is the main lever; check
first whether the timeout failures still occur after it. If they do:
1. Scale the effective timeout with the prompt size (for example a base plus a
   per-100k-token allowance), still clamped to the global `request_timeout`, as
   `advisor_timeout` already clamps today.
2. Consider whether the retry should reuse a shorter timeout, so a stalled advisor
   does not take twice as long.
3. Caching: put the stable part of the prompt first (the reviewer's system prompt
   and the executor's instructions) and the changing part last, so the upstream's
   prompt cache can hit. Check whether the upstream supports `prompt_cache_key`
   and, if so, reuse the executor's key for the advisor call. Verify by comparing
   `cached_tokens` in consecutive advisor calls in the log.

**Watch out for.** The transcript is rebuilt on every consultation and its early
items change when the oldest items are dropped (#1), which defeats caching for that
turn; if caching matters, make the trimming stable (drop from the same point each
time) rather than recomputing it.

---

## Things to know before starting

- **Rebuild on the Pi.** The Pi runs its own build, so none of the fixes so far
  take effect there until it is rebuilt and redeployed.
- **Built-in limits introduced so far** (constants in `src/tools/advisor/tool.rs`,
  documented in `proxy.json-guide.md`): `MAX_TRANSCRIPT_CHARS` (1,200,000),
  `RESULT_CAPS` (16,000 / 4,000 / 1,000), `ANY_STRING_CAP` (20,000),
  `CHARS_PER_TOKEN` (4), `RETRY_DELAY` (250 ms), `MAX_FAILED_CONSULTATIONS` (2).
  New config keys: `advisor.reasoning_max_tokens` (default 16384, env
  `ADVISOR_REASONING_MAX_TOKENS`); `advisor.max_tokens` default is now 4096.
- **Known gap from an earlier task, unrelated to the advisor:** a binary file
  served as `application/octet-stream` is still downloaded in full before the
  binary check when `web_fetch.max_bytes` is unset.
- **Not yet verified against the real model.** All fixes are covered by unit and
  end-to-end tests with a scripted fake upstream; none has been run against `beam`.
  After deploying, watch the log for `advisor transcript trimmed`,
  `advisor consultation failed; retrying once` (debug level) and any remaining
  `advisor consultation failed` warnings.
- Run `cargo test` and `cargo clippy --all-targets` before each commit. The one
  existing clippy warning at `tests/proxy_e2e.rs:81` is old and unrelated.
