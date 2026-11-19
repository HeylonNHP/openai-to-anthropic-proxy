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
//! `protocol::unavailable`, at most one TUI warning is emitted for the whole
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

/// Text injected into the executor's transcript when the advisor could not
/// be reached. It must NOT look like advice — the client sees an
/// `advisor_tool_result_error` block, and the executor must not be told a
/// review happened.
const NO_ADVICE: &str =
    "Advisor is temporarily unavailable. Continue the task without a second opinion.";

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
        instructions: Some(
            "You are an independent reviewer advising another model. Read the transcript \
             below and write ONE concise paragraph of actionable advice. Do not call tools, \
             do not address the user, and do not repeat the question. Reply with the advice only."
                .to_owned(),
        ),
        tools: None,
        tool_choice: None,
        reasoning: cfg.effort.as_ref().map(|effort| ReasoningConfig {
            effort: effort.clone(),
            ..ReasoningConfig::default()
        }),
        text: None,
        temperature: None,
        top_p: None,
        // `max_tokens: None` (and `Some(0)`, which means the same thing) =
        // "no limit": OMIT the field so the UPSTREAM's own default applies
        // (it may well be smaller than 2048).
        max_output_tokens: effective_limit(cfg.max_tokens),
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
/// plus its whole (Responses-shaped) transcript, including the advisor call
/// that triggered this consultation.
fn transcript(attempt: &Attempt) -> String {
    let instructions = attempt.outbound.instructions.as_deref().unwrap_or("");
    let input =
        serde_json::to_string_pretty(&attempt.outbound.input).unwrap_or_else(|_| "[]".to_owned());
    format!(
        "<system>\n{instructions}\n</system>\n\n\
         <transcript>\n{input}\n</transcript>\n\n\
         Advise the assistant on how to proceed."
    )
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
fn advice_from_response(
    resp: &ResponsesResponse,
) -> Result<(String, Option<ResponsesUsage>), String> {
    if resp.status != "completed" {
        return Err(format!("advisor response status is {}", resp.status));
    }
    let text = extract_advice(resp).ok_or_else(|| "advisor returned no text".to_owned())?;
    Ok((text, resp.usage.clone()))
}

/// One standalone advisor POST. Every failure is a `String`, never an
/// `AppError`: the caller degrades instead of failing the client.
async fn consult(
    attempt: &Attempt,
    cfg: &AdvisorConfig,
    prompt: &str,
) -> Result<(String, Option<ResponsesUsage>), String> {
    let outbound = advisor_request(cfg, prompt);
    let url = attempt.url();
    // `None` = no per-request timeout override ("no limit"): the global
    // `request_timeout` — the timeout the reqwest client was built with —
    // still applies.
    let timeout = advisor_timeout(cfg, attempt.state.config.request_timeout);
    let resp = send_upstream_once(&attempt.state, &url, &attempt.headers, &outbound, timeout)
        .await
        .map_err(|e| e.to_string())?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        let body: String = body.chars().take(300).collect();
        return Err(format!(
            "upstream returned {status}: {}",
            scrub_error_body(&body)
        ));
    }

    let parsed = parse_upstream_body(resp).await.map_err(|e| e.to_string())?;
    advice_from_response(&parsed)
}

/// The gate behind `warn_once`: exactly ONE warning per turn, covering
/// BOTH the `tracing::warn!` and the operator-visible TUI line.
fn should_warn(warned: &mut bool) -> bool {
    if *warned {
        false
    } else {
        *warned = true;
        true
    }
}

/// Structured warning + at most ONE operator-visible line per turn. Both the
/// `tracing::warn!` and the TUI line are gated — the first failure is the
/// useful one, repeats are noise.
fn warn_once(state: &AppState, warned: &mut bool, cfg: &AdvisorConfig, error: &str) {
    if !should_warn(warned) {
        return;
    }
    tracing::warn!(
        error = %error,
        advisor_model = %cfg.model,
        "advisor consultation failed; continuing without advice"
    );
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
    /// Consultations made so far this turn.
    uses: usize,
    /// At least one consultation was served (so a failed re-run leaves the
    /// client a degraded pair).
    consulted: bool,
    /// The cap was reached and the refusal was already sent.
    cap_reached: bool,
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
            warn_once(
                &attempt.state,
                &mut self.warned,
                &self.cfg,
                "advisor max_uses exceeded",
            );
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
        self.uses += 1;
        self.consulted = true;

        // A turn that keeps consulting is a runaway loop. Warn ONCE per turn,
        // at the moment it crosses the threshold, so the spin is visible in
        // the log without flooding it.
        if should_warn_runaway(self.uses, &mut self.runaway_warned) {
            tracing::warn!(
                consultations = self.uses,
                advisor_model = %self.cfg.model,
                max_uses = ?self.cfg.max_uses,
                "advisor consultations in one turn exceeded the runaway threshold — \
                 each iteration costs two upstream calls (executor re-run + advisor)"
            );
        }

        // What the operator sees: the outcome and the advisor's own tokens.
        let mut outcome = OUTCOME_OK;
        let mut detail: Option<String> = None;

        let advice = match consult(attempt, &self.cfg, &transcript(attempt)).await {
            Ok((text, usage)) => {
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
                    detail = Some(format!(
                        "{} in · {} out",
                        format_token_count(u64::from(u.input_tokens)),
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
                detail = Some(protocol::ERROR_CODE_UNAVAILABLE.to_owned());
                warn_once(&attempt.state, &mut self.warned, &self.cfg, &error);
                blocks.push(ResponseContentBlock::Raw(protocol::unavailable(&id)));
                NO_ADVICE.to_owned()
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
        self.cap_reached
    }

    /// A consultation was pending when the re-run failed: add the
    /// `unavailable` pair. At the cap no consultation is pending, so the last
    /// good response is rendered as it is.
    fn on_rerun_failure(&mut self, prefix: &mut Vec<ResponseContentBlock>) {
        if self.consulted && !self.cap_reached {
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
