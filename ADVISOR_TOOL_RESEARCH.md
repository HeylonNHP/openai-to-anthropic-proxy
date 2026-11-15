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
> and supersedes them where they conflict. **§12's bypass design is challenged
> and partly retracted by §13** — read §13 before building. §14 is the
> recommended sequencing; **§15 corrects who actually calls the tool** (the
> executor, not `beam`) and removes the go/no-go framing. Read §15 too.

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

> ⚠️ **Partly retracted 2026-11-15.** §13's duplication audit found that the
> `try_handle` bypass must copy ~290-330 lines of proxy policy (auth, alias,
> caching, retries, fallback, stats, logs) — more than the 150-250-line
> refactor it was designed to avoid — and that no existing test enters the
> enabled path. §12.2/§12.3/§12.4/§12.7 are superseded by §13 and §14.
> §11.3 (buffer, don't splice), §11.5 (one assistant message), §11.6
> (`Raw(Value)`), §12.5 (module-local cheap config), §12.6 (test matrix) and
> §12.8 (what not to build) still stand.

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

## 13. Challenge: would refactoring the pipeline FIRST be cleaner? (2026-11-15)

> Two read-only sub-agents audited §12's bypass design against HEAD `8d378cf`:
> a **duplication audit** (what the bypass would have to copy) and a
> **refactor verdict** (what a behaviour-preserving extraction looks like).
> This section corrects §12. It is not a repudiation of the whole design —
> §11.3 (buffer, don't splice), §11.5 (one assistant message) and §11.6
> (the `Raw(Value)` extension point) all survive.

### 13.1 The uncomfortable finding: the bypass is a second proxy

§12.2 claimed ~6 lines of shared-code churn. That is **only true because
~290-330 lines of proxy policy move into the private advisor module**. Every
cross-cutting behaviour below lives in `handle_messages` / `handle_messages_inner`
and would have to be replicated for the enabled path to behave the same:

| behaviour | file:line | if omitted |
|---|---|---|
| `check_proxy_key` | `src/proxy.rs:169` | **security hole** — §12.2 puts the hook at `:160`, *before* auth |
| inbound JSON parse + `BadRequest` | `:171-172` | §12.2 passes `&req`, which does not exist yet; must re-parse or move below |
| `mappings.load_live()` snapshot | `:180` | a TUI edit mid-request tears the alias/fallback view |
| alias `resolve` | `:190` | alias silently ignored |
| unmapped-model warning + `log_safe` | `:195-224` | operator-visible warning vanishes; log-injection fix from `b0d5629` lost |
| `reasoning_for_request` | `:237-240` | wrong/omitted effort |
| `prompt_caching_for_model` | `:241` | **no cache breakpoints** → visible token cost |
| `reasoning_replay_for_model` | `:242` | prior `thinking` blocks silently dropped |
| `outbound.model` / `prompt_cache_key` | `:255`, `:260-262` | wrong model, no cache key |
| `→ upstream` log + `replayed_chars` | `:266-281` | the source-reading guard test at `:1195` reads **the one** statement in `proxy.rs`; a copy is invisible to it |
| TUI `Inbound` log | `:283-305` | empty log pane |
| `capabilities.apply_known` | `:405-412`, `:548` | known-bad params re-sent, repeated 400s |
| URL build + outer send loop + payload logs | `:381-432` | URL builder is private and **not** in §12.4's `pub(crate)` list |
| `unsupported_parameter` strip retry | `:442-470` | 3 e2e tests pinned at `tests/proxy_e2e.rs:2457, 2557, 2591` |
| `default_model` fallback (re-translates) | `:482-552` | **user-visible**: an *executor* model rejection would degrade to `unavailable` instead of being answered |
| `map_upstream_error` | `:554`, `:1031-1055` | envelope parsing + `kind: message` lost |
| hidden corrective retry | `:603-626` | `tests/proxy_e2e.rs:2341` |
| stats + `stats_model` + `fallback_used` | `:629-638`, `:679-712` | TUI totals wrong |
| `←` log + JSON response | `:639-674` | log format shared with `print_stream_stats` `:714-752` |

§12.3 explicitly forbids `detect_model_rejection`, `default_model()` and
`capabilities.*` (`:1084`) — but that rule was meant for the **advisor's own
call**, not the executor's. §12.3 applies it to the whole session, so the
executor loses its fallback. **§12 contradicts itself here.**

Full parity would need **~106 preamble lines + ~180-220 inner lines =
~290-330 duplicated lines**, plus `pub(crate)` on ~8 more private helpers
(`:73`, `:679`, `:700`, `:706`, `:714`, `:1031`, `:1064`, `:1109`) — §12.4
budgeted **3**. That **exceeds** the 150-250-line refactor estimate.

**And the existing tests do not catch it.** All 37 e2e tests drive
`proxy::router(...)` (`tests/proxy_e2e.rs:220-224`), so they do pass through
`handle_messages`, but `try_handle` returns `None` when disabled and no test
enables the advisor. §12.6 item 7 celebrating "all 355 + 37 existing tests
still pass unchanged" is exactly the symptom: **they pass because they never
enter the enabled path.**

### 13.2 Evidence that duplication drifts in THIS codebase

The policy is **already duplicated once** — first attempt `:229-262` vs the
fallback re-translation `:496-544`. They currently agree only because every
change was hand-applied twice:

- `24f4805` added `reasoning_replay_for_model` to **both** call sites.
- `f507d59` had to separately fix `reasoning_replay` → `&reasoning_replay`
  **and** `fallback_reasoning_replay` → `&fallback_reasoning_replay`.
- `b0d5629` manually rewrote the fallback's `reasoning_for_request` and
  re-applied the `log_safe` injection fix there.
- **Real asymmetry already shipped:** `8fde2d4` added `replayed_chars` only
  to the first-attempt log; `:277-278` documents that a fallback's replay
  counter is never reported.

Three dedicated e2e tests exist **solely** to guard this one duplication:
`fallback_recomputes_reasoning_for_default_model` (`:1650`),
`fallback_recomputes_prompt_caching_for_default_model` (`:1726`),
`fallback_recomputes_reasoning_replay_for_default_model` (`:1806`). A second
pair is `:629-673` (JSON log) vs `print_stream_stats :714-752` (stream log).

**So a third copy is not a theoretical concern — this repo has already paid
for the second one.**

### 13.3 The refactor is mechanical, not a rewrite

A behaviour-preserving extraction is smaller than §11.2 implied:

```rust
struct Attempt {                 // the 7 values already moved at :307-315
    state: AppState, headers: HeaderMap, req: CreateMessageRequest,
    outbound: ResponsesRequest, registry: ToolSchemaRegistry,
    start: Instant, mappings_snapshot: Arc<RuntimeMappings>,
    fallback_used: bool, param_retries: usize, attempt: u8,
}
enum Outcome { Streaming(reqwest::Response), Buffered(ResponsesResponse) }

fn build_outbound(req, config, model) -> Result<(ResponsesRequest, Registry, u64)>;
async fn send_success(&mut self) -> Result<reqwest::Response, AppError>;
async fn execute(&mut self) -> Result<Outcome, AppError>;
async fn run(self) -> Result<Response, AppError>;
```

- `:229-249` + `:253-262` + `:496-544` collapse into **one** `build_outbound`
  — which also **eliminates the existing duplication** and its 3 guard tests.
- `:393-555` → `send_success`; `:594-626` → `execute` (composes the existing
  `send_upstream_once` `:321` + `parse_upstream_body` `:348`).
- `:560-592` → `render_streaming`; `:628-674` → `render_json`.
- **§11.2's "`req` is moved at `:310`" objection is wrong**: `:496-503` and
  `:532-538` already borrow `&req` for fallback re-translation. Store it once
  in `Attempt`; no clone. (`CreateMessageRequest` is `Clone` but not
  `Serialize`, `src/anthropic.rs:26`; `ResponsesRequest` is `Clone + Serialize`,
  `src/responses.rs:30`.)
- Keep the `→ upstream` log statement **in `proxy.rs`** — the test at `:1195`
  does a source-level `rfind` for it.
- Watch two lifetime details: `ReasoningReplay` holds `Cell<u64>`
  (`src/config.rs:277-284`) so it is `!Sync` — read `replayed_chars` before
  `run`; and `Outcome::Streaming` must return the **raw** `reqwest::Response`
  so `bytes_stream()` still works (`src/proxy.rs:560-567`).

Invariants to pin: the three bounded counters (`:400`, `:401`, `:603`), the
one-shot `attempt == 1` fallback, `apply_known` before first send and after
fallback only, validation retry being non-streaming-only and keeping the first
success on retry error, `fallback_used` stats attribution, `Drop` stats, and
the log order.

### 13.4 Safety net — mostly adequate, with gaps

14 in-file unit tests (`src/proxy.rs:1124-1309`) + 37 e2e tests. Retry order
is pinned by `unsupported_parameter_retries_without_it_and_remembers` (`:2457`),
`rejects_both_sampling_params_one_at_a_time` (`:2591`),
`invalid_tool_input_triggers_corrective_retry` (`:2341`); fallback by
`model_not_supported_falls_back_to_default` (`:792`),
`fallback_model_also_rejected_returns_the_second_error` (`:1210`),
`streaming_model_not_supported_falls_back_to_default` (`:1563`).

**Enough for bodies/retry/fallback; not enough for a hot-path refactor.** Add
FIRST, as characterization tests: (1) a `retry_order_matrix` diffing
`received_all` through unsupported-param → fallback → success; (2) corrective
retry after fallback keeping the first success; (3) fallback usage + log
attribution via `router_with_stores` + captured `OutputSink`; (4) client
disconnect after a terminal stream recording exactly once.

### 13.5 The real trade-off, in plain terms

| | A: refactor first | B: bypass (§12) |
|---|---|---|
| lines | 150-250 `proxy.rs` + 75-100 SSE + advisor loop | 600-800 module + ~290-330 **duplicated** policy lines |
| blast radius | **every** request through `:362-676` | disabled path untouched |
| future policy edit | **1 place** (and the existing 2 become 1) | **3+ places**; history shows paired edits drift |
| delete on failure | remove advisor; `Attempt` remains as a net simplification | delete module + hook + revert `Raw(Value)`, `iterations`, `ContentBlockParam`, 6 `stream.rs` literals, TUI label, docs |

A generic middleware/trait/chain is **over-engineering here**: there is exactly
one real implementation of the request policy, and a trait would only obscure
raw-response ownership and the `Drop` stats hook.

### 13.6 Verdict — the answer depends on WHEN

**Yes, refactoring is the cleaner end state. No, doing it FIRST does not
reduce debt for this feature — because the feature is speculative.** The
core assumption is still unproven: **will a cheap model like `beam` even emit
`function_call{name:"advisor"}`?** (§12.9 item 1). If it never does, the whole
orchestration is dead weight — and a 150-250-line refactor of the hottest
function in the proxy, touching every user, would have been paid for nothing.

One clean pipe is better architecture **if the feature survives**. A deletable
experiment is better risk management **before it survives**. §12's mistake was
not choosing the experiment — it was calling the experiment "no tech debt"
when it is really **deferred, privately-owned debt**, and claiming 6 lines of
integration when ~300 lines of policy were simply moved out of sight.

---

## 14. Final recommended sequencing

1. **Spike the assumption OUTSIDE the proxy** (a 30-line script against a fake
   upstream): does the **executor** emit a call to a zero-argument `advisor`
   function tool when the `# Advisor Tool` instructions are present?
   *(Corrected by §15: this is NOT a go/no-go gate, and `beam` is the
   advisor, not the model being asked to call.)* A low invocation rate just
   means choosing design B (always consult) over A (wait for a call). Keep
   the cheap-advisor notes (§7) and the option-D fix (`df5d78e`).
2. **Add the 4 characterization tests** (§13.4). They are worth
   having regardless of the advisor — they pin retry order, fallback
   attribution and disconnect behaviour that are currently implicit.
3. **Refactor `handle_messages_inner` with zero behaviour change**, producing
   `Attempt` + `build_outbound` + `execute` (§13.3). This is **debt-reducing
   on its own**: it deletes the existing first-attempt/fallback duplication
   and its 3 guard tests. Gate: all 355 + 37 + 4 tests green, plus a
   differential check of received bodies and log lines.
4. **Fold the advisor in as 2-3 `Attempt::execute` calls** with per-attempt
   overrides (advisor model, shorter timeout, no fallback, no capability
   learning). One implementation of alias/effort/caching/retry/stats policy.
   Keep §11.3's buffered-SSE shape and §11.6's `Raw(Value)` wire extension.
5. **If a time-boxed bypass is genuinely needed** to validate step 1 inside
   the proxy, give it a **deletion deadline**, keep it feature-gated, make no
   permanent wire-type edits, and add the differential matrix (same requests
   enabled vs disabled, diff received bodies + responses + logs). Do **not**
   call it "consolidate later" — consolidate or delete at the deadline.

**Change the answer if:** the spike proves the tool is called reliably **and**
the feature will be enabled by default or maintained beyond one release —
then refactor first, because duplication stops being a temporary experiment
and becomes real long-term debt. Or, more simply: **if you already know you're
keeping the advisor, refactor now.** If you're still finding out, spike first.

*"Is a bypass cleaner?" → No. "Is it the right first move?" → Probably,
but only as a time-boxed experiment.*

---

## 15. Clarification: who calls the tool, and what §12.9 / §14.1 got wrong (2026-11-15)

Operator question: *"What do you mean by beam calling the tool? Beam doesn't
need its own advisor implementation — we make the advisor implementation,
and the model just calls it? Most agentic models use tools fine, and I might
not always use beam."*

**The operator is right. §12.9 and §14.1 conflated two different models.**

### 15.1 The cast (three roles, only two of them are models)

| role | who/what | job | does it call a tool? |
|---|---|---|---|
| **executor** | the model Claude Code's request resolves to | does the user's task; decides when to ask for help | **YES** — emits `function_call{name:"advisor"}` |
| **advisor** | a second, cheap model — `beam` in §7 | writes one paragraph of advice | **NO** — just generates text |
| **proxy** | our code | declares the tool, forwards the call, runs the advisor, feeds the advice back, emits the Anthropic blocks | n/a — it *implements* the tool |

`beam` was recommended in §7 as the **advisor** (the consulted model). It
never calls anything; it answers a plain text-generation request with no
tools at all. §12.9's "will a cheap model like beam even emit
`function_call{name:"advisor"}`" and "spike it with beam" wrongly attributed
the executor's job to the advisor. The "spike it with beam" phrase meant
"use beam to keep the whole test setup cheap", but read as — and was then
reasoned about as — a claim about beam's tool-calling ability. That is the
muddle.

### 15.2 The corrected risk

The executor **already calls ~12-16 function tools through this very proxy**
(`Bash`, `Read`, `Edit`, …; measured in the corpus). Tool calling is not a
novel capability being requested of it — the entire proxy depends on it.
Adding one more zero-argument function tool is not a capability question.
The operator's "most agentic models are smart enough" is correct, and stronger
than that: it is a **proven, load-bearing assumption of the existing system**.

What is genuinely uncertain is a **soft quality question, not a feasibility
gate**: how *often* will the executor choose to consult, given the
`# Advisor Tool` system-prompt section? That only decides **design A vs B**:

- **A — detection loop:** wait for `function_call{name:"advisor"}`, then
  consult. Depends on invocation rate.
- **B — proactive review:** always run the advisor once before the turn,
  then hand its advice to the executor. **Needs no tool call at all.**

So a low invocation rate never means "nothing to build" — it means "use
B". §12.9 already said this; §14.1's "**If no → stop**" contradicted it and
was simply wrong. That over-dramatic gate was also what made §13.6 call the
feature "speculative" enough to defer the refactor.

### 15.3 The real work is on the proxy side, not the model's

The concrete, model-independent work is translating the declaration.
Claude Code sends **exactly three fields** (§3.1):

```json
{"type":"advisor_20260301","name":"advisor","model":"claude-opus-5-5"}
```

No `input_schema`, **no `description`**. `src/translate.rs:185` does
`t.input_schema.clone()?` inside a `filter_map`, so **the declaration is
dropped today** and never reaches the upstream. The proxy must synthesize an
ordinary Responses function tool, e.g.:

```json
{"type":"function","name":"advisor",
 "description":"Consult a reviewer model before substantive work. Takes no arguments.",
 "strict":true,"parameters":{"type":"object","properties":{}}}
```

and confirm the empty schema survives `sanitize_tool_schema` + the upstream's
strict validator. The "invitation" to consult is **Claude Code's
`# Advisor Tool` system-prompt section** (kept in the enabled path by the
`df5d78e` guard, which keys on the inbound tool name), not the tool
description — though adding a description is a cheap belt-and-braces.
This is a ~10-line proxy change, fully under our control, with **zero
dependence on which model is behind the proxy**.

### 15.4 What swapping models actually changes

| knob | affects | does NOT affect |
|---|---|---|
| swap the **executor** (— may happen, per the operator) | how *reliably/often* the advisor tool gets called; general answer quality | whether the tool *can* be called |
| swap the **advisor** (`beam` → something else) | advice quality + cost (§7) | tool-call behaviour, at all |

A more capable executor tomorrow may consult more reliably — that is an
argument **for** building the mechanism once, not against it.

### 15.5 Effect on the §13 / §14 recommendation

- §13's core finding stands and is unaffected: a bypass duplicates
  ~290-330 lines of policy; one clean pipe is better. The refactor also
  deletes the existing `:229-262` / `:496-544` duplication.
- §13.6 / §14.1 overstated *why* to defer the refactor. The feature is not
  technically speculative — tool calling is proven. What is actually
  undecided is a **product decision the operator makes**: do you want this
  feature kept (enabled by default / maintained)? If yes, §13.6's own
  condition is already met — **refactor first**.
- Revised step 1: a small spike is still worth it, but as a **measurement**
  (invocation rate → A vs B), not a go/no-go gate. And B needs no spike.

---

## 16. Ready-to-implement contract (2026-11-15)

Asked: *"Would you be ready to implement the advisor tool into the proxy now?"*
**Yes.** Verified baseline before starting: HEAD `48a71d5`, working tree clean
except the pre-existing `?? .claude/`; every research anchor still in place
(`translate.rs:185` tool drop, `:153-158` instruction guard, `:292` stripper;
`proxy.rs:321/348/1000` helpers); no `advisor` module yet. **355 lib + 2 doc
+ 37 e2e tests pass**; `cargo clippy --all-targets` reports the same **7**
pre-existing warnings as the baseline. Builds use
`CARGO_TARGET_DIR=/home/heylon/.cache/proxy-target` (the /tmp tmpfs is ~81%
full).

### 16.1 Decisions locked for the first implementation

| decision | choice | why |
|---|---|---|
| architecture | **§13/§14: refactor first, one clean pipe.** `Attempt` + `build_outbound` + `execute`, then advisor = 2-3 `execute` calls | §15 removed the "speculative" reason to defer; bypass duplicates ~290-330 lines and duplicates an already-drifting policy |
| consultation trigger | **A: detection** — consult only on `function_call{name:"advisor"}`, honoring `max_uses`. B (always consult) is a config flag for later | §15.2: tool calling is proven; A is faithful to Anthropic and costs nothing when the executor doesn't ask |
| advisor model | **explicit UPSTREAM name in module-local `advisor.json`, default `beam`** | §7 (25× cheaper than kimi), §11.4 (no `Config` field — `deny_unknown_fields` + 17 literals + cycle) |
| streaming | **buffer upstream, emit synthetic SSE**; `src/stream.rs` untouched | §11.3; Anthropic's own advisor sub-inference does not stream |
| blocks | **one assistant message**, contiguous `server_tool_use` + `advisor_tool_result`, `stop_reason: end_turn`, never `pause_turn` | §11.5 (proven) |
| wire types | **generic `#[serde(untagged)] Raw(Value)`** extension point on `ResponseContentBlock` + `ContentBlockKind` | §11.6 (compiled probe) |
| failure | **fail closed**: no `default_model`, no capabilities, own timeout, `unavailable` error block, never `?`-propagate `AppError` | §11.4, §11.5 (a 400 naming the beta latches the feature off client-side) |
| default | **`enabled: false`** — zero wire-field changes on the normal path | §12.0 |

### 16.2 Commit sequence (each step independently green)

1. `Attempt` refactor of `handle_messages_inner` with **zero behaviour change**
   + the 4 characterization tests (§13.4). Gate: 355+2+37+4 green, clippy
   still 7, differential diff of received bodies + log lines.
2. `src/advisor/` skeleton: `mod`, `config` (OnceLock, off), `protocol`
   (block constructors as `Value`), wire extension points. `try/execute`
   still unreachable. Gate: full suite green, no behaviour change.
3. Zero-argument `advisor` function tool synthesized from the 3-field
   declaration; keep the `# Advisor Tool` instructions. E2E: upstream sees
   the tool, a `function_call{name:"advisor"}` round-trips.
4. Advisor upstream call + advice injection + `advisor_tool_result` blocks +
   `usage.iterations[]`. E2E: happy path (3 POSTs), failure (404/timeout →
   `unavailable`, executor answer intact, no fallback POST), `max_uses` cap.
5. Synthetic SSE for `stream:true` (hard-coded `text/event-stream`, one
   `message_start`/`message_stop`, `message_delta` with `stop_reason`). E2E +
   Claude Code `--advisor` smoke test.
6. Inbound echo (`ContentBlockParam` variants), TUI label, docs, `proxy.json.example`
   note. Multi-turn smoke test.

**Deletion rule:** if step 4 or 5 shows the feature is not worth keeping,
steps 2-6 revert cleanly and step 1 remains as a debt-reducing refactor.

### 16.3 Risks carried into the build

- Empty `{}` schema must survive `sanitize_tool_schema` + the upstream's
  strict validator (§15.3). Verify in step 3 before writing the loop.
- Advice must be injected as a synthetic input item that the upstream accepts
  after a `function_call` (§12.9 item 2); mirror `append_corrective_input`.
- `ReasoningReplay`'s `Cell<u64>` is `!Sync` — read `replayed_chars` before
  `Attempt::run` (§13.3).
- `Outcome::Streaming` must carry the **raw** `reqwest::Response` (§13.3).
- Keep the single `→ upstream` log statement in `proxy.rs`; the test at
  `proxy.rs:1195` reads the source (§13.3).
- Claude Code's standby advisor needs `api.anthropic.com`; the operator must
  set `--advisor` explicitly (§9.2). Not fixable by the proxy.

---

## 17. CONFIRMED PLAN (2026-11-15 13:18) — operator overrides §16

The operator confirmed the plan with four explicit decisions. **This section
is authoritative and overrides §12.5, §14.1 and §16.1.**

### 17.1 The four decisions

1. **Advisor config lives in `proxy.json` — NOT a separate `advisor.json`.**
   The operator rejected the module-local config file. So we DO add a config
   section to `src/config.rs` despite the cost measured in §11.4. This is a
   deliberate preference for the single-config convention over the
   dependency-cycle avoidance. Accept the ~125-150 lines and 16 fixture
   fixups.
2. **Design A — faithful, detection-triggered.** Consult ONLY when the
   executor emits `function_call{name:"advisor"}`, bounded by `max_uses`.
   "It should be as faithful to anthropic as possible." Design B (always
   consult) is **not** the first implementation; it may remain a later flag.
3. **The advisor MODEL (`beam`) is configured from `proxy.json`.** Not a
   hard-coded default buried in the module, not an env-only knob. The exact
   key is TBD (see §17.2) but it must be readable from the same file as every
   other deployment setting.
4. **Every phase is independently testable.** Do NOT run all six steps in one
   go and test at the end. Each commit gets its own gate: full suite green,
   targeted new tests green, and (where the step changes behaviour) a real
   end-to-end check. Stop and report between phases. **Use a cheap model
   (`beam`) for ALL tests.** Kimi models are expensive; no test may call one.

### 17.2 Concrete config shape (to add in phase 2)

```json
// proxy.json — add ONE section
"advisor": {
  "enabled": false,
  "model": "beam",
  "effort": "medium",
  "max_uses": 2,
  "max_tokens": 2048,
  "timeout_secs": 45
}
```

Wiring required (all in `src/config.rs`, following the `JsonReasoningConfig`
pattern at `:790-804`):

| edit | lines |
|---|---|
| `use crate::advisor::AdvisorConfig;` (or define `AdvisorConfig` in `config.rs` and have the advisor module take it as a plain parameter — **prefer this**: no `config.rs -> advisor -> Config` cycle, matching `model_rejection.rs`'s primitives-only API) | ~1 |
| `pub advisor: AdvisorConfig` on `Config` (`:38-86`) + doc comment | ~4 |
| `pub advisor: Option<JsonAdvisorConfig>` on `JsonConfig` (`:702-736`) with `#[serde(default, skip_serializing_if = "Option::is_none")]` — **`skip_serializing_if` is mandatory**: the TUI rewrites the whole file and `deny_unknown_fields` would make an older binary refuse to start (`:719-725`) | ~5 |
| `JsonAdvisorConfig` with `#[serde(default, deny_unknown_fields)]`, all six fields `Option<_>` | ~20 |
| `resolve` branch: env > file > `AdvisorConfig::default()`; validate `max_uses > 0`, `timeout_secs > 0`, `enabled=false` default | ~15 |
| `+1` line in `Ok(Self { … })` at `:456-469` | 1 |
| `impl Default for Config` (currently **none**) so `src/proxy.rs:1184` and test helpers can use `..Default::default()` | ~18 |
| `advisor: Default::default(),` in the **~16** full `Config { … }` literals (`src/proxy.rs:1218`; `tests/proxy_e2e.rs:152, 185, 582, 662, 749, 824, 880, 940, 1591, 1680, 1752, 1835, 1928, 2021, 2107`) | ~16 |
| `proxy.json-guide.md` + `proxy.json.example` | ~20 |
| 3-5 in-file config tests (default off, precedence, validation, serialisation roll-back safety) | ~50 |

**`enabled: false` remains the default.** Zero wire-field changes when off.
`model` is an explicit **upstream** model name (§11.4), not an inbound alias.

### 17.3 Phase gates (each is a commit, each is reported before the next)

| phase | deliverable | test gate |
|---|---|---|
| 1 | `Attempt` + `build_outbound` + `execute` refactor, **zero behaviour change**; 4 characterization tests (§13.4) | 355+2+37+4 green; clippy still 7; differential diff of received bodies + log lines enabled/disabled |
| 2 | `advisor` config section in `proxy.json` + `src/advisor/{mod,config,protocol}.rs` skeleton; feature still unreachable | full suite green; config unit tests; **prove a `proxy.json` WITH the new key still loads on the previous binary's schema** (rollback check) |
| 3 | Zero-argument `advisor` function tool synthesized from Claude Code's 3-field declaration; `# Advisor Tool` instructions preserved | e2e: upstream sees the tool, empty `{}` schema survives strict sanitization + the fake upstream's validator |
| 4 | Advisor upstream call + advice injection + `server_tool_use` / `advisor_tool_result` blocks + `usage.iterations[]` | e2e happy path (3 POSTs), 404/timeout -> `unavailable` with the executor answer intact and **no fallback POST**, `max_uses` cap |
| 5 | Buffered synthetic SSE for `stream:true` | e2e event sequence + Claude Code `--advisor` smoke test (cheap model) |
| 6 | Inbound echo (`ContentBlockParam`), TUI label, docs | multi-turn smoke test; full suite; clippy baseline |

**All e2e tests use a fake upstream except the two smoke tests, which use
`beam` and tiny `max_tokens`. No kimi, no expensive model, anywhere.**

### 17.4 What stays from §16

Refactor-first (§13/§14), one assistant message with contiguous blocks and
`end_turn` (§11.5), buffered synthetic SSE with `src/stream.rs` untouched
(§11.3), generic `Raw(Value)` wire extension (§11.6), fail closed with
`unavailable` and no `default_model` (§11.4), off by default, and the deletion
rule: if phase 4/5 shows the feature is not worth keeping, phases 2-6 revert
cleanly and phase 1 remains a debt-reducing refactor.

### 17.5 What changed from §16

| §16 said | now |
|---|---|
| module-local `advisor.json` + `OnceLock`, "no `Config` field" | **`proxy.json` section; `Config` field added** |
| "A, with B as a later config flag" | **A only, as faithful as possible** |
| "default `beam`" in module config | **`beam` set in `proxy.json`, operator-editable** |
| six commits "each independently green" | **same, but explicitly gated and reported phase by phase; cheap models for all tests** |
| "step 1 only vs all six" offered | **all six, phase by phase** |

---

## 18. Persistent advisor configuration (2026-11-15 13:25)

Operator question: *"Do I always need `--advisor`? Can it be done from
`/home/heylon/.claude/settings.json` once the proxy advisor is implemented?"*

**Yes — set `advisorModel` once. But behind a proxy you ALSO need the
experimental-advisor env var (or a fetched Anthropic feature flag), which
the earlier §9.2 caveat missed entirely.** Three agents: binary reverse
engineering, official docs, and a live fake-upstream experiment.

### 18.1 The key — PROVEN twice

```json
{ "advisorModel": "opus" }
```

- **Binary** (v2.1.322): schema at byte 283982704
  `advisorModel:()=>o().optional().describe("Advisor model for the server-side advisor tool.")`
  — top-level, sibling of `model`; **not** nested under `env`/`experimental`.
  Reader `ASr()` / `whs(e)` at 291754937 reads `bt().settings` (the **merged**
  settings object), so any scope works. `/advisor` writes
  `Kn("userSettings",{advisorModel:e},…)` at 308288697 → **user scope
  `~/.claude/settings.json`**; `off` writes `advisorModel: void 0`.
  Bundled changelog `~/.claude/cache/changelog.md:1187` (2.1.300): *"no
  advisor runs until you pick one with `/advisor`, `advisorModel` or
  `--advisor`"*.
- **Official docs**: "You can set the advisor model in three ways: `/advisor`
  … save it as your default; `advisorModel` setting … persistent default in
  your settings file; `--advisor` … single session" [19]. Settings reference:
  "Scope: Any file. Type: string, one of `fable`, `opus`, `sonnet`, or a full
  model ID. Default: unset, so the advisor is off. Per-session overrides:
  `--advisor` takes precedence" [20]. `/advisor` "saves your pick to this key
  in `~/.claude/settings.json`" [20].
- Values are aliases (`opus` → `claude-opus-5-5`) or full model ids; the
  advisor "must be at least as capable as your main model" [20]. Scope order
  (binary 283675745, docs [21]): user < shared project < project local <
  `--settings` (command-line JSON) < managed. `--advisor` outranks all of
  them for that session [22].

### 18.2 The catch behind a proxy — PROVEN by live experiment

The earlier §9.2 claim ("standby needs `api.anthropic.com`, so just set
`--advisor` explicitly") was **incomplete**. A 14-case live experiment with a
local fake Messages API, `--settings` files, scratch `HOME`, and no external
network found:

| case | `advisor_20260301` in `tools[]` | beta `advisor-tool-2026-03-01` |
|---|---|---|
| `{}` settings, no flag | none | no |
| `{"advisorModel":"opus"}`, no flag | **none** | no |
| `--advisor opus`, no flag | **none** | no |
| scratch `HOME/.claude/settings.json` = sonnet, no flag | none | no |
| `advisorModel` **+ `CLAUDE_CODE_ENABLE_EXPERIMENTAL_ADVISOR_TOOL=1`** | **yes**: `{"type":"advisor_20260301","name":"advisor","model":"claude-opus-5-5","defer_loading":true}` | yes |
| user-scope `HOME` settings + the env var, **no `--settings`** | yes | yes |
| settings `env` holds custom URL + `advisorModel` + the env var | yes | yes |
| `CLAUDE_CODE_DISABLE_ADVISOR_TOOL=1` | none (kills it) | no |
| both `advisorModel: sonnet` and `--advisor opus` | **opus** (flag wins) | yes |

Why: `ASr()` calls `MR()`, and `MR()` requires the **remote GrowthBook flag
`tengu_sage_compass2`** unless `CLAUDE_CODE_ENABLE_EXPERIMENTAL_ADVISOR_TOOL=1`
is set. The docs confirm "the advisor needs a feature flag fetched from
Anthropic" and that a gateway token with no API key has no credential to
fetch it [19][23]. In the network-isolated test the flag could not be
fetched, so **even `--advisor` did nothing** until that env var was set.
`_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL=1` did NOT help in this setup.

**⇒ The proxy implementing the advisor does not remove this client-side
gate.** It is about Claude Code's flag fetching, not about our response.

### 18.3 What to actually put in the settings file

Merge into the existing `/home/heylon/.claude/settings.json` (it already has
top-level `env`, `model`, `enabledPlugins`, `effortLevel`, `modelSettings`,
`theme` — do **not** replace `env`, it holds the custom `ANTHROPIC_BASE_URL`
and auth tokens):

```json
{
  "advisorModel": "opus",
  "env": {
    "CLAUDE_CODE_ENABLE_EXPERIMENTAL_ADVISOR_TOOL": "1"
  }
}
```

That exact shape (case Z/V) produced the declaration with **zero CLI flags
and zero process env**. `advisorModel` alone is sufficient **only** when the
remote flag is available (first-party, reachable Anthropic); with a custom
base URL, keep the env var to be deterministic. To turn it off: remove
`advisorModel`, or `/advisor off`, or `CLAUDE_CODE_DISABLE_ADVISOR_TOOL=1`
(which wins over everything) [23].

`CLAUDE_CODE_ENABLE_EXPERIMENTAL_ADVISOR_TOOL` is **NOT in the official env-vars
docs** [23] — it is an internal binary knob (283285290). `CLAUDE_CODE_DISABLE_ADVISOR_TOOL`
IS documented. Note the settings `env` block is an env-var map, not a
separate precedence level [21].

### 18.4 Effect on the implementation plan

- **Testing (§17.3 phases 5-6):** use `advisorModel: "opus"` in a **scratch
  `HOME`/`--settings`**, plus `CLAUDE_CODE_ENABLE_EXPERIMENTAL_ADVISOR_TOOL=1`,
  and a fake/cheap (`beam`) upstream. Do NOT edit
  `/home/heylon/.claude/settings.json` during tests. No `--advisor` needed
  once the settings file carries it.
- **The standby-advisor gate (§9.2) is superseded**: the real gate that
  matters behind a proxy is the **fetched feature flag**, not
  `api.anthropic.com` per se. An explicit `advisorModel` bypasses the
  standby lookup but still needs `MR()`.
- **Docs say a gateway must forward the tool entry + beta header intact**;
  if we return a 400/422 naming `Input tag 'advisor_20260301'` or the beta,
  Claude Code retries once WITHOUT the advisor and then omits it for later
  requests [24][25]. Our fail-closed `unavailable` degradation (§11.4)
  remains mandatory.

### 18.5 Experiment hygiene / caveats

- One diagnostic run accidentally used the real settings' `ANTHROPIC_BASE_URL`
  (`http://192.168.1.6:60200`) and hit the user's own local proxy (~26.6k
  input tokens) before the sandbox was applied; no external Anthropic/Ollama/
  kimi API was called. Every later run ran in `bwrap --unshare-net` with a
  scratch `HOME`. **`/home/heylon/.claude/settings.json` was never modified**
  (sha256 verified unchanged). Claude Code did write its own state files
  under `~/.claude/` (history/sessions/plugins) — normal session side effects.
- The conclusion is scoped to **no fetched flag**. In normal first-party use
  `advisorModel` alone works; the env var is the deterministic override for
  an isolated/custom-base-URL setup.
- Scratch: `/tmp/locopilot-advisor-live-SR4F13/` and `/tmp/advisor-re-20261115/`
  (may be cleaned up).

### 18.6 Documented for users (2026-11-15 14:01)

The `advisorModel` + `CLAUDE_CODE_ENABLE_EXPERIMENTAL_ADVISOR_TOOL` settings
requirement is now in **`README.md` \u00a7 "Enabling the advisor tool"**, and
has been applied to the operator's real `~/.claude/settings.json` (merging
into the existing `env` block, keeping its `ANTHROPIC_*` and
`CLAUDE_CODE_MAX_CONTEXT_TOKENS` values). The README also carries the
"advisor support is being implemented" status caveat: until
`proxy.json` has `advisor.enabled = true`, Claude Code will declare the tool
but no consultation occurs.

---

## 19. IMPLEMENTED (2026-11-15 14:xx) — as-built record

The advisor is implemented, verified with sub-agents, and committed. This
section records what was actually built, the bugs the verifiers found, and
the gates. It is the as-built counterpart to §17's plan.

### 19.1 Commits (on `dev`, parent `17f8329`)

| commit | what |
|---|---|
| `ee2673f` | `Attempt` / `build_outbound` / `execute` refactor; 4 characterization tests. Zero behavior change; deletes the existing first-attempt/fallback policy duplication |
| `622a968` | `proxy.json` `advisor` config, `Raw(Value)` wire extension points, `Usage.iterations`, `src/advisor/{mod,protocol,sse}.rs` skeleton. Unreachable |
| `033b3a2` | full integration: zero-arg tool, detection loop, advice injection, `usage.iterations`, buffered synthetic SSE, 11 e2e tests |
| `7933758` | **12 verifier-confirmed bugs fixed** (B1-B12), +15 e2e / +6 unit tests |
| `ddac5de` | inbound echo: `ContentBlockParam::{ServerToolUse, AdvisorToolResult}`, prior advice replays into the upstream transcript |
| `89f5afc` | **2 final verifier-confirmed bugs fixed** (cap re-run double render; instruction-strip idempotence/byte-identity) |

Diff `17f8329..89f5afc`: **15 files, +6666 / −349**. `src/stream.rs` control
flow untouched (only 4 `iterations: None` literals). No new crate dependency.

### 19.2 As-built architecture

- **One clean pipe** (§13/§14): `Attempt::execute` is called 2-3 times by
  `src/advisor/session.rs`; no bypass, no duplicated proxy policy.
- **Config** in `proxy.json`: `advisor.{enabled,model,effort,max_uses,
  max_tokens,timeout_secs}`, defaults `enabled:false`, `model:"beam"`,
  `max_uses:2`, `max_tokens:2048`, `timeout_secs:45`. `JsonConfig.advisor`
  has `skip_serializing_if` (TUI rollback safety). `ADVISOR_*` env overrides.
- **Activation**: `enabled` AND a tool named `advisor` with **no
  `input_schema`** (the real Claude Code declaration). A normal function tool
  named `advisor` does not activate it (B8).
- **Design A**: consult only on `function_call{name:"advisor"}`, bounded by
  `max_uses`. Advisor POST is a standalone `send_upstream_once` with explicit
  upstream `cfg.model`, `timeout` clamped to
  `min(timeout_secs, request_timeout)`, no tools, no sampling params. No
  `default_model`, no capability learning, no fallback, ever.
- **Fail closed**: any advisor failure → `unavailable` (or
  `max_uses_exceeded`) + one warning; executor re-run failures after the
  first success degrade to the last good response; `scrub_error_body`
  removes the three beta-latching tokens from any client-visible error.
- **One assistant message**: contiguous `server_tool_use` /
  `advisor_tool_result` pairs, intermediate executor text + sibling client
  tool uses preserved in order, `end_turn` / `tool_use` / preserved terminal
  `max_tokens`/`refusal`, never `pause_turn`. `usage.iterations[]` with
  `advisor_message` billed at `cfg.model`.
- **Streaming**: upstream forced non-streaming; final `Message` converted by
  `sse::message_to_events`, hard-coded `text/event-stream`, one
  `message_start`/`message_stop`, no `[DONE]`.
- **Multi-turn**: echoed `server_tool_use` + `advisor_tool_result` blocks
  parse explicitly; `advisor_result.text` is replayed into the upstream
  assistant text as `[Advisor advice from a previous turn: …]`. Redacted and
  error results inject nothing; no orphan `function_call`.
- **`enabled:false` + no declaration**: byte-identical to `17f8329` for
  non-streaming and streaming, including the two-`# Advisor Tool`-section
  edge case (verified against a built old binary).

### 19.3 Verification performed (sub-agents, all independent)

1. **Happy path / wire shapes** — independent Python harness against the real
   binary: 3 POSTs, exact request bodies, id-matched contiguous pair,
   `iterations[]`, SSE framing, `enabled:false` byte-identity. Found 5 bugs.
2. **Blind failure/security hunt** — 15 hunt items, found 11 bugs including
   HIGH: re-run `AppError` propagation + beta-latching 400 leak,
   `status:"failed"` advice accepted, cap orphan `function_call` / no answer,
   timeout extension, fallback wiping injected advice.
3. **Protocol + live Claude Code** — conformance table PASS; ran
   `/opt/claude-code/bin/claude -p` against the real proxy with a LOCAL fake
   upstream and scratch settings (`advisorModel` + the experimental env var,
   **no `--advisor` flag**): exit 0, `is_error:false`, `result:"OK"`,
   `[AdvisorTool] Advisor tool called` / `…result received` in the debug
   log, `beam` billed separately. Found the multi-turn context gap.
4. **Regression hunt on the fix commit** — all 12 fixes PASS; found 2 NEW
   bugs (cap re-run double render; non-idempotent instruction strip).
5. **Echo fix + final-two fix**, then a **final independent confirmation**:
   cap sequence `["server_tool_use","advisor_tool_result","server_tool_use",
   "advisor_tool_result","text"]` with one text and no orphan; two-section
   instruction byte-identity SHA-256 match with `17f8329`; echo sentinel
   reaches upstream; panic audit clean. **No new bugs found.**

### 19.4 Final gates (re-run by the parent agent at HEAD `89f5afc`)

| gate | result |
|---|---|
| `cargo test` | **412 lib + 2 bin + 73 e2e + 0 doc = 487 pass, 0 fail** |
| `cargo clippy --all-targets` | **7** `^warning:` lines, exactly the pre-existing set |
| `cargo build` / `cargo build --release` | green |
| `cargo fmt -- --check` | 11 pre-existing diffs, zero new |
| `git status` | clean except pre-existing `?? .claude/` |
| `/home/heylon/.claude/settings.json` | **untouched** (mtime 14:04:20) |
| models used in tests | fake upstream + `beam`; **no kimi / expensive model, no real API** |

### 19.5 Residual risks (not bugs)

1. **No live upstream was exercised.** Every test used a fake upstream or a
   local fake behind the real proxy/Claude Code. Real gateway acceptance of
   the empty strict `{}` tool schema, the `FunctionCall`/
   `FunctionCallOutput` advice-injection shape, and `reasoning.effort`
   passthrough are unverified against a live `beam` endpoint. Before enabling
   against Ollama, do one cheap smoke turn with `max_tokens` small.
2. **TTFT**: buffered 3-call chain; no early `message_start` or keepalive
   pings. A slow advisor can hit a client-side timeout (the advisor timeout
   is clamped to `request_timeout`, but the chain is up to ~3× that).
3. **Instruction-strip fidelity** is scoped to the tested heading /
   `<system-reminder>` layouts; unusual future Claude Code prompt layouts
   could select a different boundary.
4. **Public-type additions** (`Config.advisor`, `Usage.iterations`,
   `ResponseContentBlock::Raw`, `ContentBlockKind::Raw`) are source-compatible
   for this repo but would break an external exhaustive match. No
   `impl Default for Config` was added.
5. **Deletion**: reverse-order revert of `033b3a2`…`89f5afc` was verified
   clean, leaving `ee2673f` as a debt-reducing refactor.

### 19.6 To actually use it

1. `proxy.json`: set `"advisor": { "enabled": true, "model": "beam", … }`
   (the section is already in `proxy.json.example`).
2. `~/.claude/settings.json`: `"advisorModel": "opus"` plus
   `"env": { "CLAUDE_CODE_ENABLE_EXPERIMENTAL_ADVISOR_TOOL": "1" }` —
   already applied to the operator's file on 2026-11-15 (§18.6, README).
3. Restart both. No `--advisor` flag needed.

---

## 20. `0` means "no limit" for the advisor limits (2026-11-15)

### 20.1 The operator's question

> *"If I exclude `max_uses`, `max_tokens`, `timeout_secs` from `proxy.json`,
> can these values be set to infinity (no limit)?"*

**Answer (now encoded in the code):** excluding a key means **"use the
default"** (2 / 2048 / 45) — unchanged and safe. To ask for **no limit** the
operator writes **`0`** explicitly. The same rule holds for the env vars
(`ADVISOR_MAX_USES=0`, `ADVISOR_MAX_TOKENS=0`, `ADVISOR_TIMEOUT_SECS=0`).

| value | meaning |
| --- | --- |
| key **absent** | the shipped default — 2 / 2048 / 45. NOT "no limit". |
| **`0`** | **no limit** |
| `n > 0` | that bound |

`0` = unlimited follows the house precedent already in this repo:
`reasoning_replay.max_chars` documents *"`0` means unlimited"*
(`src/config.rs`, `ReasoningReplayConfig::max_chars`).

### 20.2 What changed

* `AdvisorConfig::max_uses` / `max_tokens` / `timeout_secs` are now
  `Option<u32>` / `Option<u32>` / `Option<u64>`: **`None` = no limit**.
  `AdvisorConfig::default()` is unchanged in VALUE — `Some(2)` /
  `Some(2048)` / `Some(45)` — so the shipped behaviour does not move.
* The JSON **keys** are unchanged, so `deny_unknown_fields` and the TUI
  rollback guarantee (§18.5) are unaffected. `proxy.json.example` keeps
  the safe defaults. The JSON **types** were widened in a later commit
  (§21.2): `JsonAdvisorConfig::max_uses` / `max_tokens` are `Option<u64>`
  (they were `Option<u32>`), so an out-of-range number PARSES instead of
  killing the startup — the serialized shape is identical for every
  in-range value, so the TUI round-trip is unchanged.
* `valid_or_default` was **removed**: with `0` now meaningful there is no
  longer any invalid value for these three fields. The WARN-on-overflow
  behaviour moved into the new resolver `resolve_advisor_limit` (a value
  that does not fit `u32` falls back to the default **with** a warning,
  where the old code fell back silently). That warning is only VISIBLE
  since §21.1 — before that fix it was emitted during config resolution
  and dropped on the floor.
* `advisor_timeout` now returns `Option<Duration>`: `None` = no
  per-request override (the global `request_timeout` applies), `Some(secs)`
  = `Some(min(secs, request_timeout))`. The clamp is kept, so the advisor
  can never EXTEND the operator's global bound (§11.4).
* `advisor_request` sends `max_output_tokens: effective_limit(cfg.max_tokens)`,
  so `None` **omits** the field and the upstream's own default applies.
  (§21.3 made `Some(0)` mean the same as `None` here.)

### 20.3 "No limit" means something different per field

1. **`max_uses` = no limit → the loop is unbounded.** The consultation loop
   is bounded only by the executor's behaviour (`cap = usize::MAX`). This is
   a real hazard: every iteration costs **two** upstream calls (the executor
   re-run *and* the advisor), so an executor that keeps asking will spin.
   Implemented as asked — **no hidden ceiling** — but made visible:
   * one WARN at startup (config-resolve time) when the advisor is enabled
     with an unlimited `max_uses`;
   * one WARN per turn that passes **10** consultations.
2. **`max_tokens` = no limit → the proxy imposes no cap, the upstream still
   does.** The proxy omits `max_output_tokens` from the advisor's own
   upstream request, so the **upstream's** default applies. That default may
   be SMALLER than 2048, and for reasoning models the cap must also cover
   the reasoning tokens — so this is "the proxy does not cap it", NOT "the
   answer may be long".
3. **`timeout_secs` = no limit → only the global bound remains.** No
   per-request timeout override is sent; the reqwest client is built with
   `.timeout(config.request_timeout)`, so every request — advisor included —
   is bounded by the global `request_timeout` anyway.

### 20.4 The two WARNs

* **Startup (once).** Raised when the advisor is enabled with an unlimited
  `max_uses`. Since §21.1 the resolver does NOT emit it: it COLLECTS it
  on the resolved `Config` (`Config::warnings`) and `src/main.rs` prints it
  right after `init_tracing` — on stderr (visible even with
  `log_to_disk: false`, like the `proxy_key` warning) and as a tracing
  event (so it lands in `target/logs/proxy.log`). The operator sees it
  exactly once per surface. The message text lives in one place,
  `config::UNBOUNDED_ADVISOR_LOOP_WARNING`.
* **Runtime (once per turn).** `should_warn_runaway` fires when a turn
  crosses `RUNAWAY_CONSULTATION_THRESHOLD` (10) consultations: one WARN
  per turn, at the crossing, not one per consultation.

### 20.5 Verification

495 tests (417 lib + 2 bin + 76 e2e), all green; `cargo clippy --all-targets`
7 warnings (the pre-existing set, unchanged); `cargo fmt --check` shows only
the pre-existing diffs (in files this change does not touch). No new
dependency, no real API call, and `enabled: false` stays byte-identical
(the e2e `advisor_disabled_is_byte_for_byte_identical` case still passes).

(§21 supersedes the counts above with the current ones.)

---

## 21. Three defects found by an independent verifier (2026-11-15)

An independent verifier confirmed the CORE semantics of §20 (absent → the
default, `0` → no limit, `n` → `n`, env override, `enabled: false`
byte-identical) and found three defects. All three are fixed in the
`fix(advisor): make config-time warnings visible; align 0-means-unlimited
semantics` commit.

### 21.1 Config-time warnings were silently dropped (MEDIUM)

**The bug.** `Config::load()` runs BEFORE `init_tracing()` in
`src/main.rs`, so every `tracing::warn!` emitted during config resolution
hit NO subscriber and vanished. Proven: `ADVISOR_MAX_USES=4294967296` → the
value DID fall back to the default, but the log file contained 0 "out of
range" lines and stderr showed nothing. The unbounded-loop warning had been
worked around with a duplicate `eprintln!` + re-emit block in `src/main.rs`;
the out-of-range warning had no such workaround, so it was invisible.

**The fix — ONE mechanism, not two.** The ordering cycle is real:
`init_tracing(config.log_to_disk)` needs `config.log_to_disk`, which comes
FROM the config. The cleanest resolution is to **collect** the
config-resolution warnings on the resolved `Config` (`Config::warnings`, a
`Vec<ConfigWarning>`) and emit them from `main.rs` AFTER `init_tracing`:
on stderr (always visible) and as tracing events (so they land in
`target/logs/proxy.log`). The duplicate `eprintln!` + re-emit block in
`main.rs` is GONE — there is now ONE path for every config-time warning,
so a future warning cannot be dropped either. `src/config.rs` carries the
rule in its module docs: never call `tracing::warn!` while resolving; push
a `ConfigWarning` instead.

Verified by RUNNING the built binary (no upstream call was ever made):

| case | stderr | `target/logs/proxy.log` |
| --- | --- | --- |
| `"max_uses": 4294967296` | 1 warning | 1 warning |
| `ADVISOR_MAX_USES=4294967296` | 1 warning | 1 warning |
| `"max_uses": 0`, advisor ENABLED | 1 warning | 1 warning |
| `"max_uses": 0`, advisor DISABLED | none | none |

### 21.2 An out-of-range JSON value was a HARD STARTUP FAILURE (MEDIUM)

**The bug.** `JsonAdvisorConfig::max_uses` / `max_tokens` were `Option<u32>`,
so `"max_uses": 4294967296` failed at JSON parse time
(`invalid value: integer 4294967296, expected u32`) and the proxy EXITED.
That contradicts the house rule (`src/config.rs`: *"an operator typo in one
advisor limit should not take the whole deployment down"*) and it
contradicted §20.2's claim that an out-of-range value falls back to the
default with a warning. Only the ENV path (`Option<u64>`) ever worked.

**The fix.** `JsonAdvisorConfig::max_uses` / `max_tokens` are now
`Option<u64>` — wide enough that an out-of-range number PARSES — and the
resolver keeps the `u32::try_from` range check, so the value falls back to
the default **with a warning**. `AdvisorConfig`'s own types are unchanged
(`Option<u32>` / `Option<u32>` / `Option<u64>`): those are the resolved,
in-range values. The serialized JSON shape is identical for every in-range
value, so the TUI's rewrite of `proxy.json` (§17.2, §18.5) and its
rollback guarantee are unaffected — the `skip_serializing_if` behaviour is
untouched.

### 21.3 `Some(0)` and a configured `0` meant OPPOSITE things (LOW)

**The bug.** The config resolver was the only place that interpreted `0` →
`None` (unlimited). A directly built `AdvisorConfig { max_uses: Some(0) }`
meant "zero consultations" (`advisor_cap` → `Some(0)`), and
`timeout_secs: Some(0)` meant `Duration::ZERO` — an instant timeout. One
number, two meanings.

**The fix — option (a).** The RESOLVED `AdvisorConfig` now means exactly
what a CONFIGURED value means: `Some(0)` is "no limit", everywhere. The
house rule lives in ONE place, `config::effective_limit` (`Option<T>` →
`None` when the value is `0`), and it is applied at BOTH ends:

* the resolver (a configured `0` → `None`), and
* the advisor session — `advisor_cap` (`Some(0)` → `usize::MAX`),
  `advisor_timeout` (`Some(0)` → `None`) and the advisor's own request
  (`max_tokens: Some(0)` → the field is omitted).

`AdvisorConfig`'s field types are unchanged (`Option<u32>` / `Option<u32>` /
`Option<u64>`); only the MEANING of `Some(0)` is unified.

The e2e case `cap_terminal_rerun_emits_exactly_one_cap_pair_and_text` used
`max_uses: Some(0)` to reach the cap path. It now uses
`max_uses: Some(1)` and has the executor ask TWICE — the cap is forced by
the executor's behaviour, not by a zero bound — and still asserts exactly
one cap pair and exactly one text block.

### 21.4 Verification

503 tests (423 lib + 2 bin + 78 e2e), all green; `cargo clippy --all-targets`
7 warnings (the pre-existing set, unchanged); `cargo fmt --check` shows only
the pre-existing diffs (in files this change does not touch). No new
dependency, no real API call, and `enabled: false` stays byte-identical
(the e2e `advisor_disabled_is_byte_for_byte_identical` case still passes).

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

**§18 additions (2026-11-15)**
- [19] Escalate hard decisions with the advisor tool — https://code.claude.com/docs/en/advisor
- [20] All settings (`advisorModel`) — https://code.claude.com/docs/en/settings-reference
- [21] Settings files and precedence — https://code.claude.com/docs/en/settings
- [22] CLI reference (`--advisor`) — https://code.claude.com/docs/en/cli-reference
- [23] Environment variables — https://code.claude.com/docs/en/env-vars
- [24] Feature availability — https://code.claude.com/docs/en/feature-availability
- [25] LLM gateway protocol (automatic retry / error forwarding) — https://code.claude.com/docs/en/llm-gateway-protocol
- [26] Connect Claude Code to an LLM gateway — https://code.claude.com/docs/en/llm-gateway-connect
- [27] Claude Code commands (`/advisor`) — https://code.claude.com/docs/en/commands
- Claude Code binary v2.1.322 byte offsets 283982704 / 291754937 / 296153443 / 302870248 / 308288697; bundled `~/.claude/cache/changelog.md:1187`
- Live fake-upstream experiment (14 cases), scratch `/tmp/locopilot-advisor-live-SR4F13/`
