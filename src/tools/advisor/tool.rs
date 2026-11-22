//! The advisor as a [`ServerTool`]: serve the executor's consultation calls.
//!
//! The loop around it (running the executor, finding calls, injecting
//! outputs, assembling the response) is the shared
//! [`server_loop`](crate::tools::server_loop); this module is only what is
//! specific to consulting the advisor model (§§11.3, 11.4, 11.5, 15, 17.1 of
//! `ADVISOR_TOOL_RESEARCH.md`).
//!
//! ## The executor and the advisor are deliberately asymmetric
//!
//! Executor responses go through `Attempt::execute` (and may legitimately
//! use `default_model` fallback). Advisor responses go through a plain
//! `send_upstream_once` with an explicit per-request timeout: **no**
//! `detect_model_rejection`, **no** `mappings_snapshot.default_model()`, no
//! capability learning, no corrective retry. §11.4 spells out why applying
//! the executor's fallback to the advisor would turn a second opinion into
//! a self-review and corrupt stats attribution.
//!
//! ## Fail closed
//!
//! A consultation NEVER fails the client request. Every failure degrades to
//! `protocol::unavailable` (after ONE retry, for failures that could be
//! transient: a transport error, timeout, 429, 5xx or an unusable response; a
//! 4xx is not retried), at most one TUI warning is emitted for the whole
//! turn, and the executor's transcript is still given a
//! `function_call_output` so the upstream never sees an orphan
//! `function_call`. The loop is bounded by `AdvisorConfig::max_uses`, unless
//! `max_uses` is `0` ("no limit"), in which case it is bounded only by the
//! executor's behaviour and the proxy merely WARNs (once at startup, and once
//! per turn past [`RUNAWAY_CONSULTATION_THRESHOLD`]).
//!
//! At the cap the tool answers with `max_uses_exceeded` and becomes
//! [`exhausted`](ServerTool::exhausted): the executor gets ONE more run for a
//! real answer, and any further advisor call in it is dropped, so a turn can
//! never be kept alive by an executor that ignores the refusal.
//!
//! After the FIRST successful executor response, a failure on a re-run is
//! ALSO fail-closed: the loop renders the last good executor response and
//! [`on_rerun_failure`](ServerTool::on_rerun_failure) appends an
//! `unavailable` pair. No `AppError` from a re-run can reach the client: a
//! 400 naming the advisor beta/tool would latch the feature off in Claude
//! Code for the whole process (§18.4), so [`scrub_error`](ServerTool::scrub_error)
//! also cleans the first one.

// `tools` is a private module, so these crate-visible items look redundant to
// clippy; they must be `pub(crate)` because they name `proxy::Attempt`.
#![allow(clippy::redundant_pub_crate)]

use std::time::Duration;

use futures_util::future::BoxFuture;
use serde_json::Value;

use crate::anthropic::{ResponseContentBlock, Usage};
use crate::config::{AdvisorConfig, effective_limit};
use crate::error::AppError;
use crate::proxy::{AppState, Attempt, parse_upstream_body, send_upstream_once, token_usage};
use crate::responses::{
    Input, InputContentPart, InputItem, OutputContentPart, OutputItem, ReasoningConfig,
    ResponsesRequest, ResponsesResponse, ResponsesUsage,
};
use crate::tools::advisor::{ADVISOR_TOOL_NAME, protocol, scrub_error_body};
use crate::tools::server_loop::handler::{Call, Served, ServerTool};
use crate::tools::server_loop::summary::CallSummary;
use crate::tui::format::{OUTCOME_ERROR, OUTCOME_OK, OUTCOME_REFUSED, format_token_count};
use crate::tui::{LogKind, LogLine};

/// Text injected when the advisor failed so often that the proxy stopped
/// consulting it for this turn. Unlike [`FailureKind::executor_text`] it says the advisor will
/// not come back, because the executor's next call to it would be dropped.
const NO_MORE_ADVICE_BROKEN: &str = "The advisor is unavailable and will not be consulted again \
for this request. Do not call it again; answer with the information you already have.";

/// Text injected when the executor tried to consult more times than the
/// operator allowed. Also deliberately does NOT look like advice.
const NO_MORE_ADVICE: &str =
    "Advisor consultation limit reached. Continue the task without another second opinion.";

/// A single turn that passes this many consultations is a runaway loop.
/// Every iteration costs TWO upstream calls (the executor re-run and the
/// advisor), so an executor that keeps asking would spin. The proxy does
/// NOT cap an unlimited `max_uses` (the operator asked for no limit) — it
/// only makes the loop visible in the log.
const RUNAWAY_CONSULTATION_THRESHOLD: usize = 10;

/// Failed consultations (each already retried once) after which the advisor
/// is treated as down for the rest of the turn. Failed consultations do not
/// count toward `max_uses`, so without this an executor could keep calling a
/// broken advisor, each call costing the operator two upstream attempts.
const MAX_FAILED_CONSULTATIONS: usize = 2;

/// Clamp the advisor's own per-request timeout to the operator's global
/// `request_timeout`. reqwest's per-request `.timeout()` OVERRIDES the
/// client-level one, so a large `advisor.timeout_secs` must not be allowed
/// to EXTEND the operator's bound (§11.4).
///
/// `None` = **no per-request override** ("no limit"): the global
/// `request_timeout` — the timeout the reqwest client itself was built
/// with — still applies to every request, advisor or not. `Some(secs)` =
/// that bound, clamped to the global one.
///
/// The "0 means no limit" rule is applied where the operator's input
/// enters — in the config resolver, which turns a configured `0` into
/// `None` — AND here, so a hand-built `Some(0)` means exactly the same
/// thing ("no limit") instead of "zero seconds".
#[must_use]
fn advisor_timeout(cfg: &AdvisorConfig, request_timeout: Duration) -> Option<Duration> {
    effective_limit(cfg.timeout_secs).map(|secs| Duration::from_secs(secs).min(request_timeout))
}

/// The consultation cap for one turn.
///
/// `None` (and `Some(0)`, which means the same thing) means the loop is
/// bounded ONLY by the executor's behaviour: the executor may call
/// `advisor` any number of times. Every iteration costs TWO upstream calls
/// (the executor re-run and the advisor), so an executor that keeps asking
/// will spin — the proxy
/// warns about it (once at startup, and again at runtime past
/// [`RUNAWAY_CONSULTATION_THRESHOLD`]) but imposes no ceiling of its own.
#[must_use]
fn advisor_cap(cfg: &AdvisorConfig) -> usize {
    effective_limit(cfg.max_uses).map_or(usize::MAX, |m| usize::try_from(m).unwrap_or(usize::MAX))
}

/// The `max_output_tokens` to send: the answer budget plus the reasoning
/// budget.
///
/// Upstream counts reasoning and answer against ONE limit and offers no way
/// to exclude reasoning, so a reasoning advisor given only an answer-sized
/// limit can spend all of it thinking and return nothing. The proxy therefore
/// sends `max_tokens + reasoning_max_tokens`; the answer itself is held to
/// `max_tokens` afterwards ([`cap_answer`]).
///
/// Either budget at "no limit" (`None` or `Some(0)`) means the SUM has no
/// limit: the field is OMITTED so the upstream's own default applies.
fn output_token_limit(cfg: &AdvisorConfig) -> Option<u32> {
    let answer = effective_limit(cfg.max_tokens)?;
    let reasoning = effective_limit(cfg.reasoning_max_tokens)?;
    Some(answer.saturating_add(reasoning))
}

/// Characters per token assumed when holding the answer to `max_tokens`.
/// Deliberately generous (English averages about 4), so a cut only ever
/// removes an answer that was well over budget.
const CHARS_PER_TOKEN: usize = 4;

/// Hold the advisor's answer to `max_tokens`, which the upstream cannot do
/// for us because it counts reasoning in the same limit. The cut is an
/// estimate ([`CHARS_PER_TOKEN`]) made on a character boundary.
fn cap_answer(text: String, cfg: &AdvisorConfig) -> String {
    let Some(limit) = effective_limit(cfg.max_tokens) else {
        return text;
    };
    let max_chars = usize::try_from(limit)
        .unwrap_or(usize::MAX)
        .saturating_mul(CHARS_PER_TOKEN);
    match text.char_indices().nth(max_chars) {
        Some((end, _)) => format!("{} […]", text[..end].trim_end()),
        None => text,
    }
}

/// The advisor's own system prompt.
///
/// The user message it reads carries ANOTHER assistant's instructions and
/// conversation, including that assistant's tool calls. A model shown those
/// tends to take them as its own and carry on as that assistant: in the proxy
/// log `beam` answered with a call to `advisor` ("Let me call the advisor
/// before proceeding…") and no text, so the consultation failed. This prompt
/// therefore says, in so many words, who the reader is NOT, what the
/// transcript is, that it has no tools, and what its only output is.
const ADVISOR_INSTRUCTIONS: &str = "You are an independent reviewer advising another AI \
assistant. You are NOT that assistant, and you must not continue its work or do its task. \
The user message contains that assistant's instructions and its conversation so far; it has \
paused to ask you for advice. Write ONE concise paragraph of actionable advice for it: what \
to do next, what to check, what to avoid. You have no tools: never call a function or tool \
(including one named `advisor`) and never write tool-call syntax. Do not address the user and \
do not repeat the question. Reply with the advice as plain text only.";

/// Build the separate, tool-free advisor request. `model` is the explicit
/// UPSTREAM name from `proxy.json` — alias resolution is intentionally
/// bypassed (§11.4).
fn advisor_request(cfg: &AdvisorConfig, transcript: &str) -> ResponsesRequest {
    ResponsesRequest {
        model: cfg.model.clone(),
        input: Input::Items(vec![InputItem::Message {
            role: "user".to_owned(),
            content: vec![InputContentPart::InputText {
                text: transcript.to_owned(),
                prompt_cache_breakpoint: None,
            }],
        }]),
        instructions: Some(ADVISOR_INSTRUCTIONS.to_owned()),
        tools: None,
        tool_choice: None,
        reasoning: cfg.effort.as_ref().map(|effort| ReasoningConfig {
            effort: effort.clone(),
            ..ReasoningConfig::default()
        }),
        text: None,
        temperature: None,
        top_p: None,
        max_output_tokens: output_token_limit(cfg),
        parallel_tool_calls: None,
        store: None,
        previous_response_id: None,
        user: None,
        stream: false,
        metadata: None,
        prompt_cache_key: None,
        prompt_cache_options: None,
    }
}

/// The conversation the advisor sees: the executor's system instructions
/// plus its whole (Responses-shaped) transcript. The shared loop appends the
/// executor's current round (its text, its reasoning per the replay mode,
/// and every call it made, the triggering advisor call included) before any
/// call is served, so the advisor sees what led to the consultation, and the
/// results of any call the round served before it.
///
/// Encrypted reasoning is removed: it is opaque, often large, and only the
/// model that produced it can read it, so it is pure cost to the advisor.
fn transcript(attempt: &Attempt) -> String {
    let instructions = reviewer_view_of(attempt.outbound.instructions.as_deref());
    let mut input = serde_json::to_value(&attempt.outbound.input).unwrap_or(Value::Null);
    strip_encrypted_content(&mut input);
    strip_advisor_section_from_system_messages(&mut input);
    build_transcript(&instructions, input, MAX_TRANSCRIPT_CHARS)
}

/// The `# Advisor Tool` section can also sit in a system message kept in the
/// conversation (`system_message_placement = "inline"`); the reviewer must not
/// read it there either (see [`reviewer_view_of`]).
fn strip_advisor_section_from_system_messages(input: &mut Value) {
    let Value::Array(items) = input else { return };
    for item in items.iter_mut().filter(|i| i["role"] == "system") {
        if let Some(parts) = item["content"].as_array_mut() {
            for part in parts {
                if let Some(text) = part["text"].as_str() {
                    let mut text = text.to_owned();
                    crate::translate::strip_advisor_tool_instructions(&mut text);
                    part["text"] = Value::String(text);
                }
            }
        }
    }
}

/// The executor's instructions as the reviewer should see them: without the
/// `# Advisor Tool` section. That section tells its reader to call the
/// advisor before substantive work, and a reviewer that reads it does
/// exactly that instead of reviewing.
fn reviewer_view_of(instructions: Option<&str>) -> String {
    let mut instructions = instructions.unwrap_or_default().to_owned();
    crate::translate::strip_advisor_tool_instructions(&mut instructions);
    instructions
}

/// The most characters the advisor prompt may hold. The advisor is a
/// separate model with its own, often smaller, context window, and the
/// executor's transcript grows without bound over a long session: an
/// untrimmed 1.4M-token prompt was rejected by a 1,048,576-token model with a
/// 400, and every later consultation in that session failed the same way.
/// JSON-heavy text runs at roughly 3 characters per token, so this budget is
/// roughly 400k tokens — well inside a 1M window, with room for the advisor's
/// reasoning and answer.
const MAX_TRANSCRIPT_CHARS: usize = 1_200_000;

/// Successive per-string caps applied to tool RESULTS (the bulk of any long
/// transcript) while the prompt is over budget, gentlest first.
const RESULT_CAPS: [usize; 3] = [16_000, 4_000, 1_000];

/// Items dropped from the middle of an over-long transcript go in blocks of
/// this many, so the start of the prompt stays identical across consecutive
/// consultations and the upstream's prompt cache can keep hitting it.
const DROP_STEP: usize = 16;

/// Cap applied to every string anywhere in the transcript if tool results
/// alone were not enough (e.g. a huge pasted user message).
const ANY_STRING_CAP: usize = 20_000;

/// Assemble the advisor prompt, trimmed to `budget` characters.
///
/// What is kept, in order of how much it is worth:
/// 1. the first item (the user's original task) and the most recent items;
/// 2. the executor's instructions (capped at a quarter of the budget);
/// 3. the middle of the conversation, oldest items going first.
///
/// Trimming proceeds in stages and stops at the first that fits: inline
/// `data:` blobs (images) are always replaced by a placeholder; then tool
/// results are shortened (gentlest cap first); then every long string; then
/// the oldest items are dropped and replaced by one marker item.
fn build_transcript(instructions: &str, mut input: Value, budget: usize) -> String {
    omit_inline_data(&mut input);
    let instructions = truncate_str(instructions, budget / 4);
    let assemble = |input: &Value| {
        format!(
            "The assistant's own instructions follow. They were written for the assistant, \
             not for you.\n\n\
             <assistant_instructions>\n{instructions}\n</assistant_instructions>\n\n\
             <transcript>\n{}\n</transcript>\n\n\
             The assistant's last action in the transcript is the call that asks you for \
             advice. Do not continue its task and do not call any tool. Write your advice \
             for it now, as plain text.",
            serde_json::to_string(input).unwrap_or_else(|_| "[]".to_owned())
        )
    };

    let mut prompt = assemble(&input);
    let original = prompt.len();
    if original <= budget {
        return prompt;
    }

    for cap in RESULT_CAPS {
        if let Value::Array(items) = &mut input {
            for item in items
                .iter_mut()
                .filter(|item| item["type"] == "function_call_output")
            {
                cap_strings(item, cap);
            }
        }
        prompt = assemble(&input);
        if prompt.len() <= budget {
            return trimmed(prompt, original);
        }
    }

    cap_strings(&mut input, ANY_STRING_CAP);
    prompt = assemble(&input);
    if prompt.len() <= budget {
        return trimmed(prompt, original);
    }

    // Still too big: drop the oldest items after the first until it fits.
    //
    // The number dropped is rounded UP to a multiple of DROP_STEP. A
    // consultation happens every few items, so dropping exactly as many as
    // needed would move the cut (and the "N earlier items were omitted"
    // marker) on every consultation, changing the start of the prompt each
    // time and defeating the upstream's prompt cache. Rounded up, the cut
    // stays put for several consultations in a row.
    if let Value::Array(items) = &mut input {
        let mut size = prompt.len();
        let mut needed = 0;
        let mut removable = items.len().saturating_sub(2);
        while removable > needed && size > budget {
            size = size.saturating_sub(items[1 + needed].to_string().len() + 1);
            needed += 1;
        }
        let dropped = needed.div_ceil(DROP_STEP).saturating_mul(DROP_STEP);
        removable = removable.min(dropped);
        if needed > 0 {
            items.drain(1..=removable);
            items.insert(
                1,
                serde_json::json!({
                    "type": "omitted",
                    "note": format!(
                        "{removable} earlier items were omitted to fit the reviewer's context window"
                    ),
                }),
            );
            prompt = assemble(&input);
        }
    }
    trimmed(prompt, original)
}

/// Log that a transcript was shortened.
fn trimmed(prompt: String, original: usize) -> String {
    tracing::info!(
        original_chars = original,
        trimmed_chars = prompt.len(),
        "advisor transcript trimmed to fit the advisor's context window"
    );
    prompt
}

/// Replace every inline `data:` URI (an embedded image) with a placeholder:
/// a text-only reviewer cannot use it, and it can be megabytes.
fn omit_inline_data(value: &mut Value) {
    match value {
        Value::String(s) if s.starts_with("data:") && s.contains(";base64,") => {
            *s = format!("[inline data omitted: {} bytes]", s.len());
        }
        Value::Object(map) => map.values_mut().for_each(omit_inline_data),
        Value::Array(items) => items.iter_mut().for_each(omit_inline_data),
        _ => {}
    }
}

/// Shorten every string under `value` that is longer than `cap` bytes.
fn cap_strings(value: &mut Value, cap: usize) {
    match value {
        Value::String(s) if s.len() > cap => *s = truncate_str(s, cap),
        Value::Object(map) => map.values_mut().for_each(|v| cap_strings(v, cap)),
        Value::Array(items) => items.iter_mut().for_each(|v| cap_strings(v, cap)),
        _ => {}
    }
}

/// `s` cut to at most `cap` bytes on a character boundary, with a note of how
/// much was removed. Unchanged when it already fits.
fn truncate_str(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_owned();
    }
    let mut end = cap;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… [{} bytes omitted]", &s[..end], s.len() - end)
}

/// Remove every `encrypted_content` field from a transcript.
fn strip_encrypted_content(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.remove("encrypted_content");
            map.values_mut().for_each(strip_encrypted_content);
        }
        Value::Array(items) => items.iter_mut().for_each(strip_encrypted_content),
        _ => {}
    }
}

/// First non-empty `output_text` in the first `message` output item.
fn extract_advice(resp: &ResponsesResponse) -> Option<String> {
    resp.output.iter().find_map(|item| match item {
        OutputItem::Message { content, .. } => content.iter().find_map(|part| match part {
            OutputContentPart::OutputText { text, .. } if !text.trim().is_empty() => {
                Some(text.trim().to_owned())
            }
            _ => None,
        }),
        _ => None,
    })
}

/// Pure status + text guard for one advisor response. A 200 whose body says
/// `status:"failed"` / `"incomplete"` is NOT a successful review: it must
/// not be injected into the executor's prompt or billed as advice.
///
/// The one exception is a response that stopped at the output limit
/// (`incomplete` / `max_output_tokens`) but already holds advice text. That
/// text is real advice that was cut short, so it is kept, marked as cut, and
/// the executor is better served by it than by nothing. An `incomplete`
/// response with no text, or one stopped for any other reason (a content
/// filter), is still a failure.
fn advice_from_response(
    resp: &ResponsesResponse,
) -> Result<(String, Option<ResponsesUsage>), String> {
    let hit_output_limit = resp.status == "incomplete"
        && resp
            .incomplete_details
            .as_ref()
            .is_some_and(|d| d.reason == "max_output_tokens");
    if resp.status != "completed" && !hit_output_limit {
        return Err(format!("advisor response status is {}", resp.status));
    }
    let text = match extract_advice(resp) {
        Some(text) if hit_output_limit => format!("{text} [advice cut off at the output limit]"),
        Some(text) => text,
        None if hit_output_limit => {
            return Err(
                "advisor hit its output limit before writing any advice (its \
                        reasoning used the whole budget)"
                    .to_owned(),
            );
        }
        None if resp
            .output
            .iter()
            .any(|item| matches!(item, OutputItem::FunctionCall { .. })) =>
        {
            return Err("advisor called a tool instead of writing advice".to_owned());
        }
        None => return Err("advisor returned no text".to_owned()),
    };
    Ok((text, resp.usage.clone()))
}

/// Why an advisor consultation failed. It decides three things: the error
/// code Claude Code shows ("Advisor unavailable (overloaded)"), what the
/// executor is told, and whether a retry is worth making.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailureKind {
    /// The conversation does not fit the advisor model's context window.
    /// Retrying would fail identically.
    PromptTooLong,
    /// HTTP 429.
    TooManyRequests,
    /// HTTP 5xx other than 504, or the connection failed.
    Overloaded,
    /// The advisor did not answer in time (HTTP 504 / a request timeout).
    Timeout,
    /// The advisor answered but gave no usable advice: no text, a tool call
    /// instead of text, or stopped at its output limit before writing any.
    NoAdvice,
    /// Any other 4xx: the upstream refused the request itself (a bad model
    /// name, a rejected parameter). Retrying would fail identically.
    Rejected,
}

impl FailureKind {
    /// The `error_code` for the client's `advisor_tool_result_error` block.
    /// Only codes Claude Code knows are used, so it never renders "unknown".
    /// There is no client code for the kinds that fall back to the generic
    /// `unavailable`.
    const fn client_code(self) -> &'static str {
        match self {
            Self::PromptTooLong => protocol::ERROR_CODE_PROMPT_TOO_LONG,
            Self::TooManyRequests => protocol::ERROR_CODE_TOO_MANY_REQUESTS,
            Self::Overloaded => protocol::ERROR_CODE_OVERLOADED,
            Self::Timeout => protocol::ERROR_CODE_EXECUTION_TIME_EXCEEDED,
            Self::NoAdvice | Self::Rejected => protocol::ERROR_CODE_UNAVAILABLE,
        }
    }

    /// What the executor is told in place of advice. It must not read like
    /// advice, and each one ends by telling the executor to carry on.
    const fn executor_text(self) -> &'static str {
        match self {
            Self::PromptTooLong => {
                "The advisor could not be consulted: the conversation is too long for it. \
                 Continue the task without a second opinion."
            }
            Self::TooManyRequests => {
                "The advisor could not be consulted: it is rate limited right now. \
                 Continue the task without a second opinion."
            }
            Self::Overloaded => {
                "The advisor could not be consulted: it is overloaded or unreachable. \
                 Continue the task without a second opinion."
            }
            Self::Timeout => {
                "The advisor could not be consulted: it did not answer in time. \
                 Continue the task without a second opinion."
            }
            Self::NoAdvice => {
                "The advisor could not be consulted: it returned no advice. \
                 Continue the task without a second opinion."
            }
            Self::Rejected => {
                "The advisor could not be consulted: the request to it was refused. \
                 Continue the task without a second opinion."
            }
        }
    }

    /// A second attempt could plausibly succeed. A reasoning model is not
    /// deterministic, so "no advice" is worth one more try; a too-long prompt
    /// or a refused request would fail again identically.
    const fn retryable(self) -> bool {
        !matches!(self, Self::PromptTooLong | Self::Rejected)
    }

    /// Sort an upstream HTTP failure.
    fn from_http(status: u16, body: &str) -> Self {
        let shared = crate::context_overflow::detect(status, body).is_some();
        let body = body.to_ascii_lowercase();
        let too_long = shared
            || [
                "too long",
                "context length",
                "context window",
                "context_length",
            ]
            .iter()
            .any(|needle| body.contains(needle));
        match status {
            429 => Self::TooManyRequests,
            504 => Self::Timeout,
            _ if too_long => Self::PromptTooLong,
            500.. => Self::Overloaded,
            _ => Self::Rejected,
        }
    }

    /// Sort an error from the shared upstream client. A timeout arrives as
    /// HTTP 504 and a connection failure as 502 (see `From<reqwest::Error>`).
    fn from_app_error(err: &AppError) -> Self {
        match err {
            AppError::Upstream { status, body } => Self::from_http(status.as_u16(), body),
            _ => Self::Overloaded,
        }
    }
}

/// Why one advisor POST failed.
#[derive(Debug)]
struct ConsultError {
    /// For the operator's log: specific, and may quote the upstream.
    message: String,
    kind: FailureKind,
}

impl ConsultError {
    fn new(kind: FailureKind, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind,
        }
    }

    fn from_app_error(err: &AppError) -> Self {
        Self::new(FailureKind::from_app_error(err), err.to_string())
    }
}

/// Pause before the single retry, so a briefly overloaded upstream is not
/// hit again at once.
const RETRY_DELAY: Duration = Duration::from_millis(250);

/// One advisor consultation: one POST, plus ONE retry if the first failed in
/// a way that could be transient ([`FailureKind::retryable`]). A failure is a
/// [`ConsultError`], never an `AppError`: the caller degrades instead of
/// failing the client. When the retry fails too, the error keeps the RETRY's
/// kind and its message names both attempts.
///
/// A failed attempt costs the operator nothing in `max_uses` (see
/// [`AdvisorTool::answer`]); the retry exists so one flaky response does not
/// cost the executor its advice.
async fn consult(
    attempt: &Attempt,
    cfg: &AdvisorConfig,
    prompt: &str,
) -> Result<(String, Option<ResponsesUsage>), ConsultError> {
    let first = match consult_once(attempt, cfg, prompt).await {
        Ok(advice) => return Ok(advice),
        Err(first) if !first.kind.retryable() => return Err(first),
        Err(first) => first,
    };
    tracing::warn!(
        error = %first.message,
        failure = first.kind.client_code(),
        advisor_model = %cfg.model,
        prompt_chars = prompt.len(),
        "advisor consultation attempt failed; retrying once"
    );
    tokio::time::sleep(RETRY_DELAY).await;
    consult_once(attempt, cfg, prompt).await.map_err(|second| {
        ConsultError::new(
            second.kind,
            format!(
                "{} (after a retry; first attempt: {})",
                second.message, first.message
            ),
        )
    })
}

/// The `prompt_cache_key` for an advisor request.
///
/// Sent only when the operator has turned prompt caching on for the advisor's
/// model (`prompt_caching.models`), exactly as for the executor: an upstream
/// that does not know the field must not receive it unasked. A configured
/// `prompt_caching.cache_key` is used as given. Otherwise the key is derived
/// from `session` (see [`session_fingerprint`]), which stays the same for
/// every consultation of one conversation, so they are routed alike and share
/// a cache, while a different conversation gets a different key.
fn advisor_cache_key(
    config: &crate::config::Config,
    cfg: &AdvisorConfig,
    session: &str,
) -> Option<String> {
    let caching = config.prompt_caching_for_model(&cfg.model);
    if caching.models.is_empty() {
        return None;
    }
    if caching.cache_key.is_some() {
        return caching.cache_key;
    }
    // FNV-1a, 64-bit: stable across runs and Rust versions, unlike `DefaultHasher`.
    let hash = session
        .bytes()
        .chain(cfg.model.bytes())
        .fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        });
    Some(format!("advisor-{hash:016x}"))
}

/// What identifies one conversation across its consultations: its FIRST
/// input item (the user's original task), which never changes as the
/// conversation grows, cut to a bounded length.
fn session_fingerprint(input: &Input) -> String {
    let Input::Items(items) = input else {
        return String::new();
    };
    let first = items
        .first()
        .and_then(|item| serde_json::to_string(item).ok())
        .unwrap_or_default();
    truncate_str(&first, SESSION_FINGERPRINT_BYTES)
}

/// How much of the first input item identifies a conversation.
const SESSION_FINGERPRINT_BYTES: usize = 2_000;

/// One standalone advisor POST.
async fn consult_once(
    attempt: &Attempt,
    cfg: &AdvisorConfig,
    prompt: &str,
) -> Result<(String, Option<ResponsesUsage>), ConsultError> {
    let mut outbound = advisor_request(cfg, prompt);
    outbound.prompt_cache_key = advisor_cache_key(
        &attempt.state.config,
        cfg,
        &session_fingerprint(&attempt.outbound.input),
    );
    let url = attempt.url();
    // `None` = no per-request timeout override ("no limit"): the global
    // `request_timeout` — the timeout the reqwest client was built with —
    // still applies.
    let timeout = advisor_timeout(cfg, attempt.state.config.request_timeout);
    let resp = send_upstream_once(&attempt.state, &url, &attempt.headers, &outbound, timeout)
        .await
        .map_err(|e| ConsultError::from_app_error(&e))?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        let body: String = body.chars().take(300).collect();
        let kind = FailureKind::from_http(status.as_u16(), &body);
        return Err(ConsultError::new(
            kind,
            format!("upstream returned {status}: {}", scrub_error_body(&body)),
        ));
    }

    let parsed = parse_upstream_body(resp)
        .await
        .map_err(|e| ConsultError::from_app_error(&e))?;
    let (text, usage) = advice_from_response(&parsed)
        .map_err(|message| ConsultError::new(FailureKind::NoAdvice, message))?;
    Ok((cap_answer(text, cfg), usage))
}

/// The gate behind `warn_once`: exactly ONE operator-visible TUI line per
/// turn. (The tracing log is not gated.)
fn should_warn(warned: &mut bool) -> bool {
    if *warned {
        false
    } else {
        *warned = true;
        true
    }
}

/// At most ONE operator-visible line per turn: the first failure is the
/// useful one in the TUI, repeats are noise there. The tracing log is NOT
/// gated (see [`AdvisorTool::answer`]): it is the record of how often the
/// advisor fails.
fn warn_once(state: &AppState, warned: &mut bool, cfg: &AdvisorConfig) {
    if !should_warn(warned) {
        return;
    }
    state.output.emit(LogLine {
        kind: LogKind::Warning,
        text: format!(
            "  \u{26a0} advisor ({}) unavailable — continuing without advice",
            cfg.model
        ),
    });
}

/// The gate behind the runaway warning: exactly ONE warning per turn, at
/// the moment the turn crosses the threshold. A turn with at most
/// [`RUNAWAY_CONSULTATION_THRESHOLD`] consultations is normal and never
/// warns.
fn should_warn_runaway(consultations: usize, warned: &mut bool) -> bool {
    if *warned || consultations <= RUNAWAY_CONSULTATION_THRESHOLD {
        return false;
    }
    *warned = true;
    true
}

/// Append a fresh `server_tool_use` / `unavailable` pair (or close a
/// dangling `server_tool_use` with its result). Used when a re-run of the
/// executor fails AFTER a consultation: the client must still see a
/// contiguous, id-matched pair and a known error code.
fn append_degraded_pair(prefix: &mut Vec<ResponseContentBlock>) {
    let id = match prefix.last() {
        Some(ResponseContentBlock::Raw(v))
            if v["type"] == "server_tool_use" && v["id"].is_string() =>
        {
            v["id"].as_str().unwrap_or("").to_owned()
        }
        _ => protocol::new_id(),
    };
    if !prefix
        .last()
        .is_some_and(|b| matches!(b, ResponseContentBlock::Raw(v) if v["type"] == "server_tool_use" && v["id"] == id))
    {
        prefix.push(ResponseContentBlock::Raw(protocol::server_tool_use(&id)));
    }
    prefix.push(ResponseContentBlock::Raw(protocol::unavailable(&id)));
}

/// Serves the executor's `advisor` calls by consulting the advisor model.
pub struct AdvisorTool {
    cfg: AdvisorConfig,
    /// The consultation cap for one turn ([`advisor_cap`]).
    cap: usize,
    /// SUCCESSFUL consultations so far this turn: the number `max_uses`
    /// limits. A failed one is not counted, so one flaky advisor response
    /// does not use up the operator's allowance.
    uses: usize,
    /// At least one consultation was served (so a failed re-run leaves the
    /// client a degraded pair).
    consulted: bool,
    /// The cap was reached and the refusal was already sent.
    cap_reached: bool,
    /// Consultations that failed this turn (after their retry). They do not
    /// count toward `max_uses`; this counter is what bounds them instead.
    failures: usize,
    /// [`MAX_FAILED_CONSULTATIONS`] was reached: the advisor is treated as
    /// down for the rest of the turn.
    given_up: bool,
    /// One operator-visible warning per turn.
    warned: bool,
    /// One runaway warning per turn.
    runaway_warned: bool,
    /// `usage.iterations`: one `message` per executor sampling, plus one
    /// `advisor_message` per successful consultation, in order.
    iterations: Vec<Value>,
}

impl AdvisorTool {
    #[must_use]
    pub fn new(cfg: &AdvisorConfig) -> Self {
        Self {
            cap: advisor_cap(cfg),
            cfg: cfg.clone(),
            uses: 0,
            consulted: false,
            cap_reached: false,
            failures: 0,
            given_up: false,
            warned: false,
            runaway_warned: false,
            iterations: Vec::new(),
        }
    }

    /// One consultation, or the refusal if the cap was reached.
    async fn answer(&mut self, attempt: &Attempt) -> Served {
        let id = protocol::new_id();
        let mut blocks = vec![ResponseContentBlock::Raw(protocol::server_tool_use(&id))];

        // Terminal cap path: refuse, tell the executor, and become exhausted.
        // The loop gives the executor ONE more run for a real answer.
        if self.uses >= self.cap {
            tracing::warn!(
                advisor_model = %self.cfg.model,
                max_uses = ?self.cfg.max_uses,
                consultations = self.uses,
                "advisor consultation refused: max_uses exceeded"
            );
            warn_once(&attempt.state, &mut self.warned, &self.cfg);
            blocks.push(ResponseContentBlock::Raw(protocol::max_uses_exceeded(&id)));
            self.cap_reached = true;
            return Served {
                blocks,
                output: NO_MORE_ADVICE.to_owned(),
                summary: CallSummary::new(
                    ADVISOR_TOOL_NAME,
                    self.cfg.model.as_str(),
                    OUTCOME_REFUSED,
                )
                .with_detail(protocol::ERROR_CODE_MAX_USES_EXCEEDED),
            };
        }
        self.consulted = true;

        // What the operator sees: the outcome and the advisor's own tokens.
        let mut outcome = OUTCOME_OK;
        let mut detail: Option<String> = None;

        let prompt = transcript(attempt);
        let advice = match consult(attempt, &self.cfg, &prompt).await {
            Ok((text, usage)) => {
                self.uses += 1;
                // A turn that keeps consulting is a runaway loop. Warn ONCE
                // per turn, at the moment it crosses the threshold, so the
                // spin is visible in the log without flooding it.
                if should_warn_runaway(self.uses, &mut self.runaway_warned) {
                    tracing::warn!(
                        consultations = self.uses,
                        advisor_model = %self.cfg.model,
                        max_uses = ?self.cfg.max_uses,
                        "advisor consultations in one turn exceeded the runaway threshold — \
                         each iteration costs two upstream calls (executor re-run + advisor)"
                    );
                }
                if let Some(u) = usage.as_ref() {
                    // Advisor tokens live in iterations[], NOT the top-level
                    // totals. The TUI gets one extra "actual" bucket so the
                    // spend is at least visible.
                    self.iterations
                        .push(protocol::advisor_iteration(&self.cfg.model, u));
                    attempt
                        .state
                        .stats
                        .record_actual(self.cfg.model.clone(), token_usage(u));
                    let cached = u
                        .input_tokens_details
                        .as_ref()
                        .map_or(0, |d| d.cached_tokens);
                    detail = Some(format!(
                        "{} in ({} cached) · {} out",
                        format_token_count(u64::from(u.input_tokens)),
                        format_token_count(u64::from(cached)),
                        format_token_count(u64::from(u.output_tokens))
                    ));
                }
                blocks.push(ResponseContentBlock::Raw(protocol::advisor_result(
                    &id, &text,
                )));
                text
            }
            Err(error) => {
                outcome = OUTCOME_ERROR;
                let code = error.kind.client_code();
                detail = Some(code.to_owned());
                // Not counted in `uses`, but not free either: after
                // MAX_FAILED_CONSULTATIONS the advisor is treated as down,
                // so an executor cannot keep paying for a broken advisor.
                self.failures += 1;
                let giving_up = self.failures >= MAX_FAILED_CONSULTATIONS;
                self.given_up |= giving_up;
                // Every failure is logged (short: never the transcript), with
                // enough to tell a flaky advisor from a broken one. Only the
                // TUI line is limited to once per turn.
                tracing::warn!(
                    error = %error.message,
                    failure = code,
                    advisor_model = %self.cfg.model,
                    failures_this_turn = self.failures,
                    prompt_chars = prompt.len(),
                    giving_up,
                    "advisor consultation failed; continuing without advice"
                );
                warn_once(&attempt.state, &mut self.warned, &self.cfg);
                blocks.push(ResponseContentBlock::Raw(protocol::error_result(&id, code)));
                // Tell the executor WHY, so it can adapt. Once given up on,
                // say that instead: the executor gets one last run and any
                // further advisor call in it is dropped, so "carry on
                // without it" alone would leave it nothing to answer.
                if giving_up {
                    NO_MORE_ADVICE_BROKEN
                } else {
                    error.kind.executor_text()
                }
                .to_owned()
            }
        };

        // Server-side advice injection: emitting the Anthropic block alone is
        // NOT enough. The loop injects `advice` into the executor's own
        // prompt (§3.4).
        let mut summary = CallSummary::new(ADVISOR_TOOL_NAME, self.cfg.model.as_str(), outcome);
        if let Some(detail) = detail {
            summary = summary.with_detail(detail);
        }
        Served {
            blocks,
            output: advice,
            summary,
        }
    }
}

impl ServerTool for AdvisorTool {
    fn executor_name(&self) -> &'static str {
        ADVISOR_TOOL_NAME
    }

    fn serve<'a>(&'a mut self, attempt: &'a Attempt, _call: &'a Call) -> BoxFuture<'a, Served> {
        Box::pin(self.answer(attempt))
    }

    fn on_executor_response(&mut self, resp: &ResponsesResponse) {
        if let Some(usage) = resp.usage.as_ref() {
            self.iterations.push(protocol::message_iteration(usage));
        }
    }

    fn exhausted(&self) -> bool {
        self.cap_reached || self.given_up
    }

    /// A consultation was pending when the re-run failed: add the
    /// `unavailable` pair. At the cap, or once the advisor was given up on, no
    /// consultation is pending (the refusal or failure was already served),
    /// so the last good response is rendered as it is.
    fn on_rerun_failure(&mut self, prefix: &mut Vec<ResponseContentBlock>) {
        if self.consulted && !self.exhausted() {
            append_degraded_pair(prefix);
        }
    }

    /// Billing lives in `iterations[]`, not the top-level totals. A turn with
    /// no usage has NO `iterations` field at all, so the wire is
    /// byte-identical to the non-advisor path.
    fn contribute_usage(&self, usage: &mut Usage) {
        if !self.iterations.is_empty() {
            usage.iterations = Some(self.iterations.clone());
        }
    }

    fn scrub_error(&self, err: AppError) -> AppError {
        AppError::scrub_advisor_error(err)
    }
}
#[cfg(test)]
mod tests;
