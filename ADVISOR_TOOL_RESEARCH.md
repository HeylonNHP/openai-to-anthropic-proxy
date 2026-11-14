# ADVISOR TOOL RESEARCH

> Scratchpad. Working notes for: *"what would it take to implement Anthropic's
> `advisor_tool` server tool in this proxy?"*
>
> Started 2026-11-14. Repo: `/home/heylon/git/openai-to-anthropic-proxy`.
> Baseline for §11 onward: HEAD `df5d78e` (the orphaned-instructions fix).
> Commits on this branch: `671dabc` (initial research), `df5d78e` (option D).
>
> Researched in two waves by seven sub-agents:
> - **wave 1:** protocol (docs + binary + corpus), architecture (proxy code),
>   wire evidence (binary offsets + corpus). Disagreements resolved in §9.
> - **wave 2 (2026-11-14, for the modular-implementation question):** pipeline
>   seams, streaming feasibility, config/state/usage/failure patterns, and
>   exact protocol boundaries. Recorded in **§11**, with the implementation
>   plan in **§12**.
>
> §§1-10 are the initial research. §11 corrects several of its estimates
> and supersedes them where they conflict. Read §11 and §12 before building.

---

## 1. The question

Claude Code 2.1.322 has an **advisor**: a second, more capable model the primary
model consults mid-turn for strategic guidance. On Anthropic's API it is a
**server-side tool** (beta `advisor_20260301`).

Today it does not work through this proxy. This document is the research:
what the protocol actually is, what the proxy would have to do, and whether it
is worth doing.

---

## 2. What is already established (verified, 2026-11-14)

These were measured, not inferred. Each is reproducible.

### 2.1 The advisor is server-side; Claude Code never calls it itself

Running Claude Code 2.1.322 with `--advisor opus` against the proxy, its own
debug log says:

```
[AdvisorTool] --advisor opus
[AdvisorTool] Advisor model: opus
[AdvisorTool] Server-side tool enabled with claude-opus-5-5 as the advisor model
```

The alias `opus` is resolved to the real Anthropic model id `claude-opus-5-5`
and enabled as a **server tool**. The upstream only ever saw
`claude-sonnet-5-5`; `modelUsage` contained only sonnet. **No separate HTTP
request is made for the advisor model** — the API server runs it.

### 2.2 The proxy drops the advisor tool declaration

`src/translate.rs:171`:

```rust
let input_schema = resolve_local_refs(t.input_schema.clone()?);
```

`filter_map` + `?` on a `None` ⇒ **any tool without an `input_schema` is
discarded**. An Anthropic server-side tool has no `input_schema`; only
`web_search` is special-cased (`src/translate.rs:157`). The 12 tools that
reached the upstream contained no advisor.

The proxy's `Tool` struct (`src/anthropic.rs:266`) has no `type` field, so a
server-tool declaration cannot even be *identified* as one.

### 2.3 The beta header never leaves

`src/proxy.rs:337` forwards only `x-request-id` and `anthropic-version`.
`anthropic-beta` — which carries `advisor-tool-2026-03-01` — is dropped.

### 2.4 Claude Code's failure paths are mostly *not* silent

| what happens | what the user sees |
|---|---|
| server returns `advisor_tool_result_error` | **`Advisor unavailable (<error_code>)`**, in red |
| server refuses the advisor entry | `[advisor] the server refused the advisor …; retrying once without the advisor` — debug log only ⚠️ |
| advisor skipped (capability / model rank) | `[AdvisorTool] Skipping advisor - …` — debug log only ⚠️ |

### 2.5 The upstream `incomplete` is a separate issue

The `incomplete` seen in the TUI is an upstream `status:"incomplete"` with
`incomplete_details.reason:"max_output_tokens"` — Ollama marking a response
that reached the client's token cap. It is unrelated to the advisor. See §8.

---

## 3. The wire protocol

### 3.1 The tool declaration

Claude Code builds it as an object literal (binary offset 296,153,448):

```js
let VN=[...ne.extraToolSchemas??[]];
if(du)VN.push({type:Rpn,name:xAe,model:du,...hd&&{defer_loading:!0}});
```

with `Rpn="advisor_20260301"` (offset 294,976,092) and `xAe="advisor"`
(offset 291,752,257). So the declaration is **exactly three fields**:

```json
{ "type": "advisor_20260301", "name": "advisor", "model": "<canonical advisor model id>" }
```

Because it is a literal, every other field is provably impossible from this
code path: **no** `input_schema`, `description`, `max_uses`, `max_tokens`,
`caching`, `cache_control`, `allowed_callers` or `strict`. (`defer_loading: true`
is the sole optional extra, for the mid-conversation-tool-change beta.)

The full server-side schema does have those fields — Anthropic's Python SDK
(`beta_advisor_tool_20260301_param.py`):

```python
class BetaAdvisorTool20260301Param(TypedDict, total=False):
    model: Required[ModelParam]
    name: Required[Literal["advisor"]]
    type: Required[Literal["advisor_20260301"]]
    allowed_callers: List[Literal["direct", "code_execution_20250825", ...]]
    cache_control: Optional[BetaCacheControlEphemeralParam]
    caching: Optional[BetaCacheControlEphemeralParam]
    defer_loading: bool
    max_tokens: Optional[int]
    max_uses: Optional[int]
    strict: bool
```

### 3.2 The beta header

The **beta string is `advisor-tool-2026-03-01`** — *not* `advisor_20260301`
(that is the tool `type`). Binary offset 285,449,485:

```js
tnt=P("advisor_tool","advisor-tool-2026-03-01"),
```

It is pushed into the request's beta list at offset 296,135,282 and reaches the
wire as a comma-joined `anthropic-beta` header alongside several others:

```
anthropic-beta: claude-code-20250219,interleaved-thinking-2025-05-14,context-management-2025-06-27,advisor-tool-2026-03-01,thinking-token-count-2026-05-13,advanced-tool-use-2025-11-20,effort-2025-11-24
```

### 3.3 The response blocks

The proxy must emit, **inside one assistant message**:

```json
{ "type": "server_tool_use", "id": "srvtoolu_<id>", "name": "advisor", "input": {} },
{ "type": "advisor_tool_result", "tool_use_id": "srvtoolu_<id>",
  "content": { "type": "advisor_result", "text": "<advice>" } }
```

Hard requirements:

1. `server_tool_use.name === "advisor"` — the client matches on exactly this
   string (offset 296,236,220).
2. `server_tool_use.id` and `advisor_tool_result.tool_use_id` **must be equal
   and non-empty**.
3. `content.type` must be one of **three** values. An unknown one → the client
   logs `tengu_advisor_unknown_content` and **renders nothing**.

| `content.type` | fields | when |
|---|---|---|
| `advisor_result` | `text`, `stop_reason?` | the advisor returned plaintext |
| `advisor_redacted_result` | `encrypted_content`, `stop_reason?` | Anthropic's stronger models return encrypted output |
| `advisor_tool_result_error` | `error_code` | the consultation failed |

4. **A proxy must emit `advisor_result`, never `advisor_redacted_result`** —
   the `encrypted_content` blob is Anthropic-server-encrypted and cannot be
   manufactured. Emitting it would show the user "Advisor has reviewed the
   conversation" while the executor never saw any advice.
5. `error_code` must be one of the **six** codes the client knows
   (offset 291,751,226): `max_uses_exceeded`, `too_many_requests`,
   `overloaded`, `prompt_too_long`, `execution_time_exceeded`, `unavailable`.
   The SDK lists a seventh, `model_not_found`, which the client does **not**
   know — emitting it renders `Advisor unavailable (unknown)`.

`stop_reason` accepts eight values: `end_turn`, `max_tokens`, `stop_sequence`,
`tool_use`, `pause_turn`, `compaction`, `refusal`,
`model_context_window_exceeded`.

### 3.4 The advisor's semantics

- **The executor decides when to call it**, like any other tool. The server
  supplies the context; the executor supplies the timing.
- It is **advisory-only** — it runs with no tools and cannot block an action.
  The one quasi-denial is a `stop_reason: "refusal"`, which Claude Code renders
  as *"Advisor declined to advise on this request"*; the executor continues.
- `max_uses` caps consultations per turn. **Claude Code never sets it**, so it
  is unlimited in practice — which is why the proxy should enforce it itself.
- The advice reaches the executor **server-side**: Anthropic's server decrypts
  the result and renders the plaintext into the executor's prompt. The client
  never does this. **⇒ A proxy must inject the advice into the upstream prompt
  itself; emitting the block alone is not enough.**
- The advisor sub-inference **does not stream**. The executor's stream pauses
  while the advisor runs, and the whole result arrives in one event.

### 3.5 Usage / billing

Advisor tokens are **not** in the top-level `usage` — they are billed at the
advisor model's rate and reported in `usage.iterations[]`:

```json
"usage": {
  "input_tokens": 1760, "output_tokens": 531,
  "iterations": [
    { "type": "message",         "input_tokens": 412, "output_tokens": 89,  ... },
    { "type": "advisor_message", "model": "claude-opus-5-5", "input_tokens": 823,
      "output_tokens": 1612, "cache_read_input_tokens": 0,
      "cache_creation_input_tokens": 0, "cache_creation": null },
    { "type": "message",         "input_tokens": 1348, "output_tokens": 442, ... }
  ]
}
```

Claude Code reads those iterations (offset 294,303,021) and bills each at its
own model's rate. If `iterations` is absent, the advisor is silently unbilled.

---

## 4. On-the-wire evidence

### 4.1 There is no real advisor traffic in the corpus

Plain-substring counts over all 27 files (539 MB):

| substring | count | verdict |
|---|---|---|
| `advisor_20260301` | 48 | all documentation prose |
| `advisor-tool-2026-03-01` | 72 | all documentation prose |
| `advisor_tool_result` | 216 | all documentation prose |
| `srvtoolu` | **0** | — |
| `"name":"advisor"` | **0** | — |
| `"type":"server_tool_use"` | **0** | — |

Every hit is in `proxy.log.2026-08-23`, and every count is an exact multiple of
**24** — one document replicated across 24 request bodies. The document is an
**Anthropic-internal `claude-api` CLAUDE.md** (extracted at byte offset
5,443,771–6,391,729), which is what supplied the pairing table and the
redaction rules quoted in §3.

The 884 `server_tool_use` hits are the user's own **proxy Rust source**, read
by Claude Code and echoed into the request body. There is no Anthropic
server-tool traffic in the corpus at all.

### 4.2 A live inconsistency the corpus does prove — **FIXED 2026-11-14**

In real Aug-2026 traffic, the upstream request body carried the `# Advisor
Tool` instructions in `instructions` (15,121 chars) while the `tools` array
(16 tools) contained **no advisor**. The proxy's `append_system_message`
(`src/translate.rs:1849`) folds Anthropic `role:"system"` messages into
`instructions`, so the executor was told *"call advisor BEFORE substantive
work"* while the tool did not exist.

**This is now fixed.** The corpus contains **84** such `instructions` strings
(all in `proxy.log.2026-08-23`), every one with the identical opener
`You have access to an \`advisor\` tool…`. In 79 the section ends at the next
`<system-reminder>`; in 5 the reminder tags are absent and it ends at the
line `The following skills are available for use with the Skill tool:`. In all
84 the section starts at line 16 of the assembled prompt and is followed by
other content, so a heading-to-next-heading strip would have eaten the
skills list and the `## Auto Mode Active` section.

The fix (`src/translate.rs`, `strip_advisor_tool_instructions`) removes only
from a line-start `# Advisor Tool` heading up to the first
`<system-reminder>` / skills line, and only when no tool named `advisor` is
present. It does **not** append Claude Code's own `"The advisor tool is no
longer available; disregard the earlier advisor instructions."` (binary
offset 291,758,3xx): the executor never saw a working advisor through this
proxy, so there is nothing to retract.

Verified three ways: 5 unit tests (both corpus variants, a present advisor
tool, inline-quote and no-advisor no-ops), a replay of all 84 real strings
through the same algorithm (0 problems), and an end-to-end request through
the real binary with a real corpus `system` prompt (15,121 → 13,103 chars;
`# Advisor Tool`, `Call advisor BEFORE`, `advisor()` all absent; git context,
skills reminder and `## Auto Mode Active` all present).

### 4.3 The client accepts the blocks

Fed a synthetic `server_tool_use` + `advisor_tool_result` pair over SSE,
Claude Code 2.1.322 accepted it with **no error, no warning, exit 0**, and
echoed both blocks back verbatim in the next request. An orphan
`advisor_tool_result` was also tolerated (Anthropic's API is not).

---

## 5. What the proxy would have to change

### 5.1 The tool-declaration path

`src/translate.rs:149-186`:

```rust
let mut tools: Option<Vec<ToolDefinition>> = req.tools.as_ref().map(|tools| {
    tools.iter().filter_map(|t| {
        if t.name == "web_search" { return Some(ToolDefinition::WebSearch(…)); }
        let input_schema = resolve_local_refs(t.input_schema.clone()?);   // ← line 171
        registry.insert(t.name.clone(), input_schema.clone());
        Some(ToolDefinition::Function(ResponsesTool { … }))
    }).collect()
});
```

Two changes are needed:

1. **`Tool` gains `type` and `model`** (`src/anthropic.rs:266`). Measured blast
   radius: **exactly 4 sites, all test fixtures** — `src/translate.rs:1979,
   3526, 3778, 3785`. The library compiles clean and all 382 tests pass. It is
   backwards-compatible: `#[serde(default)]` + `Option`, and serde was already
   ignoring the unknown `type`/`model` keys.
2. **The advisor must be intercepted locally, not forwarded as-is.** It should
   be forwarded as a **zero-argument function tool** (so the executor model can
   *signal* a consultation — that is Anthropic's semantics, "the executor model
   determines when to call it, like any other tool"), and the resulting
   `function_call` intercepted by the proxy. Forwarding it verbatim would make
   an OpenAI-compatible upstream 400 on the unknown tool type.

Side benefit: the `web_search` special case matches by **name**, so a user with
an MCP tool literally named `web_search` has it silently converted into
OpenAI's built-in web-search tool. Having `Tool.r#type` lets the proxy tell
Anthropic's server-side web search from a plain function.

### 5.2 The tool-call path

- **Non-streaming**: `src/translate.rs:5057` `responses_to_anthropic` →
  `build_content_blocks` (`src/translate.rs:5246`). The upstream's
  `function_call` becomes an Anthropic `tool_use` at
  `src/translate.rs:5305-5355`. Intercept with a guarded arm
  `OutputItem::FunctionCall { name: "advisor", .. }` before the generic one.
- **Streaming**: `src/stream.rs:552-600` `handle_output_item_added`. Same
  guarded arm.

**But the real interception cannot live in the translator** — it is a pure state
machine with no I/O, and the advisor needs an HTTP call. It belongs one level
up:

- non-streaming: `src/proxy.rs:362` `handle_messages_inner`, between
  `parse_upstream_body` and `translate::responses_to_anthropic`;
- streaming: `src/proxy.rs:762` `TranslatorStream::poll_next`.

### 5.3 The `web_search` precedent

`src/stream.rs:944-959` `emit_server_tool_use` and its non-streaming twin
`src/translate.rs:5254-5280` are the template for the block shape — and the
wrong template for the mechanism:

| | `web_search` | `advisor` |
|---|---|---|
| who runs the tool | the upstream (OpenAI built-in) | **the proxy** |
| nested HTTP call | none | **one, to a different model** |
| the executor continues in the same turn | yes (upstream does it) | **no — the proxy must re-run the executor** |
| result payload | citations | free-text advice |
| id format | `stoolu_web_search_01` | `srvtoolu_…` |
| usage | `server_tool_use.web_search_requests` | `usage.iterations[{type:"advisor_message"}]` |

### 5.4 The nested upstream call — the real cost

The proxy's request path **is** re-entrant (`handle_messages` is async,
`AppState` holds a cheap `reqwest::Client`, and `handle_messages_inner` already
awaits an upstream call). The nested call should be a **direct Responses-API
call**, not a loopback through the Anthropic handler.

But the Responses API has no server-tool concept, so it has no "sub-inference"
concept, so it cannot resume an interrupted generation with new context.
Anthropic's advisor runs *inside* the executor's generation. Through the proxy:

```
executor #1 : full input + partial output
advisor     : full transcript + advice
executor #2 : full input AGAIN + final output
```

≈ **2× the executor's input tokens + 1.5× its output** per consultation. Claude
Code's own advisor instructions ask for **two** consultations per substantive
task, so ≈ **4×** the tokens. Anthropic's API charges ≈1×.

**Failure handling**: Anthropic's contract is that a failed consultation *never
fails the agent's turn*. The proxy must emit
`advisor_tool_result{content:{type:"advisor_tool_result_error",error_code:"unavailable"}}`
and continue. The advisor's timeout must be **shorter** than the executor's
(`state.config.request_timeout`), or a hung advisor hangs the whole turn.

### 5.5 Interactions with existing machinery

| subsystem | verdict |
|---|---|
| `src/capabilities.rs` | **reuse, do not extend.** `RequestParam::ALL = [Temperature, TopP]`; an advisor failure is a 500/timeout and can never match `unsupported_parameter`. The nested call should still go through `apply_known`/`record_unsupported` so facts are learned per advisor model. |
| `src/repair.rs` `ToolSchemaRegistry` | **no change.** The advisor has no schema, must not be inserted, and `registry.get("advisor")` returns `None` → arguments pass through untouched (`src/stream.rs:1035-1040`). |
| `STRICT_PROPERTY_BUDGET` (5000) | **no impact.** The advisor's schema is empty → 0 properties; a 0-weight tool is never the one demoted to `strict:false`. |
| `src/responses.rs` `OutputItem` | **no new variant.** The advisor is driven entirely proxy-side; no upstream emits an advisor item. |
| usage accounting | **must be extended.** `src/anthropic.rs:405` `Usage` has no `iterations` field; six `AnthropicUsage {}` construction sites would need it. The advisor's tokens must **not** be folded into the executor's top-level totals. |
| `src/model_rejection.rs` / `default_model` | **the advisor call must NOT fall back.** Four reasons: the fallback would substitute a different model for the advisor (silently weakening the review — or making the advisor *the executor's own model*, i.e. a self-review dressed up as a second opinion); `default_model` is the executor's safety net; the fallback re-attributes stats via `record_actual`; and mechanically it rebuilds the *executor's* Anthropic request, not the advisor's Responses request. **The advisor must fail closed.** |
| `src/proxy.rs:337` header forwarding | **leave `anthropic-beta` dropped.** The upstream is not Anthropic. But the proxy must never return a 400 mentioning `advisor-tool-2026-03-01` or `Input tag 'advisor_20260301'` — Claude Code treats those as "the server doesn't know this tool" and latches the advisor off for the whole process. |
| `src/anthropic.rs` inbound `ContentBlockParam` | **must be extended.** `server_tool_use` and `advisor_tool_result` currently fall into `#[serde(other)] Unknown` and are dropped (`src/translate.rs:1708-1713`), so the advice would be lost every subsequent turn. |

---

## 6. Design options

| option | fidelity | cost | effort | verdict |
|---|---|---|---|---|
| **A — faithful mid-turn advisor** (executor → advisor → executor#2) | high | ~2× per consultation, ~4× per task | ~1,000 lines + ~250 test lines | the only version that deserves the name |
| **B — proactive reviewer** (advisor runs before the executor, always) | low — reviews the *pre-turn* transcript | ~1.3–1.5×, always | ~350 lines, **no streaming splice** | the 80/20 if the goal is "a second opinion on every task" |
| **C — next-turn delivery** | wrong — advice arrives after the work | ~250 lines | | don't |
| **D — do nothing to the proxy; fix the instructions leak** | — | ~15 lines | | **do this first** |

### 6.1 Effort estimate (option A)

| file | change | new lines |
|---|---|---|
| `src/anthropic.rs` | `Tool.r#type`/`Tool.model`; `AdvisorToolResult` blocks; inbound `ServerToolUse`/`AdvisorToolResult`; `Usage.iterations` | ~110 |
| `src/translate.rs` | recognise the declaration; forward as a function tool; intercept the call; assemble the advisor's prompt; translate the reply | ~260 |
| `src/stream.rs` | intercept in the streaming path; survive two upstream responses | ~200 |
| `src/proxy.rs` | the nested request; the executor re-run; the streaming splice; the failure paths; `max_uses` | ~330 |
| `src/config.rs` | an `advisor` config section + guide | ~90 |
| `src/tui/stats.rs` | an advisor token bucket | ~40 |
| `tests/proxy_e2e.rs` | 8–10 tests | ~250 |

≈ **1,000 lines of new code + ~250 lines of tests, 6–7 files.** 3–5 working
days for someone who knows the codebase; ~2 weeks done properly.

A **spike of the block plumbing alone is 97 lines across 3 files**
(`src/anthropic.rs` +17, `src/translate.rs` +51, `src/stream.rs` +29) and
compiles with 4 one-line test-fixture fixups. The plumbing is a solved problem.

### 6.2 The highest-risk part

**The streaming two-response splice.** `StreamTranslator` is a single-response
state machine with a hard stop:

```rust
// src/stream.rs:182-188
pub fn feed_event(&mut self, event: &ResponsesStreamEvent) -> Vec<StreamEvent> {
    if self.finished {          // ← line 185
        return Vec::new();
```

and `capture_terminal` (`src/stream.rs:817-857`) ends with `self.finalize(...)`
→ `self.finished = true`. **The moment the executor's first response completes,
the translator refuses every subsequent event.** Feeding the continuation into
the same translator would silently drop the entire second answer. The
implementation must add a genuine "resume" to `StreamTranslator` and the bridge
must hold two upstream streams plus an in-flight future inside a hand-rolled
`poll_next`.

### 6.3 Backwards compatibility

Near-zero risk, provided the feature is behind a config flag that defaults to
off and `anthropic-beta` stays dropped. The `Tool` change is 4 test-fixture
lines and no behavioural change.

---

## 7. The cheap-advisor practice

**Do the reverse engineering against a cheap model.** Kimi is the wrong advisor
for that: `kimi-k3.1-pro` is **$2/M input, $10/M output** on Ollama Cloud — the
most expensive model in the catalogue by a wide margin.

Ollama Cloud prices (USD per million tokens, [ollama.com/pricing][pricing],
2026-11-14):

| model | input | output | vs. kimi-k3.1-pro |
|---|---:|---:|---|
| **kimi-k3.1-pro** | $2.00 | **$10.00** | — |
| kimi-k3 | $3.00 | $15.00 | worse |
| glm-5.3 | $1.40 | $4.40 | 2.3× |
| minimax-m3 | $0.60 | $2.40 | 4× |
| **beam** | **$0.125** | **$0.40** | **25× cheaper output** |
| deepseek-v4.1-flash | $0.15 | $0.60 | 17× |
| glm-5.3-flash | $0.15 | $0.50 | 20× |
| gemma4 | $0.14 | $0.40 | 25× |
| **mimo-v2.6-flash** | $0.14 | **$0.28** | **36× cheaper output** |
| gpt-oss:120b | $0.15 | $0.60 | 17× |
| gpt-oss:20b | $0.07 | $0.30 | 33× |
| nemotron-3-super | $0.015 | $0.60 | 17× |
| nemotron-3-nano:30b | $0.06 | $0.24 | 42× |

**Recommended advisor for testing: `beam`** ($0.125 in / $0.40 out, 1M
context) — it is already in the user's alias map, it is 25× cheaper on output
than kimi-k3.1-pro, and it accepts `"xhigh"` natively (Ollama's `/api/show`
reports `thinking.values = [low, medium, high, xhigh, max]` for it, unlike
kimi-k3.1-pro's `[false, low, medium, high, max]`). **`mimo-v2.6-flash`** is
even cheaper on output if the advisor only needs to write a paragraph.

### 7.1 Why the alias map can do this for free

The capability check that makes the advisor a *model* rather than a feature is
**client-side and runs on the canonical Anthropic names**:

```js
function pHe(e,n){ if(cc())return!0; let r=$i(e),s=Ui(n); return r===void 0||s===void 0||r<=s }
```

`$i`/`Ui` read `advisor_rank` from Claude Code's **local model catalog**, keyed
on the names Claude Code itself sent (`claude-sonnet-5`, `claude-opus-5-5`).
The proxy rewrites those names **afterwards**, via the alias map.

**⇒ The alias map is invisible to every capability gate.** An operator can map
`claude-opus-5-5` to a cheap upstream model and Claude Code will never know.
The "advisor must be at least as capable as the executor" rule is satisfied by
the *name*, not by the model that actually answers.

That is what makes a cheap advisor possible — and also what makes it a bit of a
lie: the user gets a cheaper model pretending to be a more capable one.

### 7.2 But the alias map is shared with the executor's own alias

`claude-opus-5-5` is also the natural inbound name for ordinary opus turns.
Mapping it to `beam` would change those too. **⇒ The advisor's upstream model
should be its own config key (`advisor.model`), not an entry in
`model_aliases.map`.** Same for the advisor's effort: `reasoning.models` is
keyed on the upstream name, so a shared name would couple the advisor's
thinking level to the executor's.

The advisor's own config would then carry: `model`, `max_uses_per_turn`,
`max_tokens`, `timeout`, and `effort` — all of them separate from the
executor's.

### 7.3 The cost control that matters most

`max_uses` is **never set by Claude Code**, and Anthropic's own instructions
ask for **two consultations per substantive task**. With a proxy that re-runs
the executor, two consultations ≈ **4× the executor's tokens** and roughly
doubled latency. The proxy must enforce `max_uses` itself — it is the
operator's only safety valve, and it is part of the tool's documented surface.

---

## 8. Unrelated but adjacent: the upstream `incomplete`

Recorded here so it is not confused with the advisor work.

- Ollama marks a Responses response `incomplete` when `output_tokens` reaches
  `max_output_tokens` — **even when the answer fits in that last token**.
  Measured: `think:false`, cap 1 → `status: incomplete`, message text `"OK"`,
  `output_tokens: 1`.
- The proxy translates that faithfully to Anthropic `stop_reason: "max_tokens"`.
  Claude Code renders that as *"API Error: Claude's response exceeded the N
  output token maximum."*
- Every `incomplete` in the 539 MB corpus is on a cap of **1** (14×) or **64**
  (712×). Real turns (8192/2112/1024) completed 588/588.
- The operator-visible consequence is that the TUI prints the *raw* upstream
  status on the non-streaming path (`← incomplete`, `src/proxy.rs:662`) but the
  *translated* stop reason on the streaming path (`← max_tokens`,
  `src/proxy.rs:740`). Same event, two different-looking verdicts.
- Separately: `reasoning.effort: "xhigh"` is **not a valid level for
  `kimi-k3.1-pro`** — Ollama's vocabulary is `[false, low, medium, high, max]`
  with default `max`, so `"xhigh"` silently becomes the model's *deepest*
  level. That is why the opus alias behaves differently from the other three.

---

## 9. Open questions and resolved disagreements

### 9.1 Resolved: the advisor is **not** gated out behind a proxy

One research report claimed the provider check ("not `firstParty` → advisor
never attached") was "the big one for a proxy". **That is wrong**, and the
other two reports measured it independently. `ve()` (binary offset 283,498,300)
never reads `ANTHROPIC_BASE_URL`:

```js
function ve(){ … return i.CLAUDE_CODE_USE_BEDROCK?"bedrock": … :"firstParty"}
```

so with a proxy base URL and no `CLAUDE_CODE_USE_*` variable, `ve()` returns
`"firstParty"` and `zQt()` passes. The declaration was captured arriving at a
`127.0.0.1` base URL:

```json
{"type": "advisor_20260301", "name": "advisor", "model": "claude-opus-5-5"}
```

**Measured evidence beats a static reading.**

### 9.2 The one real proxy blocker

The **standby** advisor — the org's default advisor model, used when the user
has not set one — requires the first-party base URL:

```js
function ls(){return ve()==="firstParty"&&ki()}
function Kd(){let e=process.env.ANTHROPIC_BASE_URL;if(!e)return!0;return Ud(e)}
function Ud(e){try{let t=new URL(e).host;return["api.anthropic.com"].includes(t)}catch{return!1}}
```

**⇒ The user must set the advisor explicitly** (`--advisor opus`, or the
`advisorModel` setting, or `/advisor`). Without that, the standby path is
unreachable behind a proxy and the advisor is simply absent. This is
client-side, so a proxy cannot fix it — only the user can.

### 9.3 Still unknown

1. **The `tengu_sage_compass2` feature flag's value.** A remote flag, not
   resolvable statically. But `Ft.push(tnt)` fires when
   `zQt() && (MR() || ne.advisorModel!==void 0)` — **an explicitly configured
   advisor model bypasses the flag entirely** — and `CLAUDE_CODE_ENABLE_EXPERIMENTAL_ADVISOR_TOOL=1`
   bypasses the rank predicates too (`mX`, `Sp`, `pHe` all `if(cc())return!0`).
2. **Whether Anthropic's server enforces the executor↔advisor pairing table.**
   The client enforces it; the server-side behaviour was not verified (no live
   API call was made).
3. **`advisor_stripped`** — the attachment emitted when the client strips
   advisor blocks after a 400. Its emission and consumption are proven; its
   full schema is not.
4. **`Ct.advisorHeld`'s state machine** — only partially traced.
5. **The `server_tool_use.id` format.** Anthropic's ids are `srvtoolu_*`, but
   the binary contains only `srvtoolu_bdrk_` / `srvtoolu_vrtx_` (error
   classifiers). The client only requires the id to be non-empty and to match
   `tool_use_id`.
6. **Whether the Responses API can resume an interrupted generation** — the
   blocker behind option A. Not investigated; assumed absent.

---

## 10. Recommendation

1. **DONE (option D, 2026-11-14):** strip the `# Advisor Tool` block from the
   instructions the proxy forwards. See §4.2 for the implementation and its
   verification. The executor is no longer told to call a tool that does not
   exist.
2. **Then decide between option A and option B.** Option B (~350 lines, no
   streaming splice) answers "do I want a second opinion on every task?" for
   a fraction of the cost and risk; option A answers "do I want Anthropic's
   advisor?" and pays ~4× the tokens to get it.
3. **Either way:** gate it behind config (default off), give the advisor its
   own model / effort / `max_uses` / timeout keys rather than borrowing the
   executor's, **use a cheap model** (§7), and never let the advisor call fall
   back to `default_model`.

> **Superseded by §12.** The wave-2 research found a materially cheaper and
> safer design than option A/B: force a non-streaming upstream call, run the
> consultation as an explicit outer loop, and emit one synthetic SSE message.
> It also found that the whole thing can be done without touching `stream.rs`,
> `config.rs`, `AppState`, or the `Tool` struct. See §11 and §12.

---

## 11. Wave-2 research: the seams (2026-11-14)

Four sub-agents, all read-only against HEAD `df5d78e`. Every finding below is
from the actual source, with file:line. **Line numbers are for `df5d78e`** —
the §1-10 references to `translate.rs:149` / `responses_to_anthropic:5057`
were against `671dabc`; add +17 / +166.

### 11.1 What the codebase actually looks like

`src/lib.rs` is 22 lines. There is **one** sub-module directory in the repo
(`src/tui/`, `mod.rs` + 6 files). Everything else is a flat `src/<name>.rs`
with a `//!` doc block, a narrow `pub` API, private helpers, and an inline
`#[cfg(test)] mod tests`. **That flat-file shape is the house style** for a
single-purpose unit; `src/tui/` is the precedent only if a module grows past
~700 lines.

The three existing extension patterns, cheapest first:

| pattern | precedent | integration cost |
|---|---|---|
| **pure helper, private `mod`, takes primitives** | `src/model_rejection.rs`, `src/schema_refs.rs` | 1 `lib.rs` line, 1-2 call sites, 0 `Config`/`AppState` changes |
| **per-request state as an explicit parameter** | `src/repair.rs` (`ToolSchemaRegistry` returned in a tuple from the translator) | 0 `AppState` changes, but every signature on the path |
| **process-lifetime `Arc<Store>` in `AppState`** | `src/capabilities.rs` | 1 field, 1 literal, 1 new delegating router constructor, 2 `main.rs` args, TUI threading |

`model_rejection.rs` (commit `86b6a4a`) is the closest analogue: **1 private
`mod` line, 4 public items, 2 call sites in `proxy.rs`, zero `Config` fields,
21 in-file unit tests + rewritten e2e fixtures**. Its commit *deleted* ~200
lines of inline detector from `proxy.rs` — the integration was a net
simplification. That is the standard to aim at.

### 11.2 The two narrow hook points — and why they are not enough

- **Outbound augmentation:** `src/proxy.rs:262-263`, the blank line after
  `outbound.prompt_cache_key = …` and before the `tracing::info!("→ upstream")`
  at `:266`. `&mut ResponsesRequest`, `&AppState`, `&CreateMessageRequest` and
  the mappings snapshot are all in scope. The proxy already mutates `outbound`
  here (`:255` model, `:260` cache key) and later at `:404` (`apply_known`).
- **Final-`Message` post-processing:** `src/proxy.rs:628-629`, between
  `let anth = translate::responses_to_anthropic(...)` and the stats block.
  `Json(anth).into_response()` at `:680` is the only consumer, so `let mut anth`
  + one call is enough. **Caveat:** the stats block at `:629-638` reads
  `upstream_resp_body.usage`, not `anth.usage` — usage mutations here are
  invisible to `state.stats`.

**But a real advisor needs to re-run the executor**, which neither hook can
do. `handle_messages_inner` (`src/proxy.rs:362-681`, ~320 lines) is one
function owning the send loop (`:413-556`), the streaming/non-streaming split
(`:559`), the validation retry (`:603-626`), translation (`:628`), stats
(`:629-639`), logging (`:641-679`) and the response (`:680`). There is **no**
"send this `ResponsesRequest` and give me a parsed `ResponsesResponse`"
helper: `send_upstream_once` (`:321`) deliberately returns a raw
`reqwest::Response` so the streaming branch can consume `.bytes_stream()`.
`parse_upstream_body` (`:348`) is separate and never composed with it.

Retry loops, in order (all inside `handle_messages_inner`):

| # | what | lines | bound |
|---|---|---|---|
| 0 | `capabilities.apply_known(&mut outbound)` | `:404` | once before attempt 1 |
| 1 | outer `loop { send_upstream_once … }` | `:413-556` | — |
| 1a | `unsupported_parameter` → strip `temperature`/`top_p` | `:438-470` | `RequestParam::ALL.len() == 2` |
| 1b | `default_model` fallback (re-translates the whole request) | `:484-552` | `attempt == 1` only |
| 1c | `map_upstream_error` | `:554` | fatal |
| — | success split on **`outbound.stream`** | `:559` | — |
| 2 | `parse_upstream_body` | `:594` | non-streaming only |
| 3 | hidden validation/corrective retry | `:603-626` | `validation_retries < 1` |
| 4 | `responses_to_anthropic` | `:628` | — |

**There is no `xhigh` effort-downgrade loop.** Effort is resolved once by
`Config::reasoning_for_request` and forwarded verbatim; `src/translate.rs:114`
explicitly says "no per-request 'downgrade'" (correcting an earlier guess in
this document).

The **unavoidable refactor**: an outer loop needs an `AttemptState` holding
`outbound`, `registry`, `fallback_used`, `param_retries`, `attempt`, `start`
(6 locals at `:393-401`), plus a ~150-line `send_and_classify` extracted from
`:414-556` returning `Streaming(reqwest::Response)` /
`NonStreaming(ResponsesResponse)` / `Err(AppError)`. `req` is **moved** into
`handle_messages_inner` at `:310`, so re-translating after advice requires
keeping it available. Estimated **150-250 lines of `proxy.rs` restructure** —
this is the single largest cost and it is the same for every design option.

### 11.3 The streaming decision — buffer, do not splice

`TranslatorStream` (`src/proxy.rs:762-780`) polls **only** an upstream SSE
`S` and a `StreamTranslator`; it holds no future. `poll_next` (`:874-994`) is a
hand-rolled state machine with 6 `encode_sse_events` call sites and 3 error
paths. `StreamTranslator::finish(mut self)` **consumes** the translator
(`src/stream.rs:349`), `feed_event` hard-stops on `self.finished`
(`src/stream.rs:185-187`), and `capture_terminal` → `finalize` sets
`finished = true` (`:817-857`, `:905-921`). Splicing a second upstream
response means adding a `resume()` to `StreamTranslator`, changing
`finish(self)` → `&mut self`, and turning `poll_next` into a two-stream +
in-flight-future state machine: **~350-400 lines across `stream.rs` and
`proxy.rs`**, on the hot path for **every** streaming request, with 31
`stream.rs` unit tests and 37 e2e tests pinned to the current one-terminal
invariants. Risk to non-advisor users: **HIGH**.

**The better option:** force `outbound.stream = false` when the advisor is
enabled, run the whole chain through the **existing** non-streaming pipeline
(`:594-628`), then convert the final `Message` into a synthetic SSE sequence.
This is not a fidelity compromise — Anthropic's own advisor **does not stream**
the sub-inference; the executor's stream pauses and the result arrives whole
in one `content_block_start` event (§11.5). Pieces that already exist:

- `StreamEvent` and all 8 variants (`src/anthropic.rs:453-480`)
- `encode_sse_events(&[StreamEvent]) -> Bytes` (`src/proxy.rs:1000-1014`) —
  a pure free function, **zero coupling** to `TranslatorStream`; needs only
  `fn` → `pub(crate) fn` (1 word)
- `Body::from_stream(...)` (`:585`), `futures_util::stream::iter` (`:21`)
- the non-streaming stats/logging block at `:629-673` (so no `Drop` hook is
  needed)

What is **missing** is a `message_to_events(&Message) -> Vec<StreamEvent>`
adapter (~55-75 lines), because there is no `Vec<ResponseContentBlock>` →
`StreamEvent` conversion anywhere. Streaming delta: **~75-100 production lines
+ ~80-120 test lines, 3-4 files, `src/stream.rs` untouched.**

Gotchas, all verified: (a) hard-code `Content-Type: text/event-stream` — the
upstream answered `application/json`, and copying `:561-565` verbatim would
forward the wrong header; (b) carry `stop_reason`/`stop_sequence` into
`message_delta` (mandatory); (c) `usage` in `message_start` and the final
`message_delta`; (d) thinking blocks need `signature_delta` immediately before
`content_block_stop` (`src/stream.rs:878-903`); (e) no `[DONE]` sentinel;
(f) `StreamEvent::Ping` is never constructed in production, and buffered
output has no idle gap — **do not emit `message_start` early**, or you need a
keepalive state machine; (g) TTFT becomes the full 3-call chain, and three
sequential calls each get their own `request_timeout`.

**Option 3 (answer `stream:true` with a JSON body) is simply wrong.** No
precedent exists: `src/translate.rs:132` is the only place `stream` is read,
and `:560` branches on `outbound.stream`. Claude Code's SDK expects an SSE
async iterable; `BUG_HUNT_SCRATCHPAD.md:847-859` documents the exact failure
(a normal-looking successful turn containing nothing). The buffered option
honours the client contract — it only changes the *upstream* decision.

| | buffer + synthetic SSE | true incremental splice | `stream:true` → JSON |
|---|---|---|---|
| streaming delta | ~75-100 prod + ~80-120 tests | ~350-400 | ~5-10 |
| `stream.rs` risk | **zero** | 31 tests + all streaming users | — |
| TTFT | full 3-call chain (burst at end) | best | none |
| verdict | **take this** | revisit only if measured | never |

### 11.4 Config, state, usage, failure — the house patterns

- **`Config` is expensive and has a cycle.** `src/config.rs:38-86` is a
  *resolved*, non-`Deserialize`, no-`Default` struct with 13 fields; the JSON
  mirror `JsonConfig` (`:702`) has **`deny_unknown_fields`**; there are **17
  full `Config { … }` literals** (15 in `tests/proxy_e2e.rs`, 2 in
  `src/proxy.rs`). Adding `pub advisor: AdvisorConfig` costs **~125-150 lines
  across 6 files** plus 16 one-line fixture fixups, and creates a
  `config.rs → advisor → Config` dependency cycle with **no precedent** (every
  sub-config type lives in `config.rs` today). Also: `JsonConfig` is rewritten
  wholesale by the TUI, so a new field **must** have `skip_serializing_if` or
  an older binary refuses to start (`src/config.rs:719-725`).
  **⇒ Keep advisor config in the advisor module** (its own file/env, loaded
  into a `OnceLock`, `enabled: false` by default). Zero `config.rs` edits,
  zero fixture fixups. This is a new convention; document it.
- **`AppState` is cheap to change** (1 field, 1 literal, 1 delegating router
  constructor, 2 `main.rs` args) — but **no new state is needed**. Use
  `state.client.clone()` (a cheap internal-`Arc` clone) and add
  `.timeout(cfg.timeout)` on the `RequestBuilder`; `reqwest` documents that
  this overrides the client-level timeout for that request only. No second
  client, no startup wiring.
- **Bypass alias resolution.** `AdvisorConfig.model` should be an explicit
  **upstream** model name, written directly into the advisor's own
  `ResponsesRequest.model`. `RuntimeMappings::resolve` (`src/tui/runtime.rs:57`)
  runs on *inbound client* names at `src/proxy.rs:190`, **before** translation;
  an advisor model is not client-supplied. This also decouples from
  `MappingsStore`, `mappings_snapshot`, and the entire fallback path.
- **Skip `capabilities.rs` entirely.** `RequestParam::ALL = [Temperature,
  TopP]`; the advisor request carries neither, so `apply_known` is a no-op,
  `param.is_set(&outbound)` is false, and `record_unsupported` can never fire.
  §5.5's "reuse, do not extend" recommendation is **unnecessary** — confirmed
  from `src/capabilities.rs:83-97, 136-151` and `src/proxy.rs:442-445`. Zero
  edits, zero coupling.
- **Fail closed.** `detect_model_rejection` (`src/model_rejection.rs:245`) is
  deliberately conservative because a false positive *silently retries with a
  different model* (`:33-40`). The fallback at `src/proxy.rs:532-538`
  re-translates the **executor's** `CreateMessageRequest`, sets
  `fallback_used = true`, and consumes the executor's one-shot `attempt == 1`
  budget. Applying it to an advisor call would (i) turn the second opinion
  into a self-review, (ii) discard the advisor's own prompt, (iii) corrupt
  stats attribution, (iv) burn the executor's fallback. **The precedent for an
  upstream call that must not fall back is the hidden validation retry
  (`src/proxy.rs:603-626`):** a separate `send_upstream_once` outside the loop,
  its own bounded counter, `break` and keep the already-good response on
  non-2xx, structured `tracing::warn!`. Copy that shape exactly.
- **Error handling:** there is **no** graceful-degradation helper and no
  non-fatal `Result` convention. `AppError` is exclusively fatal. The house
  pattern is `tracing::warn!` with structured fields + a best-effort value +
  continue (10 examples, e.g. `src/translate.rs:5235-5242`, `src/proxy.rs:619-623`).
  `state.output.emit(LogLine { kind: Warning, … })` is synchronous, safe from
  async, and used once-per-fact at `:457-465`. **Never `?`-propagate an
  `AppError` from the advisor.**
- **Usage:** `Usage` (`src/anthropic.rs:392-410`) has no `Default`, no
  `iterations`, and **6** explicit literals (`src/stream.rs:362, 461, 474,
  1050`; `src/translate.rs:5258, 5283`). Adding
  `#[serde(default, skip_serializing_if = "Option::is_none")]`
  `iterations: Option<Vec<IterationUsage>>` is ~20-30 lines + 6 one-line
  fixups. Advisor tokens must **not** be folded into top-level totals; Claude
  Code reads `iterations` to exclude `advisor_message` from context-size math.
  TUI attribution: `state.stats.record_actual(advisor_model, usage)` is 1 line
  and lands in "ALL COMBINED", but the TUI labels that bucket *"actual fallback
  models"* (`src/tui/app.rs:757`) — semantically wrong. A proper third bucket
  is ~40 lines; **rename the label to "actual / advisor models"** (1 line) is
  the honest 80/20.
- **Tests:** 355 in-file `#[test]`s + 37 `#[tokio::test]`s in the single
  `tests/proxy_e2e.rs`. `FakeUpstream` (`tests/proxy_e2e.rs:27-122`) already
  has **`canned_per_attempt`, `received_all` and `delay`** — exactly what a
  3-call advisor chain and a timeout/fail-closed test need. No new test
  infrastructure.

### 11.5 The protocol, corrected and pinned

The biggest correction to §§1-10: **one consultation is ONE assistant
message, not two.** `server_tool_use` and `advisor_tool_result` are contiguous
blocks in the **same** `content[]`, with optional executor text on both sides,
and `stop_reason: "end_turn"`. `usage.iterations[]` is a list of **sampling
iterations inside one API request**, not a list of messages. Streaming is
**one** `message_start` … **one** `message_stop`; the advisor sub-inference
does not stream; its result arrives whole in a single `content_block_start`
with **no deltas**. `pause_turn` is only for a *dangling* `server_tool_use`
with no result — **a proxy must never emit it**.

Verbatim wire shape (Anthropic docs, response-structure example):

```json
{"role":"assistant","content":[
  {"type":"text","text":"Let me consult the advisor on this."},
  {"type":"server_tool_use","id":"srvtoolu_abc123","name":"advisor","input":{}},
  {"type":"advisor_tool_result","tool_use_id":"srvtoolu_abc123",
   "content":{"type":"advisor_result","text":"Use a channel-based coordination pattern…"}},
  {"type":"text","text":"Here's the implementation…"}],
 "stop_reason":"end_turn"}
```

Corrections and confirmations:

| item | §1-10 said | wave 2 established |
|---|---|---|
| messages per consult | unclear / "later turn" | **one**; both blocks in the same `content[]` |
| `stop_reason` | `pause_turn` suspected | **`end_turn`** (or `tool_use` iff a *client* tool is pending) |
| `iterations[]` | billing, separate | **sampling iterations in one request**; on final `usage` + `message_delta.usage` |
| beta header | `advisor-tool-2026-03-01` ✓ | ✓ **request** header; `advisor_20260301` is the tool `type` |
| response beta echo | unknown | **none needed** (proven by exhaustive absence) |
| inbound echo | assumed | **PROVEN**: Claude Code echoes both blocks verbatim; `ContentBlockParam::Unknown` currently drops them |
| `srvtoolu_` prefix | unknown | real prefix; Claude Code does **no** format validation — only non-empty + equal `tool_use_id` |
| error codes | 6 client-known | ✓ plus `model_not_found` in SDK, which renders `unknown`; **use `unavailable`** |
| `advisor_stripped` | unknown | a Claude Code **internal attachment**, not a wire block — do not emit |

**Claude Code's renderer is per-block, not per-message-index** (binary byte
321650852): `server_tool_use` → "Advising …", `advisor_result` → "✓ Advisor has
reviewed the conversation …" (Ctrl+O to read), error → `Advisor unavailable
(<error_code>)`, `stop_reason:"refusal"` → "Advisor declined to advise". It
tracks pairing via **id string sets**, not positions — but the **API** requires
the pair contiguous in one assistant message (real 400s in Claude Code issues
#62885, #95406, #56515, #60523: *"Each `advisor_tool_result` block must have a
corresponding `server_tool_use` block before it"*).

**Never return a 400 naming `advisor-tool-2026-03-01`, `advisor_20260301`, or
`Input tag 'advisor_…'`** — Claude Code latches the advisor **off for the whole
process** (binary bytes 296188621 / 294902915). This makes the generic
`unavailable` degradation not just graceful but mandatory.

**Inbound echo is the one hard wire-type change.** The SDK TypedDicts are:
`BetaServerToolUseBlockParam{id, input, name, type, cache_control?, caller?}`,
`BetaAdvisorToolResultBlockParam{content, tool_use_id, type, cache_control?}`,
where `content` is a 3-way union (`advisor_result{text,stop_reason?}`,
`advisor_redacted_result{encrypted_content,stop_reason?}`,
`advisor_tool_result_error{error_code}`). Keep `content` as an opaque
`serde_json::Value` so all variants and future fields round-trip byte-for-byte.

### 11.6 A verified trick that removes most wire-type churn

`ResponseContentBlock` (`src/anthropic.rs:310-343`), `ContentBlockKind`
(`:484-513`) and `StreamEvent` are **`Serialize`-only** internally-tagged
enums. A probe compiled against this repo's serde version confirmed that an
`#[serde(untagged)] Raw(Value)` variant on an internally-tagged enum
serialises a raw JSON object **verbatim**, including its own `type` field:

```rust
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ResponseContentBlock {
    Text { text: String },
    // …
    #[serde(untagged)] Raw(Value),
}
```

serialises to `{"type":"advisor_tool_result","tool_use_id":"…",
"content":{…}}` exactly. **⇒ Add ONE generic, permanent extension point to
each enum** (2 lines × 2 enums) and the advisor module constructs its own
blocks as `Value`. No per-feature enum variants, no new match arms in
`translate.rs`/`stream.rs`, and no future churn for the next Anthropic block
type. This is the key to keeping the feature out of the shared wire types.

### 11.7 Where the earlier estimates were wrong

| §1-10 estimate | corrected |
|---|---|
| option A ~1,000 lines, 6-7 files, streaming splice | the splice is **avoidable**; the real cost is the 150-250-line `proxy.rs` loop extraction + ~75-100-line SSE adapter |
| `src/config.rs` ~90 lines for advisor config | ~125-150 across 6 files; **avoid entirely** with module-local config |
| `Usage.iterations` ~110 lines in `anthropic.rs` + 6 fixups | ~20-30 lines + 6 one-line fixups (or 0 with a raw-`Value` envelope) |
| capabilities: "reuse, do not extend" | **skip entirely** — nothing learnable |
| `Tool` gains `type`/`model` (4 fixture fixups) | **not needed** if the module intercepts by `name == "advisor"` |
| 2 messages per consultation | **1 message** |
| "the advisor must be injected into the upstream prompt itself" | true, but we can do it in the module's own `ResponsesRequest`, not in `translate.rs` |

---

## 12. Implementation plan — modular, no new tech debt

### 12.0 Design principles (non-negotiable)

1. **Off by default.** When `enabled = false`, the request path is
   byte-for-byte what it is today: no extra call, no field change, no log
   line. The only shared-code edits are a single early branch and a
   `pub(crate)` visibility word.
2. **One private module.** `mod advisor;` in `src/lib.rs` (the
   `model_rejection.rs` precedent). Public API takes plain data / existing
   types, returns plain data. No `pub use` unless e2e tests need it.
3. **No `Config`, no `AppState`, no `Tool` changes.** Module-local config in a
   `OnceLock`; `state.client.clone()`; intercept the tool by name.
4. **No `stream.rs` changes.** Force non-streaming upstream; emit synthetic
   SSE from the final `Message`.
5. **Fail closed and bounded.** Own timeout, own `max_uses` counter, no
   `default_model`, never `?`-propagate `AppError`; degrade to
   `advisor_tool_result_error{"unavailable"}` on **any** failure.
6. **Two-layer tests** (in-file unit + `tests/proxy_e2e.rs`), following the
   house pattern; pin the test count in the commit message.

### 12.1 Module layout

```
src/advisor/
  mod.rs        //! docs: off-by-default, fail-closed, single-message protocol
                pub(crate) fn enabled() -> bool
                pub(crate) fn try_handle(...) -> Option<Response>   // THE one hook
                pub use {Config, Outcome};
  config.rs     AdvisorConfig { enabled:false, model, effort, max_uses:2,
                timeout_secs:45, max_tokens:2048 } + load() -> OnceLock
                (own file / ADVISOR_* env; NOT proxy.json — deny_unknown_fields)
  protocol.rs   pure wire constructors: advisor_tool(), server_tool_use(),
                advisor_tool_result(), error_result(), iteration_usage(),
                srvtoolu_id(); all as serde_json::Value
  prompt.rs     build the advisor's ResponsesRequest: system role, the
                transcript as one user message, no tools, stream:false,
                temperature/top_p:None, model = explicit upstream name
  upstream.rs   consult(client, cfg, base_url, api_key, request)
                -> Result<Advice, AdvisorError>; one POST with .timeout()
  session.rs    the outer loop: executor#1 → detect advisor call → consult →
                inject advice into the transcript → executor#2; bounded by
                max_uses; fail-closed
  sse.rs        message_to_events(&Message) -> Vec<StreamEvent> (the ~70-line
                adapter), using the Raw(Value) extension point
  tests.rs      inline #[cfg(test)] mod tests (or per-file inline modules)
```

Why a directory here (unlike the flat-file house style): 6-7 focused files,
~600-800 lines total, each with a single responsibility. `src/tui/mod.rs` is
the precedent for the `mod.rs` + `pub use` shape. If it stays under ~400 lines,
collapse to one flat `src/advisor.rs` instead — do not create a directory for
its own sake.

### 12.2 The ONE integration hook

`src/proxy.rs`, at the top of `handle_messages` (`:160`), **before** the
translation and before `handle_messages_inner`:

```rust
if let Some(resp) = crate::advisor::try_handle(&state, &headers, &body, &req).await {
    return resp;                 // Ok(Response) | Err(AppError)
}
// … existing path, completely unchanged …
```

`try_handle` returns `None` immediately when disabled or when the request has
no tool named `advisor`. This is the **entire** shared-code footprint: one
branch, plus `pub(crate)` on `encode_sse_events`, `send_upstream_once` and
`parse_upstream_body` (3 words), plus `mod advisor;` (1 line). The existing
retry/fallback/validation/capability machinery is **not modified** — the
advisor session owns its own simpler loop, modelled on
`src/proxy.rs:603-626`.

`try_handle` reuses `translate::anthropic_to_responses` and
`translate::responses_to_anthropic` via a small `pub(crate)` wrapper (or by
making those two functions `pub(crate)` — they are already `pub` inside a
private `mod translate`, so this is free). It builds its own `ResponsesRequest`
for the advisor and calls the same `send_upstream_once`.

### 12.3 Session state machine (the core)

State is **per-request**, held in one struct — no `AppState`, no global mutable
state beyond the immutable `OnceLock<Config>`:

```rust
struct Session<'a> {
    state: &'a AppState,
    inbound: &'a CreateMessageRequest,
    outbound: ResponsesRequest,           // executor's, translated once
    registry: ToolSchemaRegistry,
    cfg: &'static AdvisorConfig,
    consultations: usize,
    iterations: Vec<Value>,               // usage.iterations[]
    blocks: Vec<Value>,                  // advisor blocks for final content[]
}
```

Loop, bounded by `cfg.max_uses`:

1. **Executor attempt.** `send_upstream_once` (non-streaming; `outbound.stream
   = false`), `parse_upstream_body`. On any failure, degrade: append
   `error_result("unavailable")` and **return the last good executor answer**.
2. **Detect a consultation** in `ResponsesResponse.output`: an
   `OutputItem::FunctionCall { name: "advisor", .. }`. If none → done.
3. **Emit the pair.** `srvtoolu_id()` → `server_tool_use` +
   `advisor_tool_result` (advice text, or `unavailable` on failure). Both are
   `Value` blocks appended to `Session.blocks`, contiguous.
4. **Consult.** Build the advisor's own `ResponsesRequest` (explicit upstream
   model, `.timeout(cfg.timeout)`, no tools, no sampling params, `stream:false`),
   POST, parse, extract the first `output_text`. On **any** error:
   `tracing::warn!(error = %e, advisor_model = %cfg.model, "…")` +
   `error_result("unavailable")`, then **continue** to the final executor call.
5. **Inject the advice into the executor's context** — this is the part the
   Responses API cannot do server-side. Append one `InputItem::Message` with
   the advice text (system-role instructions are folded by the translator, so
   use a plain transcript message) to `outbound.input`, then re-send.
6. **Accumulate usage** into `iterations`: `message` (executor) /
   `advisor_message` (with `model`) / `message` (final executor), keeping the
   advisor's tokens **out** of the top-level totals.
7. **Final response** = `responses_to_anthropic(final_response, &registry)`
   with `content` = `[…executor text…, advisor blocks…, …final text…]` and
   `usage.iterations = Some(iterations)`, `stop_reason: end_turn`.
8. If the client asked for `stream: true`, convert with
   `sse::message_to_events` and return `text/event-stream`; otherwise return
   `Json(message)`.

**Never** call `detect_model_rejection`, `mappings_snapshot.default_model()`,
`capabilities.*`, or set `fallback_used`. A 404 on the advisor model is an
advice failure, not a routing decision.

### 12.4 Shared wire-type edits (the minimal set)

| file | edit | lines |
|---|---|---|
| `src/lib.rs` | `mod advisor;` | 1 |
| `src/proxy.rs` | the early `try_handle` branch; `pub(crate)` × 3 on `encode_sse_events`, `send_upstream_once`, `parse_upstream_body` | ~6 |
| `src/anthropic.rs` | `#[serde(untagged)] Raw(Value)` on `ResponseContentBlock` **and** `ContentBlockKind` (generic extension point — verified to work in §11.6) | 4 |
| `src/anthropic.rs` | `iterations: Option<Vec<Value>>` on `Usage` + `#[serde(default, skip_serializing_if = …)]` | 2 |
| `src/stream.rs` | **6** `iterations: None,` one-line fixups on the `AnthropicUsage {}` literals (`:362, 461, 474, 1050`) | 6 |
| `src/translate.rs` | **2** `iterations: None,` (`:5258, 5283`); 1 match arm for `Raw(Value)` if any exhaustively matches these enums; **keep the name `advisor`** so HEAD's `strip_advisor_tool_instructions` guard (`:153-159`) preserves the instructions; update that helper's now-stale doc comment | ~6 |
| `src/anthropic.rs` | `ContentBlockParam::{ServerToolUse, AdvisorToolResult}` with opaque `Value` + `#[serde(flatten)]` catch-all, so inbound echo round-trips verbatim | ~14 |
| `src/translate.rs` | **no new `filter_map` arm needed** — session intercepts by name; but verify the declaration is not forwarded upstream as an unknown tool type | 0-2 |
| `src/tui/app.rs` | rename `"actual fallback models"` → `"actual / advisor models"` | 1 |
| `proxy.json-guide.md` | document the separate advisor config file + `ADVISOR_*` env | ~20 |

**Total shared-code churn: ~40-60 lines across 7 files**, all of it either a
one-line field, a visibility word, or a generic extension point. No control
flow in `handle_messages_inner`, `TranslatorStream`, `StreamTranslator`, or
`Config::load` is touched. The advisor module itself is ~600-800 lines +
~250 lines of tests.

### 12.5 Config shape (module-local, off by default)

```json
// advisor.json — NOT proxy.json (JsonConfig has deny_unknown_fields)
{
  "enabled": false,
  "model": "beam",              // explicit UPSTREAM name; cheap (§7)
  "effort": "medium",
  "max_uses": 2,
  "max_tokens": 2048,
  "timeout_secs": 45
}
```

`ADVISOR_ENABLED`, `ADVISOR_MODEL`, … override the file, same **env > file >
default** precedence as `Config::load` (`src/config.rs:679-681`). Load once
into `OnceLock<AdvisorConfig>`. **`enabled: false` means zero wire-field
changes** — the §12.2 branch returns `None` before anything is built.
Document that `proxy.json` must **not** contain an `advisor` key (an unedited
binary would refuse to start).

Use **`beam`** while reverse engineering (§7): $0.125/M in, $0.40/M out, 25×
cheaper on output than `kimi-k3.1-pro`, and it accepts `xhigh` natively.

### 12.6 Verification plan (house two-layer pattern)

**In-file unit tests (`src/advisor/`):** config default/precedence; disabled →
`try_handle` returns `None`; `build_advisor_request` asserts no `temperature`,
no `top_p`, `stream:false`, explicit model, no tools; all 3 result-content
variants + `unavailable` error; id pairing; `iteration_usage` shape;
`max_uses` bound; every `AdvisorError` → `unavailable`; `message_to_events`
asserts exactly one `message_start`, contiguous indices, `message_delta` with
`stop_reason`, one `message_stop`, no `[DONE]`, hard-coded SSE content type.

**E2E (`tests/proxy_e2e.rs`, using existing `canned_per_attempt` /
`received_all` / `delay`):**
1. disabled → identical request/response to today, **0 extra POSTs**;
2. happy path → 3 POSTs (executor, advisor, executor), assert each body's
   `model`, the advice text injected into attempt 3, and the final `content[]`
   block order + `usage.iterations`;
3. `stream:true` → `Content-Type: text/event-stream`, full event sequence,
   upstream saw `stream:false`;
4. advisor 404 / timeout (`delay` > `timeout_secs`) → executor's answer still
   arrives, `error_code:"unavailable"` present, **no `default_model` POST**;
5. `max_uses` cap → no 4th POST;
6. next-turn echo → inbound `server_tool_use` + `advisor_tool_result` survive
   translation (the `ContentBlockParam` change);
7. a non-advisor request → all 355 + 37 existing tests still pass unchanged.

Also replay the 84 real corpus `instructions` strings from §4.2 to confirm the
enabled path **preserves** the `# Advisor Tool` section (the guard keys on the
inbound tool name).

### 12.7 Build order / risk gates

| step | deliverable | gate |
|---|---|---|
| 1 | `src/advisor/{mod,config,protocol}.rs` + wire extension points; `try_handle` returns `None` always | full suite green; **zero behaviour change** |
| 2 | `prompt.rs` + `upstream.rs`, non-streaming happy path (1 consult) behind `enabled` | e2e tests 1-2; no streaming yet |
| 3 | `sse.rs` + `message_to_events`, `stream:true` support | e2e test 3; Claude Code smoke test with `--advisor opus` |
| 4 | failure paths, `max_uses`, `usage.iterations`, TUI label | e2e tests 4-6 |
| 5 | inbound echo (`ContentBlockParam`) + next-turn smoke test | e2e test 6; multi-turn Claude Code session |
| 6 | docs, `proxy.json.example` note, clean `cargo clippy` baseline | review |

Each step is a separate commit. **Stop after step 3** if the smoke test shows
Claude Code does not render the consultation — the block plumbing is worth
keeping but the orchestration is not.

### 12.8 What this design deliberately does NOT do

- No true incremental streaming across two upstream responses (§11.3).
- No `default_model` fallback for the advisor, ever (§11.4).
- No `advisor_redacted_result` — the `encrypted_content` blob is
  Anthropic-server-encrypted and cannot be manufactured (§3.3).
- No `pause_turn` (§11.5).
- No `anthropic-beta` forwarding to the **upstream** (it is not Anthropic), and
  no 400 that names the beta/tool (would latch the feature off client-side).
- No `Config`/`AppState`/`Tool`/`stream.rs` control-flow changes.
- No capability learning for the advisor (nothing learnable).

### 12.9 Open items to settle before step 2

1. **Does the executor model reliably emit a `function_call{name:"advisor"}`?**
   The advisor declaration must be forwarded as a zero-argument function tool,
   with a description that invites consultation. Cheap-model behaviour here is
   unproven — spike it with `beam` before building the loop. If the model never
   calls it, use option B (proactive pre-turn review) instead: same module,
   simpler session (no detection, always one consult).
2. **How to inject advice into the executor's transcript.** The Responses API
   cannot resume a generation. `append_corrective_input`
   (`src/repair.rs:570`) is the existing precedent for appending a synthetic
   input item; mirror its shape. Confirm the upstream accepts an extra
   `message` item after a `function_call` without a matching
   `function_call_output` — if it rejects, emit a synthetic
   `function_call_output` first (the advisor's advice as the tool result).
3. **Claude Code's `--advisor` gate** (§9.2): the standby advisor needs
   `api.anthropic.com`, so the operator **must set the advisor explicitly**.
   A proxy cannot fix this; document it.
4. **`tengu_sage_compass2` flag value** — remote, not statically resolvable,
   but an explicit advisor model bypasses it (§9.3).

---

## Sources

**Official docs**
- [1] Advisor tool — Claude Platform Docs — https://platform.claude.com/docs/en/agents-and-tools/tool-use/advisor-tool
- [2] Escalate hard decisions with the advisor tool — Claude Code Docs — https://code.claude.com/docs/en/advisor
- [3] The advisor strategy — https://claude.com/resources/articles/the-advisor-strategy
- [4] `beta_advisor_tool_20260301_param.py` — Anthropic Python SDK — https://github.com/anthropics/anthropic-sdk-python/blob/main/src/anthropic/types/beta/beta_advisor_tool_20260301_param.py
- [5] `beta_advisor_tool_result_block.py` — Anthropic Python SDK — https://github.com/anthropics/anthropic-sdk-python/blob/main/src/anthropic/types/beta/beta_advisor_tool_result_block.py
- [6] `beta_advisor_tool_result_error.py` — Anthropic Python SDK — https://github.com/anthropics/anthropic-sdk-python/blob/main/src/anthropic/types/beta/beta_advisor_tool_result_error.py
- [7] `beta_advisor_message_iteration_usage.py` — Anthropic Python SDK — https://github.com/anthropics/anthropic-sdk-python/blob/main/src/anthropic/types/beta/beta_advisor_message_iteration_usage.py

**Pricing**
- [pricing] Ollama Cloud pricing — https://ollama.com/pricing
- [ollama-tps] Ollama Cloud pricing mirror — https://ollamatps.com/pricing/
- [sovyron] Ollama Cloud models & pricing — https://sovyron.com/providers/ollama-cloud/

**Wave-2 additions (2026-11-14)**
- [8] Server tools — Claude Platform Docs — https://platform.claude.com/docs/en/agents-and-tools/tool-use/server-tools
- [9] Streaming messages — Claude Platform Docs — https://platform.claude.com/docs/en/build-with-claude/streaming
- [10] Anthropic TypeScript SDK `helpers.md` — https://github.com/anthropics/anthropic-sdk-typescript/blob/main/helpers.md
- [11] Advisor tool (docs mirror) — https://doc.jarvisuni.com/claude/api/en/agents-and-tools/tool-use/advisor-tool
- [12] `beta_advisor_tool_result_block_param.py`, `beta_advisor_result_block_param.py`, `beta_advisor_redacted_result_block_param.py`, `beta_advisor_tool_result_error_param.py`, `beta_server_tool_use_block_param.py`, `beta_iterations_usage.py`, `beta_usage.py`, `beta_message_delta_usage.py` — https://github.com/anthropics/anthropic-sdk-python/tree/main/src/anthropic/types/beta/
- [13] Claude Code issue #62885 (orphaned `advisor_tool_result` 400) — https://github.com/anthropics/claude-code/issues/62885
- [14] Claude Code issue #95406 — https://github.com/anthropics/claude-code/issues/95406
- [15] Claude Code issue #56515 — https://github.com/anthropics/claude-code/issues/56515
- [16] Claude Code issue #60523 — https://github.com/anthropics/claude-code/issues/60523
- [17] `reqwest::Client` docs (cheap to clone) — https://docs.rs/reqwest/latest/reqwest/struct.Client.html
- [18] `reqwest::RequestBuilder::timeout` docs — https://docs.rs/reqwest/latest/reqwest/struct.RequestBuilder.html

**Local evidence**
- Claude Code binary `/opt/claude-code/bin/claude` (2.1.322) — byte offsets cited inline
- Proxy corpus `/tmp/proxylogs/` — 27 files, 539 MB, 2026-08-15 → 2026-11-12
- Ollama `POST https://ollama.com/api/show` (public, no auth) — per-model `thinking` vocabulary
- Repo at HEAD `df5d78e` — all §11/§12 `file:line` references are against this commit
- Serde `#[serde(untagged)] Raw(Value)` probe — compiled against this repo's serde version; scratch at `/home/heylon/.cache/serde-probe/`
