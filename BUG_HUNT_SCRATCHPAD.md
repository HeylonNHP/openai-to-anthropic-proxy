# BUG HUNT SCRATCHPAD — openai-to-anthropic-proxy

> **Purpose:** durable record of a bug hunt so it survives conversation compaction.
> Fix the 10 bugs below **one at a time**, in order. Check off each row in
> [§ Progress Tracker](#progress-tracker) as you land it.
>
> **Status:** analysis complete, **no fixes started**.

---

## 0. Environment / provenance

| Item | Value |
|---|---|
| Repo | `/home/heylon/git/openai-to-anthropic-proxy` |
| Branch | `dev` |
| HEAD at time of analysis | `f53653b` — "fix(translate): inline `$ref` and budget strict tool schemas" |
| Worktree state | **clean** — `git diff` = 0 files, `git diff --cached` = 0 files. Only untracked: `.claude/worktrees/agent-*/`, `.idea/` |
| Date of analysis | 2026-11-14 (note: `f53653b` was pulled into the local clone at 15:49 on 2026-11-14) |
| Rust / toolchain | cargo; `cargo test --all-targets` → **333 passed, 0 failed, 0 ignored** |
| Lint state at HEAD | `cargo fmt --all -- --check` **FAILS** (14 diffs: `capabilities.rs`, `proxy.rs`, `stream.rs`, `translate.rs`, `tui/app.rs`, `tests/proxy_e2e.rs`); `cargo clippy --all-targets --all-features` → 5 warnings, 0 errors |
| Log archive | `/home/heylon/git/proxy.log.2026-11-12.zip` (161 MB) → 27 files, 2026-08-15 → 2026-11-12, 539 MB uncompressed |

### Re-extracting the logs (fresh shell)

```bash
mkdir -p /tmp/proxylogs && cd /tmp/proxylogs
unzip -o /home/heylon/git/proxy.log.2026-11-12.zip
# newest file: /tmp/proxylogs/proxy.log.2026-11-12  (18527 lines)
```

### Branch topology (from `git log --all`)

- `dev` = integration branch = HEAD. `master` is frozen at `bab6fbb` (2026-08-20).
- History is **duplicated** — two parallel lineages carry identical messages/dates with
  different hashes (e.g. `29a6f72`/`694f4b7`, `04ed5a8`/`5ae3872`, `4ec892d`/`930ee8b`).
  136 commits total; every branch is an ancestor of `dev`; 15 local
  `worktree-agent-*` branches point at `4a55f0c` and add **zero** unique commits.
- **Do not assume a commit hash from the log table below is the "real" one** — check both
  twins when `git log -S` misses.

### ⚠️ Two log-analysis traps that produce false positives

1. **The client embeds the proxy's own source code in its prompts.** Naive substring
   greps match log *text* that is really payload. Examples that look present but are
   actually **0** occurrences as real log lines:
   `falling back to default_model` (26 raw hits, all inside `sent_body=` payloads),
   `upstream SSE error; closing stream`, `failed to serialize stream event`,
   `dropping unknown user content block type`, `dangling reasoning_summary_text delta`.
   **Always anchor:** `grep -E '^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9:.]+Z +(WARN|INFO|ERROR|DEBUG) <module>: <msg>'`.
2. **tracing right-pads the level field** (` WARN`, ` INFO`, `DEBUG`). A regex of the
   form `^TS LEVEL` silently drops every WARN/INFO/ERROR line.

### ⚠️ "Stopped firing" ≠ "fixed"

Git forensics proved **all 12 log-observed symptoms are still live at HEAD**. Three of
them only stopped appearing on 2026-10-01 because the *client's tool set* changed, not
because of a code fix — the rules are still in `src/translate.rs`:

| Symptom | Rule still present at HEAD | Last fired |
|---|---|---|
| ``stripping `format` keyword`` | `translate.rs:779` | 2026-10-01 |
| ``dropping an unsupported `pattern``` | `translate.rs:923` | 2026-10-01 |
| ``dropping a keyword the Responses strict validator refuses`` | `translate.rs:891` | 2026-10-01 |

### Log ↔ commit alignment (proves the corpus tracks commits faithfully)

| Template | Introduced | First log | Last log |
|---|---|---|---|
| `repair: dropping optional property…` | `29a6f72` 2026-08-15 | 2026-08-17 | 2026-10-01 |
| ``dropping an unsupported `pattern``` | `478e1c4` 2026-09-21 | 2026-09-29 | 2026-10-01 |
| ``adding a permissive `items``` | `0ea9ec1` 2026-09-21 | 2026-09-29 | 2026-11-12 |
| `inbound_thinking=` in the `→ upstream` line | `7ed19db` 2026-10-13 | **2026-11-12 only** (309×) | 2026-11-12 |

⇒ The 2026-11-12 build is **≥ `7ed19db`**. No log predates the commit that introduced
its template, so no symptom in the corpus is an artefact of a newer build.

### Corpus-wide template census (2026-11-12 vs. all logs)

| # | Template | All logs | 2026-11-12 | First → last | Active at HEAD? |
|---|---|---|---|---|---|
| 1 | dropping unsupported block from assistant message | 80,302 | **10,467** | 08-16 → 11-12 | **YES** |
| 2 | extending schema `required` array … | 215,505 | **5,640** | 08-15 → 11-12 | YES (deliberate) |
| 3 | stripping `propertyNames` keys … | 8,508 | **376** | 08-15 → 11-12 | YES |
| 4 | adding a permissive `items` … | 842 | **376** | 09-29 → 11-12 | YES (rule added 09-21) |
| 5 | schema node missing `type` key; defaulting to `string` | 13,320 | **188** | 08-15 → 11-12 | YES |
| 6 | dropping stop_sequences … | 2,963 | **120** | 08-15 → 11-12 | YES |
| 7 | truncating metadata.user_id … | 8,728 | **309** | 08-15 → 11-12 | YES |
| 8 | web search citation extraction | 3,329 | 120 | 08-15 → 11-12 | informational — always `0/0` |
| 9 | OutputText part with annotations | 2,627 | 119 | 08-15 → 11-12 | informational — always `[]` |
| 10 | repair: dropping optional property … | 736 | 0 | 08-17 → 10-01 | YES (input-dependent) |
| 11 | handle_output_item_added | 10,966 | 260 | 08-15 → 11-12 | YES |
| 12 | ← upstream error / upstream_payload | 138 pairs | 2 | 08-16 → 11-12 | YES |

The translate module emits **exactly 12 distinct message templates**. No others exist.

### Runtime-side numbers worth remembering

- 309 upstream requests on 2026-11-12; 188 are "main" requests
  (`tools=48`, `inbound_thinking=Some("adaptive")`), 121 are sidecar requests
  (`tools=0`, `inbound_thinking=None`).
- Upstream errors across the whole corpus: 138 events = 69 failures logged twice
  (short + `target=upstream_payload`). 402 ×108, **400 ×16**, **404 ×11**, 502 ×2, 503 ×1.
  **No 429, no timeouts, no connection errors.**
- **ZERO** log lines at `ERROR` level in the entire corpus. Every failure is WARN or silent.
- `reasoning_tokens` is `0` in **120 out of 120** usage objects on 2026-11-12, despite
  60 real reasoning items.
- Largest logged outbound body: **1,601,482 bytes** (2026-08-23) — only ~30 % headroom
  under axum's 2 MB default limit.
- On 2026-11-12 the live alias map was `claude-sonnet-5-5 → deepseek-v4.1-flash:1017`
  and `claude-opus-5-5 → kimi-k3.1-pro`. **No plain `claude-sonnet-5` entry** → 404.

### Where the code lives (quick map)

| File | Lines | Role |
|---|---|---|
| `src/translate.rs` | 5103 | Anthropic ⇄ Responses request/response translation, `sanitize_tool_schema` |
| `src/stream.rs` | 2306 | SSE event translation |
| `src/proxy.rs` | 1332 | HTTP layer, upstream calls, error mapping |
| `src/config.rs` | 1292 | Config parsing, reasoning/effort maps, model aliases |
| `src/schema_refs.rs` | 798 | `$ref` resolution (new in `f53653b`) |
| `src/repair.rs` | 796 | Tool-call argument repair |
| `src/anthropic.rs` | 555 | Anthropic wire types |
| `src/responses.rs` | 513 | OpenAI Responses wire types |
| `src/capabilities.rs` | 567 | Learned "unsupported param" store |
| `src/error.rs` | 116 | Anthropic error envelope |

Test modules: `src/anthropic.rs:470`, `src/capabilities.rs:367`, `src/config.rs:679`,
`src/proxy.rs:1091`, `src/repair.rs:611`, `src/schema_refs.rs:431`, `src/stream.rs:1098`,
`src/translate.rs:1643`. Integration: `tests/proxy_e2e.rs` (26 tests, has a
`FakeUpstream` harness — see `start_fake_upstream()` at `tests/proxy_e2e.rs:124`).

---

## Progress Tracker

| # | Bug | Status | Commit | Notes |
|---|---|---|---|---|
| 1 | Thinking blocks dropped on the way back in | ⬜ TODO | | |
| 2 | `default_model` fallback is dead code → hard 404 | ⬜ TODO | | |
| 3 | Parallel tool calls → interleaved blocks → corrupted JSON | ⬜ TODO | | |
| 4 | Malformed upstream SSE event dropped wholesale | ⬜ TODO | | |
| 5 | Native `web_search` tool → 400 kills the whole request | ⬜ TODO | | |
| 6 | `required` extension → invented args → repair deletes them | ⬜ TODO | | |
| 7 | Streaming never forwards reasoning text; `reasoning_tokens` always 0 | ⬜ TODO | | |
| 8 | `stop_sequences` dropped → ~22 % malformed client output | ⬜ TODO | | |
| 9 | `metadata.user_id` truncated mid-JSON on 100 % of requests | ⬜ TODO | | |
| 10 | Capabilities false positive → `temperature` permanently disabled | ⬜ TODO | | |

---

## BUG 1 — The proxy throws away the thinking blocks it just produced

**Severity:** HIGH — destroys model context on every single turn.
**Files:** `src/anthropic.rs:143-154`, `src/translate.rs:1545-1549`.
**Status:** ❌ not fixed. Present since the initial commit `269f5ed` (2026-07-17).

### Plain English

The proxy asks the upstream model to reason, converts the upstream `Reasoning` item into
an Anthropic `thinking` block, and hands that block to the client. On the **next** turn
the client echoes the block back (that is how Anthropic's protocol works). The proxy
parses it into `ContentBlockParam` — which has **no `Thinking` variant** — so it lands in
the `#[serde(other)] Unknown` variant and is **silently discarded**.

Net effect: the model can never see any of its own prior reasoning. It re-derives
everything from scratch on every turn, and the thinking *signature* (which the client
uses to verify the block) is lost too. Extended thinking is effectively write-only.
The proxy is destroying the exact data it just went to the trouble of producing.

### Root cause

`src/anthropic.rs:141-154`:

```rust
/// Request-side content block. Tagged by `type`.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlockParam {
    Text(TextBlockParam),
    Image(ImageBlockParam),
    /// Tool invocation produced by the model in a prior turn.
    ToolUse(ToolUseBlockParam),
    /// Tool result returned to the model in response to a prior `tool_use`.
    ToolResult(ToolResultBlockParam),
    /// Unknown block types round-trip as raw JSON; the translator drops them
    /// when forwarding to OpenAI (with a tracing warning).
    #[serde(other)]
    Unknown,
}
```

No `Thinking` / `RedactedThinking` variant. `src/translate.rs:1538-1549`:

```rust
                    text.push_str(&t.text);
                    }
                    ContentBlockParam::ToolUse(tu) => { … }
                    ContentBlockParam::Image(_)
                    | ContentBlockParam::ToolResult(_)
                    | ContentBlockParam::Unknown => {
                        tracing::warn!("dropping unsupported block from assistant message");
                    }
```

The proxy *creates* these blocks itself:
- `src/stream.rs:597-611` — emits `ContentBlockStart { ContentBlockKind::Thinking { thinking:"", signature:"" } }`
  for every upstream `reasoning` item.
- `src/translate.rs:4291-4307` (non-streaming) — maps
  `OutputItem::Reasoning { summary, encrypted_content }` → `ResponseContentBlock::Thinking`.

Note the stale comment at `src/anthropic.rs:433-438` claiming *"Thinking blocks
(extended thinking). Not generated by the proxy"* — **false**.

### Evidence

Log (2026-11-12), **10,467 occurrences** — 59 % of all translate lines that day:

```
2026-11-12T12:27:08.538502Z  WARN openai_to_anthropic_proxy::translate: dropping unsupported block from assistant message
2026-11-12T12:27:35.260687Z  WARN openai_to_anthropic_proxy::translate: dropping unsupported block from assistant message
2026-11-12T13:53:25.485919Z  WARN openai_to_anthropic_proxy::translate: dropping unsupported block from assistant message
```

Quantitative proof the dropped blocks are the reasoning ones:
- On 2026-11-12 the upstream produced **60** `reasoning` items.
- The identity **`drops(request n) == 24 + (# upstream reasoning items emitted before request n)`**
  holds for **188 / 188** requests. (Offset 24 = assistant turns already in the resumed session's history.)
- The drop count is **monotonically non-decreasing across all 187 steps**, 24 → 84.
- The 188 dropping requests are *exactly* those with `inbound_thinking=Some("adaptive")` + `tools=48`;
  the 121 with `inbound_thinking=None`/`Some("disabled")` + `tools=0` drop **zero** blocks.
- Parsing every `sent_body=` payload in the corpus: input item types are only
  `message` / `function_call` / `function_call_output` — **never** a `reasoning` item.

```
2026-11-12T12:27:08.542301Z  INFO openai_to_anthropic_proxy::proxy: → upstream inbound_model=claude-sonnet-5-5 upstream_model=deepseek-v4.1-flash:1017 stream=true tools=48 inbound_thinking=Some("adaptive") inbound_effort=Some("medium") reasoning_effort=Some("medium")
```

### Fix sketch

1. Add `Thinking(ThinkingBlockParam)` and `RedactedThinking(RedactedThinkingBlockParam)`
   variants to `ContentBlockParam` (`src/anthropic.rs`). Anthropic's shape:
   `{"type":"thinking","thinking":"…","signature":"…"}` and
   `{"type":"redacted_thinking","data":"…"}`.
2. In `src/translate.rs` map them to the Responses equivalent. The Responses API has
   **no first-class reasoning item in `input`** for round-tripping, so the realistic
   options are:
   - a) emit them as a `reasoning` input item (Responses does accept `type:"reasoning"`
     items in `input` for multi-turn), or
   - b) fold the thinking text back into the assistant `message` content as an
     `output_text` part, or
   - c) at minimum, **stop silently dropping** — emit a WARN that names the block type
     and the number of characters lost, so the operator can see the leak.
   *Check the upstream actually accepts option (a) for the configured providers before
   committing to it; if not, (b) is the safe fallback.*
3. Decide policy for `redacted_thinking`: it cannot be re-sent verbatim to a non-Anthropic
   upstream (it is an encrypted Anthropic blob) — it should be dropped with a clear WARN,
   not silently.
4. Update the stale comment at `src/anthropic.rs:433-438`.

### Regression test plan

- Unit test in `src/translate.rs` tests module: feed an Anthropic request whose assistant
  message contains a `thinking` block → assert the outbound Responses `input` contains
  the reasoning (and that the WARN is **not** emitted).
- Round-trip test: `responses_to_anthropic` on a response containing a `Reasoning` item
  → assert a `thinking` block with non-empty `thinking` and `signature` → feed that back
  through `anthropic_to_responses` → assert nothing was dropped.

---

## BUG 2 — `default_model` fallback is dead code; unmapped model names hard-404

**Severity:** HIGH — kills the whole request.
**File:** `src/proxy.rs:670-712` (+ `src/proxy.rs:399-437` the fallback branch).
**Status:** ❌ not fixed. Fallback added `027f6c2`/`5001be1` (2026-07-18), never fired.

### Plain English

The config guide promises *"`default_model` is a safety net. If the upstream rejects a
model, the proxy retries once with this fallback and logs at WARN."* In 539 MB of logs
that WARN has fired **zero** times, while there were **11 real 404s**.

Why: the detector looks for the literal phrase `"model not found"`. The real gateway
says `model "claude-sonnet-5" not found. Try glm-5.3-flash:cloud.` — note the model name
sits *between* "model" and "not found", so the substring never matches. The body is also
a **nested** `{"error":{…}}` envelope with `code: null`, so the structured-code path
also fails. Result: the client gets a raw upstream 404 with no retry.

Compounding it: model mapping is an **exact** `BTreeMap` lookup with **no family or
prefix matching anywhere in the crate**. Every new Anthropic model name
(`claude-sonnet-5-5`, `claude-opus-5`, `claude-fable-5-1`, …) must be hand-added to
`proxy.json` or the request 404s.

### Root cause

`src/proxy.rs:670-700`:

```rust
fn is_model_not_supported(status: StatusCode, body: &str) -> bool {
    if !matches!(status.as_u16(), 400 | 404) {
        return false;
    }
    if let Some(code) = extract_model_error_code(body)
        && matches!(code.as_str(), "model_not_found" | "model_not_supported")
    {
        return true;
    }

    const PHRASES: &[&str] = &[
        "model_not_found",
        "model not found",
        "model not supported",
        "not supported for the selected provider",
        "unknown model",
    ];
    let lower = body.to_ascii_lowercase();
    PHRASES.iter().any(|p| lower.contains(p))
}
```

`extract_model_error_code` (`src/proxy.rs:702-720`) tries the **flat** Responses shape
(`ResponsesError`) then `{"error":{"code":…}}`. The real body has `code: null` → `None`.

The mapping side (`src/proxy.rs:163`) is `mappings_snapshot.resolve(&req.model)` →
`src/tui/runtime.rs:59-64`, an exact `BTreeMap` lookup, no normalisation.

### Evidence

Exact upstream body from the 2026-11-12 log:

```
← upstream error status=404 Not Found body={"error":{"message":"model \"claude-sonnet-5\" not found. Try glm-5.3-flash:cloud.","type":"not_found_error","param":null,"code":null}}
```

Corpus-wide 404s: `claude-opus-5` ×8, `claude-fable-5-1` ×2, `claude-sonnet-5` ×1.
Corpus-wide `proxy: upstream rejected model` (the fallback WARN) → **0**.

**Why the existing e2e test passes anyway:** `tests/proxy_e2e.rs:741`
(`model_not_supported_falls_back_to_default`) feeds a *flat* body containing the exact
phrase `not supported for the selected provider` — a shape the real gateway never sends.
That test is a false negative, not a passing guard.

### Fix sketch

1. Make `is_model_not_supported` understand the real nested envelope:
   - parse `{"error":{…}}` and check `error.type == "not_found_error"` / `error.message`
     contains `not found` / `does not exist` / `is not supported`;
   - treat `model "<name>" not found` as model-not-supported (regex or a looser
     "model" + "not found" co-occurrence check within a short window).
2. Consider extracting the suggested model from the message (`Try glm-5.3-flash:cloud.`)
   and honouring it before falling back to `default_model`.
3. Add a **family/prefix fallback** for unmapped inbound names, e.g.
   `claude-sonnet-*` → the configured sonnet alias, `claude-opus-*` → opus alias.
   Or at minimum **log a WARN when an inbound model is not in the alias map** so the
   operator knows to add it — currently it passes through completely silently.
4. **Fix the e2e test** `tests/proxy_e2e.rs:741` to use the real nested body shape, and
   add a case for `model "X" not found`.

### Regression test plan

- `tests/proxy_e2e.rs`: fake upstream returns 404 with the real nested body →
  assert the proxy retries with `default_model` and the client gets a 200.
- Unit test in `src/proxy.rs` tests: `is_model_not_supported(404, real_body) == true`
  and `extract_model_error_code(real_body) == None` (assert the current wrong answers
  first, then flip).
- Guard against false positives: a 400 that is *not* about the model must not trigger the
  fallback (there is already `unrelated_upstream_400_is_not_a_fallback_trigger` at
  `tests/proxy_e2e.rs:881` — keep it passing).

---

## BUG 3 — Parallel tool calls produce interleaved content blocks

**Severity:** HIGH — corrupts tool-call JSON, client rejects the call.
**File:** `src/stream.rs:566-595` (FunctionCall arm), `src/stream.rs:859-876`
(`close_open_tool_blocks`).
**Status:** ❌ not fixed.

### Plain English

When the model calls two tools in one turn, the proxy opens content block 0 for tool A
and content block 1 for tool B **without closing block 0**, then interleaves their
argument deltas. Anthropic's official API never does this — it streams blocks strictly
sequentially (start → deltas → stop, one block at a time).

Anthropic's TypeScript SDK (`MessageStream.ts:519`) attributes a `content_block_delta`
to the **last started** block, *not* to `event.index`:

```ts
case 'content_block_delta': { const content = messageSnapshot.content.at(-1)!; … }
```

So tool B's `input` arrives as **tool A's JSON glued onto tool B's JSON**. The client
gets invalid JSON → "Invalid tool parameters", and tool A's arguments are lost.

### Root cause

`src/stream.rs:566-595` — the code knows and does it anyway:

```rust
                // Close any open thinking block (a tool call
                // interleaves with reasoning on a different
                // output_index, but a single response can have
                // multiple parallel tool calls; we must NOT close
                // existing tool blocks here).
                self.close_open_thinking_blocks(events);
                // Allocate a fresh Anthropic index for this tool.
                let index = self.allocate_index();
                events.push(StreamEvent::ContentBlockStart {
                    index,
                    content_block: ContentBlockKind::ToolUse { … },
                });
                self.tool_blocks.insert(output_index, ToolBlockState { index, … });
```

`close_open_tool_blocks` is only called at `src/stream.rs:516`, `602`, `687`, `909` —
i.e. when a Message/Reasoning item arrives or the stream finalises.

### Evidence

Production, 2026-11-12, two `FunctionCall` items **233 µs apart**:

```
2026-11-12T12:41:05.261699Z DEBUG openai_to_anthropic_proxy::stream: handle_output_item_added output_index=0 item=FunctionCall { id: Some("fc_861799_0"), status: Some("in_progress"), call_id: "call_hiecwj78", name: "mcp_…
2026-11-12T12:41:05.261932Z DEBUG openai_to_anthropic_proxy::stream: handle_output_item_added output_index=1 item=FunctionCall { id: Some("fc_787007_1"), status: Some("in_progress"), call_id: "call_dx42kpf4", name: "Bash…
```

Wire order actually emitted (from a sub-agent probe):

```
content_block_start index=0 tool_use
content_block_start index=1 tool_use
content_block_delta index=0 input_json_delta {"n":0}
content_block_delta index=1 input_json_delta {"n":1}
content_block_stop index=0
content_block_stop index=1
```

Anthropic's API streams blocks sequentially — *"A series of content blocks, each of
which has content_block_start, any number of content_block_delta events, and a
content_block_stop event"* (platform.claude.com/docs/en/build-with-claude/streaming).

### Fix sketch

Buffer parallel tool calls and emit them **serially**: when a second tool block would
open while another is still open, first flush the open tool's accumulated arguments and
emit its `content_block_stop`, then start the next. The upstream's `output_index`
ordering must be preserved.

Design note: the upstream sends `output_item.added` for tool B *before* tool A's
argument deltas may have finished. So the proxy must **buffer** tool A's argument
deltas until it is safe to close A, or emit A's args + stop as soon as the upstream
signals A is complete (`output_item.done` for A), and only then start B's block.
This means the Anthropic-side block order may differ from the upstream `output_index`
order — that is fine and is what Anthropic's own API does.

Also fix the neighbouring defect found by the same audit: `self.tool_blocks.insert(...)`
**overwrites** on a duplicate `output_item.added` for the same `output_index`, orphaning
the earlier block (its `content_block_stop` is never sent) — `src/stream.rs:586-595`,
same for `thinking_blocks` at `src/stream.rs:612-618`.

### Regression test plan

- Unit test in `src/stream.rs`: feed two `output_item.added` FunctionCall events, then
  argument deltas for both → assert the emitted event sequence has **no two open
  blocks**: block 0 is stopped before block 1 starts, and each block's deltas are
  contiguous.
- E2E in `tests/proxy_e2e.rs`: fake upstream streams two parallel tool calls → assert the
  client receives two separate, individually valid JSON arguments.

---

## BUG 4 — A malformed upstream SSE event loses the entire turn

**Severity:** HIGH — silent total content loss; client sees a *successful* empty turn.
**Files:** `src/proxy.rs:866-878`, `src/responses.rs:415-474`.
**Status:** ❌ not fixed. Since `0695a2f`/`0d9149a` (2026-07-19).

### Plain English

`output_index`, `item_id` and `created_at` are **required** fields in the upstream event
structs. If the upstream sends an event without one, deserialisation fails and the proxy
throws the **whole event away** — all the text, all the tool arguments, gone, with only
a WARN. The client then receives `message_start → message_delta(stop_reason:end_turn) →
message_stop`, i.e. a completely normal-looking successful turn containing **nothing**.

Worse, if the lost event was `response.output_item.added`, the tool call vanishes
entirely — the `output_item.done` payload carries the *complete* arguments but
`maybe_finalize_tool_args` bails out because no block was opened
(`src/stream.rs:626-640`: `let Some(state) = self.tool_blocks.get_mut(&output_index) else { return; };`).

### Root cause

`src/proxy.rs:866-878`:

```rust
                        let event: responses::ResponsesStreamEvent =
                            match serde_json::from_str(&msg.data) {
                                Ok(e) => e,
                                Err(e) => {
                                    tracing::warn!(
                                        error = %e,
                                        event_kind = %msg.event,
                                        "dropping malformed upstream SSE event"
                                    );
                                    continue;
                                }
                            };
```

`src/responses.rs:415-474` — required (non-`Option`, no `#[serde(default)]`) fields:

```rust
    OutputItemAdded { output_index: u32, item: OutputItem },
    OutputItemDone { output_index: u32, item: OutputItem },
    OutputTextDelta { item_id: String, output_index: u32, … },
    …
```
plus `pub created_at: i64` at `src/responses.rs:265` on `ResponsesResponse`.

### Evidence

**186 such drops on 2026-08-24**: 172 × `missing field output_index`
(`output_text.delta` ×106, `function_call_arguments.delta` ×24,
`output_item.added/done` ×24, `created/in_progress` ×14).

### Fix sketch

1. Make the fragile fields `#[serde(default)]`:
   `output_index: Option<u32>` / `u32` with default, `item_id: Option<String>`,
   `created_at: i64` with default 0.
2. Fall back to **state keyed by `item_id`** when `output_index` is missing, and to
   "the single open block" when there is only one.
3. If an event still cannot be parsed, **do not `continue` silently** — emit an Anthropic
   `error` SSE event so the client learns the turn failed, instead of a fake success.
4. When `output_item.done` carries complete arguments for a tool whose `added` was
   missed, **open the block then** and emit the tool call.

### Regression test plan

- Unit test in `src/responses.rs`/`src/proxy.rs`: deserialize an SSE event lacking
  `output_index` → assert it parses.
- E2E in `tests/proxy_e2e.rs`: fake upstream sends `output_text.delta` without
  `output_index` → assert the text still reaches the client.
- E2E: fake upstream sends a deliberately malformed frame → assert the client gets an
  `error` event, **not** a successful empty message.

---

## BUG 5 — Native `web_search` tool is sent to upstreams that reject it

**Severity:** HIGH — 400 kills the whole conversation turn.
**File:** `src/translate.rs:149-160` (+ `src/capabilities.rs` for the learning gap).
**Status:** ❌ not fixed. Translation added `4b615e8`/`51a03bf` (2026-07-21).

### Plain English

Anthropic's `web_search` tool is converted into OpenAI's **native built-in** and sent to
*whatever* upstream the alias map resolved to — including DeepSeek and GLM, which don't
have it. The upstream answers `400 {"error":{"message":"the web_search tool is not
supported (ref: …)"}}` and the **entire request dies**, not just the web-search feature.

The proxy already has a self-healing mechanism for exactly this class of problem
(`capabilities.rs`: learn that a model rejects something, drop it, retry). But it can
only learn to drop **parameters** — `RequestParam` has only `{Temperature, TopP}`. There
is no way for it to learn to drop a **tool**. The capability store is
`ArcSwap<CapabilityRegistry>` in memory only (no disk persistence).

### Root cause

`src/translate.rs:149-160`:

```rust
                // Anthropic server-side web_search tool → OpenAI native web_search.
                // Server tools have no input_schema; we detect them by name.
                if t.name == "web_search" {
                    return Some(ToolDefinition::WebSearch(WebSearchTool {
                        kind: "web_search".to_string(),
                        max_uses: None,
                        search_context_size: Some("medium".to_string()),
                        user_location: None,
                        allowed_domains: None,
                        blocked_domains: None,
                    }));
                }
```

Emitted unconditionally for the resolved upstream model. No config knob, no capability
fact for tools.

### Evidence

**7 × HTTP 400** in the corpus:

```
the web_search tool is not supported (ref: 357a6936-f7da-4772-b20d-a93884642942)
the web_search tool is not supported (ref: 490f64b3-1e45-4671-aaef-8e7ff3a71f4b)
the web_search tool is not supported (ref: 88269ffa-1a93-41bc-a0f6-16c77cda0685)
```

Upstreams: `glm-5.3-flash` (09-13), `deepseek-v4.1-flash` (09-19, 09-29).
The outbound body contains `{"type":"web_search","search_context_size":"medium"}`.

It stopped firing after 09-29 only because **the client stopped sending the tool** — the
code is unchanged at HEAD.

**Secondary bug (same lines):** the check is on the tool *name* only and runs **before**
`resolve_local_refs(t.input_schema.clone()?)`. A **client-defined** tool named
`web_search` is therefore hijacked into the built-in and loses its schema and
`max_uses`. Verified: input `{"name":"web_search","input_schema":{…,"required":["q"]}}`
→ outbound `{"type":"web_search","search_context_size":"medium"}`.

**Third bug (same block):** other Anthropic **server tools** (`bash_20250124`,
`text_editor_20250124`, `computer_20250124`, `code_execution_20250522`) have no
`input_schema`, so `t.input_schema.clone()?` inside `filter_map` **skips them entirely** —
they vanish with no warning at all.

### Fix sketch

1. Prefer the **self-healing** route, which matches the existing design: on a 400 whose
   message matches `the web_search tool is not supported`, drop the `WebSearch` tool from
   `outbound.tools`, retry once, and record "this upstream model has no native
   web_search" in the capability store (keyed by **upstream** model).
   → requires adding a tool-level dimension to `capabilities.rs`.
2. Or a config allowlist mirroring the existing `prompt_caching.models` pattern:
   `web_search.models` — anything not listed gets the tool stripped **with a WARN**.
3. Only treat it as the native built-in when the client tool has **no `input_schema`**
   (that is the actual signature of an Anthropic server tool). A client tool named
   `web_search` *with* a schema must stay a function tool.
4. Handle the other server tools (`bash_20250124` etc.) explicitly — strip them with a
   WARN instead of dropping them silently via `?`.

### Regression test plan

- Unit in `src/translate.rs`: client tool `web_search` **with** `input_schema` → assert
  it stays a `Function`, not a native `WebSearch`.
- Unit: client tool `web_search` **without** `input_schema` → assert it becomes
  `WebSearch` only when the upstream allows it (per the new allowlist).
- E2E in `tests/proxy_e2e.rs`: fake upstream returns the 400 →
  assert the proxy retries without the tool and the client gets a 200.
- Unit: `bash_20250124` / `code_execution_20250522` → assert a WARN is logged (not silent).

---

## BUG 6 — `required` extension forces optional params mandatory; repair then deletes the invented value

**Severity:** MEDIUM-HIGH — silent semantic drift on every request.
**Files:** `src/translate.rs:1263-1300` (`reconcile_required`),
`src/translate.rs:993-995` + `1033-1113` (`make_nullable`), `src/repair.rs:437-455`.
**Status:** ❌ not fixed. Rule added `0695a2f`/`0d9149a` (2026-07-19).

### Plain English

The OpenAI Responses **strict** validator rejects any schema whose `required` array is
not a superset of its `properties` keys. So the proxy adds every optional property to
`required`. The model is now *obliged* to emit `model`, `effort`, `isolation`,
`description`, `timeout`, `run_in_background`, `limit`, `offset`, `pages`… and invents
values the client never asked for.

Then on the way back, `repair` validates the tool call against the **original** schema
and **deletes** the invented value (736 times historically: `pages` ×608, `args` ×44,
`ws` ×31, `isolation` ×14, `run_in_background` ×11). Net result: the parameter is
neither defaulted by the client nor meaningful. The client's intent is lost, and the
model burned tokens hallucinating a value.

### Root cause

`src/translate.rs:1263-1299`:

```rust
fn reconcile_required(map: &mut serde_json::Map<String, Value>) {
    let prop_keys: Vec<String> = map.get("properties").and_then(|v| v.as_object())
        .map(|o| o.keys().cloned().collect()).unwrap_or_default();
    if prop_keys.is_empty() { return; }
    if !map.contains_key("required") {
        let req: Vec<Value> = prop_keys.iter().map(|k| Value::String(k.clone())).collect();
        map.insert("required".to_owned(), Value::Array(req));
        return;
    }
    if let Some(Value::Array(req)) = map.get_mut("required") {
        let missing: Vec<Value> = prop_keys.iter()
            .filter(|k| !req.iter().any(|v| v.as_str() == Some(k.as_str())))
            .map(|k| Value::String(k.clone())).collect();
        if !missing.is_empty() {
            let names: Vec<&str> = missing.iter().filter_map(|v| v.as_str()).collect();
            tracing::warn!(
                missing = ?names,
                "extending schema `required` array with keys present in `properties` but missing from the client-supplied `required`; …"
            );
            for m in missing { req.push(m); }
        }
    }
}
```

The mitigating encoding is `make_nullable` (`src/translate.rs:1033-1113`), applied at
`src/translate.rs:993-995` to properties the original schema did not require: it turns
`{"type":"string"}` into `{"type":["string","null"]}` so the model can signal "omitted"
with `null`.

**But `make_nullable` never touches `enum`/`const`.** `enum`/`const` apply to *all*
types, so `null` is rejected by them (unlike `pattern`/`minLength`/`minimum`, which are
type-scoped and do admit `null`). Verified: client schema
`{"contentType":{"type":"string","enum":["markdown","html"]}}` → wire schema
`{"enum":["markdown","html"],"type":["string","null"]}` — `null in enum` is false. Yet
`reconcile_required` puts `contentType` in `required`, so the model is *ordered* to emit
it and *cannot* signal "omitted" → it fabricates `"markdown"`, and `repair` keeps it
(the value is in the enum) and ships it to the client, which receives a parameter it
never asked for.

### Evidence

```
2026-11-12T12:27:08.538968Z  WARN openai_to_anthropic_proxy::translate: extending schema `required` array with keys present in `properties` but missing from the client-supplied `required`; the Responses API strict validator rejects any `required` that is a strict subset of `properties` keys. See sanitize_tool_schema doc comment in src/translate.rs. missing=["effort", "isolation", "model", "subagent_type"]
```

- 2026-11-12: **5,640 events / 13,160 keys across 188 requests ≈ 70 keys per request.**
  28 distinct `missing=[…]` shapes. Top names: `tabId` (940); `contentType`,
  `description`, `level`, `trigger`, `action_summary` (376 each); then `effort`,
  `isolation`, `model`, `subagent_type`, `timeout`, `run_in_background`,
  `dangerouslyDisableSandbox`, `limit`, `offset`, `pages`, … (188 each).
- All-logs top names: `rootFolder` **66,720**, `limit` 22,079, `timeout` 15,855,
  `contentFormat` 12,040, `contentType` 10,146, `description` 9,216, `offset` 8,579.
- `repair: dropping optional property that violates the original schema` — 736 events
  (0 on 11-12; input-dependent). One historical failure to fix:
  `tool input still violates the original schema after repair tool=Bash violation=input: missing required property 'command'`.

### Fix sketch

1. Make `make_nullable` **enum/const-aware**: for an optional property with `enum`/`const`,
   either (a) extend the enum with `null`, or (b) drop the `enum`/`const` and keep
   `["<type>","null"]`, or (c) keep the property out of `required` and rely on the
   upstream's `strict:false` for that tool. Option (c) is cleanest if the upstream allows
   `strict: false` for that tool — note the repo already demotes heavy tools to
   `strict:false` via the budget logic (`src/translate.rs:306-345`).
2. Audit `repair` so that a value the model was *forced* to invent (i.e. a key that was
   only in `required` because of `reconcile_required`) is **stripped on the way back**
   rather than shipped. Today `repair` strips it only when it *violates* the original
   schema — an invented value that happens to be schema-valid survives.
3. Consider tracking, per tool, which keys were force-added, so `repair` can remove
   exactly those and nothing else.

### Regression test plan

- Unit in `src/translate.rs`: optional property with `enum` → assert the wire schema
  either admits `null` or the property is not forced into `required`.
- Unit in `src/repair.rs`: a tool call containing a force-added key with a schema-valid
  invented value → assert the key is removed from the delivered `input`.
- E2E: Claude Code-shaped request with 48 tools → assert the client receives tool calls
  whose arguments contain **only** the properties the client declared as required.

---

## BUG 7 — Streaming never forwards reasoning text; `reasoning_tokens` always 0

**Severity:** MEDIUM-HIGH — thinking is invisible in the UI; context accounting is wrong.
**Files:** `src/stream.rs:597-618` (block start), `src/stream.rs:795-815` (summary delta),
`src/stream.rs:639-652` (signature), `src/translate.rs:4291-4307` (non-streaming path).
**Status:** ❌ not fixed.

### Plain English

Two separate defects:

**(a)** `item.summary` is **never read** anywhere in `stream.rs` — the only mentions of
`summary` are a log message and test helpers. So every upstream reasoning item becomes
`content_block_start{thinking:"", signature:""}` immediately followed by
`content_block_stop`. Claude Code renders a blank "Thinking" box every turn, and the
model's reasoning is invisible.

The **non-streaming** path does the opposite (`src/translate.rs:4291-4307` joins the
summary into a real thinking block), so the *same upstream response* yields a full
thinking block non-streaming and an empty one streaming. That's a straight parity bug.

**(b)** `reasoning_tokens` is `0` in **120 out of 120** usage objects on 2026-11-12,
despite 60 real reasoning items with real summaries and ~700-byte signatures. Claude Code
therefore under-counts context.

A live capture from the real upstream (reasoning effort = high, no `reasoning.summary`
requested) showed the upstream emits `output_item.added {type:"reasoning", summary:[]}`,
**zero** `reasoning_summary_text.delta` events, and
`output_item.done {type:"reasoning", summary:[], encrypted_content:"gAAAA…"}`.
So for this deployment the reasoning text is simply **not available on the wire** — the
fix for (a) is to carry `encrypted_content` as the signature and, when the upstream
supplies a summary, emit it as `thinking_delta`.

### Root cause

`src/stream.rs:597-611`:

```rust
            OutputItem::Reasoning { .. } => {
                // Close any open text block.
                if let Some(text) = self.text_block.take() { … }
                self.close_open_tool_blocks(events);
                self.close_open_thinking_blocks(events);
                let index = self.allocate_index();
                events.push(StreamEvent::ContentBlockStart {
                    index,
                    content_block: ContentBlockKind::Thinking {
                        thinking: String::new(),
                        signature: String::new(),
                    },
                });
```

`src/stream.rs:639-652` — the signature is captured only **if the block is still open**:

```rust
                if self.thinking_blocks.contains_key(&output_index) && !encrypted_content.is_empty()
                {
                    // … the Anthropic Thinking block has no signature-delta event in
                    // the current wire format, and the client renders the block based
                    // on its `thinking` text.
                    if let Some(state) = self.thinking_blocks.get_mut(&output_index) {
                        state.signature = encrypted_content.clone();
                    }
                }
```

But `src/stream.rs:575`/`603` close the thinking blocks as soon as another item is added,
so if `output_item.done` arrives after the next `output_item.added`, the signature is
silently lost and `close_open_thinking_blocks` (`src/stream.rs:879-900`) never emits a
`signature_delta`.

### Evidence

```
2026-11-12T12:27:08.538502Z  … (60 Reasoning items logged that day, all with `summary: []`)
handle_output_item_added output_index=0 item=Reasoning { id: Some("rs_…"), summary: [], encrypted_c…
$ grep -o '"reasoning_tokens":[0-9]*' proxy.log.2026-11-12 | sort | uniq -c
    120 "reasoning_tokens":0
```

### Fix sketch

1. Read `item.summary` in the streaming path and emit `ContentDelta::ThinkingDelta` for
   each `ReasoningSummaryText` — matching the non-streaming join in
   `src/translate.rs:4291-4307`.
2. Request a summary from the upstream: the Responses API takes
   `reasoning: { effort, summary: "auto"|"concise"|"detailed" }`
   (`src/responses.rs:151-157` already models `summary: Option<String>`). Set it when the
   client asked for thinking.
3. Emit the signature as a `SignatureDelta` **before** the `content_block_stop`. Note the
   comment at `src/stream.rs:645-648` is out of date — Anthropic's wire format *does*
   have `signature_delta`, and `close_open_thinking_blocks` already emits one.
4. Don't close a thinking block until its `output_item.done` has arrived (or at least
   keep the signature pending).
5. For `reasoning_tokens`: map the upstream's reasoning token count into
   `usage.output_tokens` / the Anthropic `usage` block. Check whether the upstream even
   reports it — if it reports `0` while emitting reasoning, the proxy cannot invent it;
   in that case, at least surface it in the TUI rather than reporting a wrong total.

### Regression test plan

- Unit in `src/stream.rs`: feed a `Reasoning` item **with** a non-empty `summary` →
  assert a `thinking_delta` carrying the text is emitted.
- Unit: feed `reasoning_summary_text.delta` events → assert they are forwarded (there is
  a "dangling reasoning_summary_text delta" WARN at `src/stream.rs:805` — assert it is
  **not** hit in the happy path).
- Parity test: same upstream payload through the streaming and non-streaming paths →
  assert the thinking text is identical.

---

## BUG 8 — `stop_sequences` discarded → ~22 % of client-facing output is malformed

**Severity:** MEDIUM — client-side parse failures.
**File:** `src/translate.rs:198-208`.
**Status:** ❌ not fixed. Added `31716b0`/`8850342` (2026-07-18) as a deliberate drop.

### Plain English

The Responses API has **no `stop` parameter**, so the client's stop sequences are
dropped with a warning. Those stop sequences were the client's *only* guard against the
model running past its closing tag. Measured against the upstream's actual output:
**744 of 3,328 responses (22.4 %) came back malformed** — 690 as `<severity>5` with the
closing tag missing, 48 as `ity>5</severity>` with the *leading* characters missing,
5 as `<block>no` — all tagged `status: "completed"`. Two of these landed on 2026-11-12.

The malformed text is hard evidence in the logs; whether the client's parser choked on
it is not directly visible (inference).

### Root cause

`src/translate.rs:198-208`:

```rust
    // Reasoning models don't support stop sequences. Responses has no
    // `stop` field at all; warn so the configuration loss is visible if
    // a future client sets it.
    if let Some(stop) = req.stop_sequences.as_ref()
        && !stop.is_empty()
    {
        tracing::warn!(
            stop_sequences = ?stop,
            "dropping stop_sequences; Responses API does not support a stop parameter"
        );
    }
```

`ResponsesRequest` has **no `stop` field at all**.

### Evidence

```
2026-11-12T12:27:30.003794Z  WARN openai_to_anthropic_proxy::translate: dropping stop_sequences; Responses API does not support a stop parameter stop_sequences=["</severity>"]
2026-11-12T12:27:39.663735Z  WARN openai_to_anthropic_proxy::translate: dropping stop_sequences; Responses API does not support a stop parameter stop_sequences=["</block>"]
```

Corpus: 2,963 drops. Values are **always exactly one stop sequence per request**:
`["</severity>"]` 2,697 (2 on 11-12); `["</block>"]` 266 (**118** on 11-12).
The affected requests are identifiable: the non-streaming `tools=0` requests whose
upstream `instructions` begin *"You are a security monitor for autonomous AI coding agents."*

Measured output on 11-12 (119 upstream output texts): **116 × `<block>no</block>`**
(17 chars, correct), **2 × `<block>no`** (9 chars — closing tag MISSING),
1 × `<severity>6</severity>`.

### Fix sketch

The Responses API has no `stop`, so this cannot be forwarded as-is. Options:
1. **Post-process the response text**: truncate at the first occurrence of any of the
   client's stop sequences. This exactly reproduces the semantics the client wanted and
   is a small, well-scoped change in the response path (both streaming and non-streaming).
   Watch out for stop sequences that are prefixes of legitimate content.
2. Use `instructions` / a system-prompt hint — weaker, not equivalent.
3. At minimum, keep the WARN (already there) and consider surfacing it to the client.

Option 1 is the only one that actually restores the client's contract.

### Regression test plan

- Unit in `src/translate.rs`: non-streaming response text `"<block>no"` with
  `stop_sequences: ["</block>"]` → assert the delivered text is truncated to `<block>no`.
- Unit in `src/stream.rs`: same for the streaming path (the truncation must be applied to
  the accumulated text, and the `message_delta` must carry the truncated length).
- E2E in `tests/proxy_e2e.rs`: fake upstream returns text containing the stop sequence.

---

## BUG 9 — `metadata.user_id` truncated mid-JSON on 100 % of requests

**Severity:** MEDIUM — malformed payload on every request; attribution collapse.
**File:** `src/translate.rs:60-88`.
**Status:** ❌ not fixed. Added `0695a2f`/`0d9149a` (2026-07-19).

### Plain English

Every request's `user` field is cut to 64 bytes **in the middle of the client's JSON
blob**. The log shows `user_id_len=150 max=64` every single time — 309 times on
2026-11-12, 8,728 times historically. The sent value literally ends mid-`device_id`
hex with no closing brace, so the upstream receives **syntactically invalid JSON** in a
field it is meant to parse. All per-session attribution collapses into one shared
64-byte prefix.

The code's **own doc comment already proposes SHA-256 hashing as the fix** — it was
never implemented.

### Root cause

`src/translate.rs:60-88`:

```rust
const USER_ID_MAX_LEN: usize = 64;

fn truncate_user_id(s: String) -> String {
    if s.len() <= USER_ID_MAX_LEN {
        s
    } else {
        tracing::warn!(
            user_id_len = s.len(),
            max = USER_ID_MAX_LEN,
            "truncating metadata.user_id to fit Responses API user-field limit; \
             see USER_ID_MAX_LEN doc comment in src/translate.rs for rationale and \
             alternatives (e.g. SHA-256 hashing)",
        );
        let cutoff = (0..=USER_ID_MAX_LEN)
            .rev()
            .find(|&idx| s.is_char_boundary(idx))
            .expect("0 is always a UTF-8 character boundary");
        s[..cutoff].to_owned()
    }
}
```

Note the doc comment says the truncation "preserves the leading prefix" — true, but the
leading prefix of a *JSON blob* is not valid JSON.

### Evidence

```
2026-11-12T12:27:08.540918Z  WARN openai_to_anthropic_proxy::translate: truncating metadata.user_id to fit Responses API user-field limit; see USER_ID_MAX_LEN doc comment in src/translate.rs for rationale and alternatives (e.g. SHA-256 hashing) user_id_len=150 max=64
```

The actual sent value (from an upstream payload in the corpus):

```
"user":"{\"device_id\":\"b03136f58528a51a61cde940d1c9da0c2a376220ff1214d0ff
```

— cut mid-`device_id`, unterminated string, no closing brace; everything after
`device_id` (session id, org id …) is gone.

Only one length value (`150`) appears in the entire corpus.

### Fix sketch

Implement what the doc comment already says: **SHA-256 hash** the user id (e.g. hex, 64
chars, or a truncated base64) instead of cutting it. That preserves *uniqueness* within
64 bytes and yields valid JSON.

Alternative: parse the client blob as JSON and keep only a hash of the identifying
fields. If the blob must stay human-readable, at minimum truncate **at a JSON-safe
boundary** (e.g. keep a valid JSON prefix such as `{"device_id":"<first 32 hex>"}`).

### Regression test plan

- Unit in `src/translate.rs`: a 150-byte JSON `user_id` → assert the outbound `user`
  field is **valid JSON** and ≤ 64 bytes, and that two different ids produce two
  different values (no collision).
- Unit: a non-ASCII `user_id` → assert no panic and a char-boundary-safe result.

---

## BUG 10 — Capabilities false positive permanently disables `temperature` / `top_p`

**Severity:** MEDIUM — silent, permanent, process-wide quality regression.
**File:** `src/capabilities.rs:203-220` (+ `src/proxy.rs:352-398`).
**Status:** ❌ not fixed. Feature added `e2a15b9` (2026-09-18).

### Plain English

The "learn which parameters this upstream rejects" feature matches on
`code.contains("unsupported")`. OpenAI's *value* error code is **`unsupported_value`** —
e.g. `"Unsupported value: 'temperature' does not support 5 with this model. Only the
default (1) is supported."` That matches, so the proxy permanently strips `temperature`
from **every future request to that model**, for every user of the proxy, until
restart or a TUI `c` keypress.

The upstream error *names* the accepted values — the proxy ignores them. The store's
own doc comment admits the risk: *"A false positive … would silently strip a parameter
the model actually supports."*

### Root cause

`src/capabilities.rs:203-220`:

```rust
    /// Does the body carry an "unsupported"-flavoured signal?
    fn is_unsupported_signal(&self) -> bool {
        let code = self.code.to_ascii_lowercase();
        if code.contains("unsupported") || code.contains("unknown") || code.contains("unrecognized")
        {
            return true;
        }
        let hay = self.haystack();
        SIGNAL_PHRASES.iter().any(|phrase| hay.contains(phrase))
    }

    /// Does the body name this specific parameter?
    fn names(&self, param: RequestParam) -> bool {
        let structured = self.param.to_ascii_lowercase();
        if !structured.is_empty() && param.aliases().contains(&structured.as_str()) {
            return true;
        }
        let hay = self.haystack();
        param.aliases().iter().any(|alias| hay.contains(alias))
    }
```

`code.contains("unsupported")` matches `unsupported_value`, `unsupported_parameter`,
`unsupported_value` etc. indiscriminately — it cannot distinguish *"this parameter is
not implemented"* from *"this parameter is implemented but this value isn't allowed"*.

Chain: `src/proxy.rs:375-398` → `record_unsupported(&outbound.model, param)` →
`src/capabilities.rs:318-330` stores it → `apply_known` (`src/capabilities.rs:337-343`)
then strips `temperature` from every later request to that model.

The latch is **one-way for the process lifetime** (`src/capabilities.rs:32-40`). The
store is `ArcSwap<CapabilityRegistry>` — **in-memory only, no disk persistence** (so
the blast radius is one process; the TUI's separate `MappingsStore` at
`src/tui/app.rs:610-622` *is* correctly persisted via temp-file + atomic rename).

### Fix sketch

1. Tighten `is_unsupported_signal`: match **exact** codes / code prefixes, not
   `contains`. E.g. accept `unsupported_parameter`, `unknown_parameter`,
   `unrecognized_parameter`, `not_supported` — and explicitly **reject**
   `unsupported_value` / `invalid_value` / `invalid_request_error` (those are *value*
   errors, not *parameter* errors).
2. Distinguish the two cases by looking at the upstream's own error shape: OpenAI's
   `invalid_value` errors carry `param` + the accepted set in the message. If the message
   says *"does not support 5"* (a value) rather than *"is not supported"* (the
   parameter), do **not** latch.
3. When the error *does* name the accepted values, **adapt** rather than drop: clamp
   `temperature` to the allowed set instead of removing it entirely.
4. Consider making the latch **decay** (e.g. re-probe after N minutes) so a transient
   upstream error can't permanently degrade the model.
5. Consider persisting the capability store so the learning survives restarts — but
   only *after* the false-positive rate is fixed.

### Regression test plan

- Unit in `src/capabilities.rs`: `unsupported_parameter(400, {"error":{"message":"Unsupported value: 'temperature' does not support 5 with this model. Only the default (1) is supported.","param":"temperature","code":"unsupported_value"}})`
  → assert `None` (i.e. **do not** latch). The sub-agent's probe confirmed the current
  answer is `Some(Temperature)`.
- Unit: a genuine `unsupported_parameter` error → assert it **does** latch.
- E2E in `tests/proxy_e2e.rs`: fake upstream rejects `temperature` once with
  `unsupported_value` → assert the *next* request still carries `temperature`.

---

## Appendix A — Other real bugs found (below the top 10; fix later)

| Bug | Location | Why it matters |
|---|---|---|
| axum's default **2 MB body limit** is never raised | `src/proxy.rs:136` (`body: Bytes`), router at `src/proxy.rs:124-126` | Bodies >2 MB get a plain-text 413, **not** an Anthropic error envelope. Largest logged outbound body was 1,601,482 bytes — only ~30 % headroom. Fix: `.layer(axum::extract::DefaultBodyLimit::disable())` or `max(...)` on `/v1/messages`. |
| `propertyNames` stripping deletes a client parameter literally named `propertyNames` | `src/translate.rs:1301-1320` | The walk can't distinguish a schema node from a `properties` map, so it deletes the **property definition** and leaves a dangling `required` entry (`reconcile_required` only ever *adds*). One bad parameter name breaks the whole request (strict validator rejects a required key with no property). The same walk corrupts `enum`/`const`/`default` **values**. Contrast `src/schema_refs.rs:78-83`, which protects `DATA_KEYWORDS`. |
| `reasoning.effort = "xhigh"` → permanent 400 | `src/config.rs:397-450`, `src/responses.rs:166-176` | 7 × 400 `invalid reasoning value: "xhigh"` on 2026-08-29. The shipped `proxy.json` `effort_map` maps `xhigh → xhigh`. `RequestParam` has no `ReasoningEffort` variant, so the learning path can't help. The upstream error *names* the accepted set — the proxy ignores it. |
| Upstream error body pasted raw into the client's error message | `src/proxy.rs:993-1010`, `src/responses.rs:388-398` | Only the **flat** Responses envelope is modelled (`ResponsesError { message: String, … }`); real gateways send nested `{"error":{…}}`, so the parse always fails and the raw JSON is double-encoded into `error.message`. Leaks provider catalogue hints and request refs. Fix: reuse `capabilities::ErrorEnvelope::parse`, which already handles both. |
| `retry-after` and `request-id` headers dropped on every error | `src/proxy.rs:353-360`, `src/error.rs:73-86` | A 429 reaches the client with no backoff hint; no response ever carries Anthropic's `request-id`. Fix: add `headers: HeaderMap` to `AppError::Upstream`. |
| Upstream `Content-Type` forwarded verbatim while the body is always SSE | `src/proxy.rs:471-499` | A gateway answering `application/json` makes Claude Code's SSE parser fail. Always send `text/event-stream`. |
| `status: "failed"` upstream → HTTP 200 with `end_turn` | `src/proxy.rs:538`, `src/proxy.rs:584`, `src/translate.rs:3995-4009` | The upstream failed; the client gets `200 OK` and a normal assistant message whose text begins `[error]`. The **streaming** path does the right thing (`proxy.rs:838-852`) — the two paths disagree. |
| `request_timeout` is a **total** deadline covering the streaming body | `src/main.rs:155-160` | reqwest 0.12.28 `TotalTimeoutBody` does not reset per chunk. A stream running >10 min (default 600 s) is aborted mid-flight. Use `read_timeout` for streaming. |
| Upstream URL is naive string concatenation; no provider support | `src/proxy.rs:316-320`, `src/config.rs:246-250` | `https://api.openai.com/v1` + `/v1/responses` → `…/v1/v1/responses`. `https://api.openai.com` + `v1/responses` → malformed host → DNS failure. `upstream_path` is never validated to start with `/`. Azure needs an `api-key:` header but auth is hard-coded bearer (`proxy.rs:266`) — there is no per-provider auth option. |
| `top_k` and `thinking.budget_tokens` parsed then silently dropped, **no log line** | `src/anthropic.rs:48`, `src/anthropic.rs:95` | Neither field is read anywhere in the crate. Unlike `stop_sequences` (which warns), these produce nothing. |
| System prompt `cache_control` dropped, contradicting the docs | `src/config.rs:64` vs `src/translate.rs:245-255`, `src/responses.rs:35` | Docs claim `cache_control` on user **and** system blocks becomes a `prompt_cache_breakpoint`; `instructions` is a plain `Option<String>` — there's no field to carry it. Only user blocks get breakpoints (`translate.rs:1361-1364`). The system prompt (usually the largest stable prefix) is never cached. |
| `LAST_SENT_BODY` holds the **wrong** body after a fallback or corrective retry | `src/proxy.rs:238-247`, `358`, `399-437`, `546-556` | `body_json` is captured once, *before* `handle_messages_inner`; the fallback replaces `outbound` and the corrective retry mutates `outbound.input`, but the log line still shows the original. Defeats the "postmortem debugging is one log line away" purpose exactly when it matters. |
| `allOf` flattening keeps only `properties`/`required`/`description` | `src/translate.rs:604-694` | A nested `allOf` branch is discarded entirely (verified: the node degenerates into a nullable string). Also `patternProperties`/`additionalProperties` are dropped, and branch `required` arrays are **unioned** — union ≠ intersection, so the merged node is *less* strict than the original. |
| Unresolvable / recursive `$ref` stays on the wire | `src/schema_refs.rs:121-155`, `268-312` | `resolve_local_refs` deliberately leaves the schema untouched when it can't resolve faithfully; the sanitizer treats the `$ref` node as opaque, so `$ref` **and** `$defs` go out on the wire. One MCP tool with an unresolvable schema 400s the **entire** request — the exact failure mode `f53653b` set out to eliminate. (Listed as known follow-up #2 in `f53653b`'s own commit message.) |
| `MAX_EXPANSIONS` counts `$ref` **sites**, not resulting nodes | `src/schema_refs.rs:56` | Does not bound the emitted size — a 12-level diamond of `$ref`s produced a **520 KB** single-tool schema in a probe. |
| `merge_siblings` keeps the *target's* value on a conflicting keyword | `src/schema_refs.rs:353-386` | Silently discards the sibling's `required` (verified: a sibling `required:["sibling_only"]` vanished). |
| `make_nullable`'s fallback branch deletes the node's own `properties`/`required` | `src/translate.rs:1066-1113` | When a node is typeless *and* has no null branch, its object shape is deleted rather than moved into a branch. |
| `anyOf`/`oneOf` branch with `required` but no `properties` → unsatisfiable | `src/translate.rs:728-742` | `force_object` inserts `properties:{}` + `additionalProperties:false` while `required` still demands a key → no value can satisfy it. `repair` then reports "matches no branch of 'anyOf'" forever. |
| Typeless node with a non-string `enum`/`const` → unsatisfiable | `src/translate.rs:823-838`, `is_object_schema` at `1202-1210` | `is_object_schema` only inspects `type` and `properties`; a node with `enum:[1,2,3]` gets `type:"string"` → no JSON value satisfies it. |
| `repair` can't undo a `null` array element from the permissive `items` union | `src/translate.rs:1239-1259`, `src/repair.rs:466-472` | The permissive `items` union includes `"null"`, so the model may legitimately emit one; `repair`'s `Value::Null` arm falls through `_ => {}`. |
| Anthropic error `type` strings don't match Anthropic's vocabulary | `src/error.rs:44-53` | 403 → `authentication_error` (Anthropic: `permission_error`); 413 → `api_error` (`request_too_large`); 529/503 → `api_error` (`overloaded_error`); 408 → `api_error` (`timeout_error`). |
| `log_to_disk: true` writes full prompts + responses to disk | `src/proxy.rs:286-291`, `src/proxy.rs:359-365` | That is the 539 MB corpus — full conversations, PII included. Default filter `main.rs:216` = `"info,openai_to_anthropic_proxy=debug"` enables both. **No API key is ever leaked** (verified). |
| Config path is CWD-relative with no `--config` flag | `src/config.rs:207`, `src/main.rs:41` | Started from another directory, the proxy starts happily and **silently ignores `proxy.json`** (alias map, reasoning tables, prompt caching). |
| `Config::upstream_model_for` / `Config::default_model` are dead code | `src/config.rs:472`, `src/config.rs:485` | The proxy uses `RuntimeMappings::resolve` / `mappings_snapshot.default_model()` instead. Harmless today; a future edit to the config accessors would silently do nothing. |
| `emit_text_delta` ignores `output_index` | `src/stream.rs:679` | Two message items in one response merge their text into one block; a stray text delta after a tool call silently opens a brand-new text block. |
| `StopReason::Refusal` unreachable in streaming | `src/stream.rs:261-273`, `1071-1084` | `RefusalDelta` is surfaced as ordinary text and every status maps to `EndTurn`. The non-streaming path prefixes `"[refusal] "` instead of setting the stop reason. |
| `message_start.usage.input_tokens` is always 0 | `src/stream.rs:437-457`, `459-497` | Usage only arrives on `response.completed`, long after `message_start`. The placeholder `{input_tokens: 0, output_tokens: 1}` is emitted instead. |
| Failed/truncated streams logged to the operator as "completed" | `src/proxy.rs:624-668` | `usage.as_ref().map_or("completed", …)` — with no terminal event the line reads `← completed | 0.00s | 0 in | 0 out`. Every upstream failure is invisible in the TUI. |
| `function_call_arguments.done` silently replaces accumulated deltas | `src/stream.rs:742-772` | On any divergence the done blob overwrites the deltas, even when it is a strict subset. Deltas `{"command":"ls -la"}` + done `{}` → the client receives `partial_json: "{}"`. |
| `error` SSE path emits no `message_delta`; `[DONE]` path emits a zero-usage delta | `src/stream.rs:392-421`, `src/proxy.rs:848-861` | The client never gets the final usage/stop_reason on the error path. |
| Duplicate `output_item.added` orphans a content block | `src/stream.rs:586-595`, `612-618` | `tool_blocks.insert` / `thinking_blocks.insert` **overwrite**, so the earlier block's `content_block_stop` is never sent. |

---

## Appendix B — Documentation that is currently wrong

Update these when the corresponding bugs are fixed, or they will mislead the next person:

| Claim | File | Reality |
|---|---|---|
| "`tool_choice`, `temperature`, `top_p`, `max_tokens`, and **`stop_sequences`** are forwarded." | `REFERENCE.md` (translation overview) | **FALSE** — `stop_sequences` is dropped with a WARN (`translate.rs:198-208`); `ResponsesRequest` has no `stop` field. |
| "`cache_control` on user **and system** text and image blocks is translated to OpenAI `prompt_cache_breakpoint`" | `REFERENCE.md`, `src/config.rs:64` | **FALSE for system** — `instructions` is `Option<String>` (`responses.rs:35`); only user blocks get breakpoints (`translate.rs:1361-1364`). |
| "`default_model` is a safety net. If the upstream rejects a model, the proxy retries once with this fallback and logs at WARN." | `proxy.json-guide.md` | **FALSE** — the fallback never fires (0 WARNs in 539 MB). See BUG 2. |
| "Off-by-default keeps request and response bodies off the terminal and off disk on every upstream error." | `proxy.json-guide.md` | True only while `log_to_disk` is false — the shipped `proxy.json` has `"log_to_disk": true`. |
| "Extended thinking blocks — out of scope" | `REFERENCE.md` | Understates it: the proxy **generates** thinking blocks and then **drops them**. See BUG 1. |
| "Server-side tools (`web_search`, `web_fetch`, `code_execution`) — out of scope" | `REFERENCE.md` | `web_search` *is* translated, to a native built-in that non-OpenAI upstreams reject. See BUG 5. |
| "`cargo clippy --all-targets --all-features -- -D warnings` is the lint gate" | `REFERENCE.md` (Development) | HEAD does **not** satisfy it (5 warnings); `cargo fmt --check` also fails. |

---

## Appendix C — Verified-clean (do **not** re-investigate these)

- **SSE framing** is sound — the `eventsource-stream` crate handles multi-line `data:`,
  `:` comment lines, CRLF vs LF, BOM and chunk-split lines correctly (verified E2E).
  `event:` lines are ignored, but the upstream sends `data:`-only frames and the JSON
  `type` field is authoritative.
- `message_start` is emitted exactly once and first; `message_delta` (stop_reason +
  usage) precedes `message_stop`; exactly **one** `message_stop` on every path (success,
  `response.failed`, top-level `error`, SSE error, early EOF); `finish()`/`finalize()`
  are idempotent.
- Tool arguments are emitted exactly once (idempotent via an `emitted` flag); the same
  tool call is never emitted twice.
- Usage is **replaced**, not accumulated — no double counting across
  `completed`/`incomplete`; `cached_tokens` is mapped correctly.
- **No hang** — `reqwest::Client::builder().timeout(config.request_timeout)` bounds the
  whole request including the body read; an early EOF becomes a clean `error` SSE event.
- **No deadlock** — `TranslatorStream` owns both the SSE reader and the translator;
  `OutputSink` uses an unbounded `std::sync::mpsc`, so `emit` never blocks; no tasks are
  spawned or leaked.
- **No output_index collisions** — re-segmenting on index-reset gives 6,824 segments with
  0 intra-response reuse; on 11-12 exactly 188 segments == 188 streaming requests.
- **No dangling/divergent deltas** — 0 in the corpus.
- `total_tokens == input + output` on 3,325 / 3,325 usage objects.
- `max_tokens` → `max_output_tokens` rename is correct; `tool_choice`
  (auto/any/none/tool) and `disable_parallel_tool_use → parallel_tool_calls` map correctly.
- **No credential leakage** — `upstream_api_key` is never traced/printed; `proxy*.json`
  is git-ignored; `/healthz` exposes nothing. reqwest strips `Authorization` on
  cross-host redirects.
- **Auth** — the `X-Proxy-Key` check runs *before* body parsing; constant-time over the
  equal-length case; a whitespace-only key is treated as unset.
- **Concurrency** — no unsynchronised global mutable state (`ArcSwap` for
  mappings/capabilities, `Arc<Config>` immutable, `std::sync::mpsc` for the log sink,
  `task_local!` for the sent body).
- **UTF-8 slicing** — the only truncation (`truncate_user_id`) is boundary-safe.
- **No panics** — no `unwrap`/`expect` on request data in the hot path.
- **Token arithmetic** — `u64` counters in `tui/stats.rs`; overflow needs ~1.8e19 tokens.
- **Config validation** — `deny_unknown_fields` + `Result` everywhere; a bad
  `proxy.json` fails loudly at startup; a missing `UPSTREAM_BASE_URL`/`UPSTREAM_API_KEY`
  is a hard error, never a panic.

---

## Appendix D — Useful commands

```bash
cd /home/heylon/git/openai-to-anthropic-proxy

# build / test / lint
cargo build
cargo test --all-targets                 # 333 tests, all green at HEAD
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all -- --check               # currently FAILS (14 diffs)

# re-extract the logs
mkdir -p /tmp/proxylogs && cd /tmp/proxylogs && unzip -o /home/heylon/git/proxy.log.2026-11-12.zip

# anchored log greps (level field is right-padded by tracing!)
cd /tmp/proxylogs
grep -cE '^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9:.]+Z +WARN +openai_to_anthropic_proxy::translate: dropping unsupported block' proxy.log.2026-11-12

# distinct log templates across the corpus
for f in proxy.log.*; do sed -E 's/^[0-9T:.Z-]+ +//' "$f" | sed -E 's/[0-9]+/N/g' | cut -c1-140; done | sort | uniq -c | sort -rn | head -30

# find the commit that introduced/fixed a string
git log -S"<exact string>" --oneline -- src/
git log -S"<exact string>" --oneline        # note: duplicated history — check both twins

# blame a line
git blame -L 143,154 src/anthropic.rs
```

---

## Appendix E — Scratch dirs (safe to delete)

`/tmp/proxylogs` (539 MB extracted logs) · `/tmp/agent-logtranslate` ·
`/tmp/agent-logproxy` · `/tmp/agent-translate` · `/tmp/agent-stream` ·
`/tmp/agent-proxy` · `/tmp/agent-git`

Several contain throwaway copies of the repo with probe test modules
(`/tmp/agent-*/repo`) — the real repo was never modified.

> **Note:** this file lives in the repo root and is **untracked**. It will show up in
> `git status`. Add it to `.gitignore` (or move it outside the repo) if a clean status
> matters, but keep it in the project folder so it survives compaction.
