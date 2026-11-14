# ADVISOR TOOL RESEARCH

> Scratchpad. Working notes for: *"what would it take to implement Anthropic's
> `advisor_tool` server tool in this proxy?"*
>
> Started 2026-11-14. Repo: `/home/heylon/git/openai-to-anthropic-proxy`.
> Commits at the time of writing: `4b101ac` (BUG 2 scratchpad).
>
> Researched by three sub-agents: **protocol** (docs + binary + corpus),
> **architecture** (the proxy's code), **wire evidence** (binary offsets +
> corpus). Their disagreements are resolved in §9.

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

### 4.2 A live inconsistency the corpus does prove

In real Aug-2026 traffic, the upstream request body carried the `# Advisor
Tool` instructions in `instructions` (15,121 chars) while the `tools` array
(16 tools) contained **no advisor**. The proxy's `append_system_message`
(`src/translate.rs:1849`) folds Anthropic `role:"system"` messages into
`instructions`, so the executor was told *"call advisor BEFORE substantive
work"* while the tool did not exist.

**⇒ A proxy that does not implement the advisor should strip the
`# Advisor Tool` block from the instructions it forwards.** Claude Code's own
removal string is `"The advisor tool is no longer available; disregard the
earlier advisor instructions."` (binary offset 291,758,3xx).

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

1. **Do this first (option D, ~15 lines):** strip the `# Advisor Tool` block
   from the instructions the proxy forwards, or append Claude Code's own
   "the advisor tool is no longer available" string. Today the executor is
   told to call a tool that does not exist.
2. **Then decide between option A and option B.** Option B (~350 lines, no
   streaming splice) answers "do I want a second opinion on every task?" for
   a fraction of the cost and risk; option A answers "do I want Anthropic's
   advisor?" and pays ~4× the tokens to get it.
3. **Either way:** gate it behind config (default off), give the advisor its
   own model / effort / `max_uses` / timeout keys rather than borrowing the
   executor's, **use a cheap model** (§7), and never let the advisor call fall
   back to `default_model`.

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

**Local evidence**
- Claude Code binary `/opt/claude-code/bin/claude` (2.1.322) — byte offsets cited inline
- Proxy corpus `/tmp/proxylogs/` — 27 files, 539 MB, 2026-08-15 → 2026-11-12
- Ollama `POST https://ollama.com/api/show` (public, no auth) — per-model `thinking` vocabulary
