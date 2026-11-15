//! The bounded consultation loop (§§11.3, 11.4, 11.5, 15, 17.1 of
//! `ADVISOR_TOOL_RESEARCH.md`).
//!
//! ## Architecture choice (a)
//!
//! This module calls [`crate::proxy::Attempt::execute`] directly, 2–3 times
//! per turn. That is exactly what phase 1's `Attempt` / `build_outbound` /
//! `execute` extraction was for: the whole retry/capability/fallback policy
//! lives in ONE reusable unit, so the advisor never has to duplicate it (the
//! §13 "bypass is a second proxy" failure mode). Option (b) — keeping the
//! loop in `proxy.rs` and putting only pure helpers here — was rejected
//! because it would have re-introduced a second send loop in the very commit
//! that removes the first duplication. `Attempt`, `execute`, `Outcome` and
//! the three reusable helpers are `pub(crate)`; nothing leaks into the
//! public API.
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
//! `consult` returns `Result<_, String>` and NEVER propagates an error to the
//! client. Every failure degrades to `protocol::unavailable`, at most one TUI
//! warning is emitted for the whole turn, and the executor's transcript is
//! still given a `function_call_output` so the upstream never sees an orphan
//! `function_call`. The loop is bounded by `AdvisorConfig::max_uses`.
//!
//! After the FIRST successful executor response, a failure on a re-run is
//! ALSO fail-closed: the last good executor response is rendered and an
//! `unavailable` pair is appended. No `AppError` from a re-run can reach the
//! client — a 400 naming the advisor beta/tool would latch the feature off
//! in Claude Code for the whole process (§18.4).

use std::time::Duration;

use serde_json::Value;

use crate::advisor::{protocol, scrub_error_body};
use crate::anthropic::ResponseContentBlock;
use crate::config::AdvisorConfig;
use crate::proxy::{
    AppState, Attempt, Outcome, parse_upstream_body, send_upstream_once, token_usage,
};
use crate::responses::{
    Input, InputContentPart, InputItem, OutputContentPart, OutputItem, ReasoningConfig,
    ResponsesRequest, ResponsesResponse, ResponsesUsage,
};
use crate::translate;
use crate::tui::{LogKind, LogLine};

/// What the consultation loop produced.
pub enum SessionOutcome {
    /// A fully buffered, assembled final response ready for rendering.
    /// Boxed so the enum is not dominated by the buffered response (the
    /// streaming variant carries only a `reqwest::Response`).
    Complete(Box<SessionResult>),
    /// `prepare` forces `stream:false`, but if an upstream somehow answers
    /// with a stream anyway, degrade to the pre-existing streaming path
    /// rather than erroring the client.
    UnexpectedStreaming(reqwest::Response),
}

/// The buffered result of the whole turn: the LAST executor response, the
/// ordered content produced BEFORE it, and the per-iteration billing entries.
pub struct SessionResult {
    pub final_resp: ResponsesResponse,
    /// Ordered prefix: translated content of every INTERMEDIATE executor
    /// sampling (minus `tool_use{name:"advisor"}`) interleaved with the
    /// contiguous `server_tool_use` / `advisor_tool_result` pairs, in the
    /// order they actually happened. Spliced in front of `final_resp`'s
    /// content during rendering.
    pub prefix_blocks: Vec<ResponseContentBlock>,
    pub iterations: Vec<Value>,
}

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

/// Clamp the advisor's own per-request timeout to the operator's global
/// `request_timeout`. reqwest's per-request `.timeout()` OVERRIDES the
/// client-level one, so a large `advisor.timeout_secs` must not be allowed
/// to EXTEND the operator's bound (§11.4).
#[must_use]
fn advisor_timeout(cfg: &AdvisorConfig, request_timeout: Duration) -> Duration {
    Duration::from_secs(cfg.timeout_secs).min(request_timeout)
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
        max_output_tokens: Some(cfg.max_tokens),
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

/// Find the FIRST `function_call{name:"advisor"}` in an executor response.
fn find_advisor_call(output: &[OutputItem]) -> Option<(String, String)> {
    output.iter().find_map(|item| match item {
        OutputItem::FunctionCall {
            call_id,
            name,
            arguments,
            ..
        } if name == "advisor" => Some((call_id.clone(), arguments.clone())),
        _ => None,
    })
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
    let resp = send_upstream_once(
        &attempt.state,
        &url,
        &attempt.headers,
        &outbound,
        Some(advisor_timeout(cfg, attempt.state.config.request_timeout)),
    )
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

/// Canonical Responses shape for feeding a tool result back to the executor
/// (the same shape `repair::append_corrective_input` uses, which the upstream
/// definitely accepts). Note: if an upstream turns out to need a plain
/// message instead, this is the ONE place to change.
fn inject(attempt: &mut Attempt, call_id: &str, arguments: &str, output: &str) {
    if let Input::Items(items) = &mut attempt.outbound.input {
        items.push(InputItem::FunctionCall {
            call_id: call_id.to_owned(),
            name: "advisor".to_owned(),
            arguments: arguments.to_owned(),
        });
        items.push(InputItem::FunctionCallOutput {
            call_id: call_id.to_owned(),
            output: output.to_owned(),
        });
    }
}

/// Translated content of one executor sampling, minus the internal advisor
/// `tool_use`. Kept in order so intermediate text + sibling client tool uses
/// survive to the final Anthropic message.
fn intermediate_blocks(resp: &ResponsesResponse, attempt: &Attempt) -> Vec<ResponseContentBlock> {
    let mut blocks = translate::responses_to_anthropic(resp, &attempt.registry).content;
    blocks.retain(
        |block| !matches!(block, ResponseContentBlock::ToolUse { name, .. } if name == "advisor"),
    );
    blocks
}

/// The response we keep as `last_good` when the loop CONTINUES: its content
/// has already been spliced into `prefix_blocks`, so rendering it verbatim
/// on a later failure would duplicate it. Keep the metadata (id, model,
/// status, usage, stop-reason inputs) and drop only `output`.
fn without_output(resp: &ResponsesResponse) -> ResponsesResponse {
    let mut clone = resp.clone();
    clone.output.clear();
    clone
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

fn complete(
    final_resp: ResponsesResponse,
    prefix_blocks: Vec<ResponseContentBlock>,
    iterations: Vec<Value>,
) -> Result<SessionOutcome, crate::error::AppError> {
    Ok(SessionOutcome::Complete(Box::new(SessionResult {
        final_resp,
        prefix_blocks,
        iterations,
    })))
}

/// Run the whole detection-triggered turn. The FIRST executor failure is an
/// ordinary upstream failure and propagates (the caller scrubs any advisor
/// beta/tool tokens from the body before it reaches the client); every LATER
/// executor failure degrades to the last good response. Advisor failures
/// never propagate.
pub async fn run(
    attempt: &mut Attempt,
    cfg: &AdvisorConfig,
) -> Result<SessionOutcome, crate::error::AppError> {
    let url = attempt.url();
    let mut prefix_blocks: Vec<ResponseContentBlock> = Vec::new();
    let mut iterations: Vec<Value> = Vec::new();
    let mut warned = false;
    let mut uses = 0usize;
    let cap = usize::try_from(cfg.max_uses).unwrap_or(usize::MAX);
    // Once set, the very next executor response is TERMINAL: no more
    // consultations and no more re-runs. This bounds the whole chain even
    // when the executor keeps asking for advice.
    let mut cap_reached = false;
    // The last successfully buffered executor response. Once present, ANY
    // later `execute` error renders THIS instead of failing the client.
    let mut last_good: Option<ResponsesResponse> = None;

    loop {
        // The executor's own pipe: capabilities, parameter-strip retry,
        // one-shot default_model fallback, error mapping, corrective retry.
        let resp = match attempt.execute(&url).await {
            Ok(Outcome::Buffered(body)) => body,
            Ok(Outcome::Streaming(raw)) => {
                tracing::warn!(
                    advisor_model = %cfg.model,
                    "advisor path expected a buffered response; rendering the upstream stream directly"
                );
                return Ok(SessionOutcome::UnexpectedStreaming(raw));
            }
            Err(err) => {
                let Some(last) = last_good.take() else {
                    // First executor attempt: an ordinary upstream failure.
                    // `Attempt::run` scrubs the body before it reaches the
                    // client; no last-good response exists to degrade to.
                    return Err(err);
                };
                warn_once(&attempt.state, &mut warned, cfg, &err.to_string());
                // A consultation was pending when the re-run failed → add
                // the `unavailable` pair. At the cap no consultation is
                // pending, so just render the last good response.
                if !cap_reached {
                    append_degraded_pair(&mut prefix_blocks);
                }
                return complete(last, prefix_blocks, iterations);
            }
        };

        // One `message` iteration per executor sampling, in order.
        if let Some(usage) = resp.usage.as_ref() {
            iterations.push(protocol::message_iteration(usage));
        }

        let Some((call_id, arguments)) = find_advisor_call(&resp.output) else {
            return complete(resp, prefix_blocks, iterations);
        };

        // An advisor call was emitted. Everything the executor said in this
        // sampling is an INTERMEDIATE block: keep all of it except the
        // internal advisor tool_use.
        prefix_blocks.extend(intermediate_blocks(&resp, attempt));

        let id = protocol::new_id();
        prefix_blocks.push(ResponseContentBlock::Raw(protocol::server_tool_use(&id)));

        // Terminal cap path: inject a FunctionCallOutput for the pending
        // call and re-run the executor ONE last time for a real answer.
        // That extra re-run is terminal — if it calls `advisor` again we
        // strip that tool_use and render without consulting.
        if uses >= cap || cap_reached {
            if !cap_reached {
                warn_once(
                    &attempt.state,
                    &mut warned,
                    cfg,
                    "advisor max_uses exceeded",
                );
                prefix_blocks.push(ResponseContentBlock::Raw(protocol::max_uses_exceeded(&id)));
                inject(attempt, &call_id, &arguments, NO_MORE_ADVICE);
                cap_reached = true;
                last_good = Some(without_output(&resp));
                continue;
            }
            // The terminal extra re-run still asked for advice. Do NOT
            // consult again and do NOT POST again. Close the pair and
            // render (the advisor tool_use is filtered out below).
            prefix_blocks.push(ResponseContentBlock::Raw(protocol::max_uses_exceeded(&id)));
            return complete(resp, prefix_blocks, iterations);
        }
        uses += 1;

        let advice = match consult(attempt, cfg, &transcript(attempt)).await {
            Ok((text, usage)) => {
                if let Some(u) = usage.as_ref() {
                    // Advisor tokens live in iterations[], NOT the top-level
                    // totals. The TUI gets one extra "actual" bucket so the
                    // spend is at least visible.
                    iterations.push(protocol::advisor_iteration(&cfg.model, u));
                    attempt
                        .state
                        .stats
                        .record_actual(cfg.model.clone(), token_usage(u));
                }
                prefix_blocks.push(ResponseContentBlock::Raw(protocol::advisor_result(
                    &id, &text,
                )));
                text
            }
            Err(error) => {
                warn_once(&attempt.state, &mut warned, cfg, &error);
                prefix_blocks.push(ResponseContentBlock::Raw(protocol::unavailable(&id)));
                NO_ADVICE.to_owned()
            }
        };

        // Server-side advice injection: emitting the Anthropic block alone
        // is NOT enough — the executor must see the text in its own prompt
        // (§3.4).
        inject(attempt, &call_id, &arguments, &advice);
        last_good = Some(without_output(&resp));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg() -> AdvisorConfig {
        AdvisorConfig {
            enabled: true,
            model: "beam".into(),
            effort: Some("medium".into()),
            max_uses: 2,
            max_tokens: 64,
            timeout_secs: 2,
        }
    }

    fn response(status: &str, text: Option<&str>) -> ResponsesResponse {
        let mut v = json!({
            "id": "resp_a", "object": "response", "created_at": 1,
            "status": status, "model": "beam", "output": [],
            "usage": {"input_tokens": 7, "output_tokens": 3, "total_tokens": 10}
        });
        if let Some(text) = text {
            v["output"] = json!([{
                "type": "message", "role": "assistant",
                "content": [{"type": "output_text", "text": text, "annotations": []}]
            }]);
        }
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn advisor_request_is_tool_free_buffered_and_explicitly_modeled() {
        let v = serde_json::to_value(advisor_request(&cfg(), "transcript")).unwrap();
        assert_eq!(v["model"], "beam");
        assert_eq!(v["stream"], false);
        assert!(v.get("tools").is_none(), "no tools on the advisor call");
        assert!(v.get("tool_choice").is_none());
        assert!(v.get("temperature").is_none());
        assert!(v.get("top_p").is_none());
        assert_eq!(v["max_output_tokens"], 64);
        assert_eq!(v["reasoning"]["effort"], "medium");
        assert_eq!(v["input"][0]["role"], "user");
        assert_eq!(v["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(v["input"][0]["content"][0]["text"], "transcript");
        assert!(
            v["instructions"]
                .as_str()
                .unwrap()
                .contains("independent reviewer")
        );
    }

    #[test]
    fn advisor_request_omits_reasoning_when_no_effort() {
        let mut c = cfg();
        c.effort = None;
        let v = serde_json::to_value(advisor_request(&c, "t")).unwrap();
        assert!(v.get("reasoning").is_none());
    }

    #[test]
    fn should_warn_gates_tracing_and_tui_warnings_to_once_per_turn() {
        let mut warned = false;
        assert!(should_warn(&mut warned));
        assert!(!should_warn(&mut warned), "second failure must not warn");
        assert!(!should_warn(&mut warned), "third failure must not warn");
        let mut fresh = false;
        assert!(should_warn(&mut fresh), "a new turn warns again");
    }

    #[test]
    fn advisor_timeout_is_clamped_to_the_global_request_timeout() {
        let mut c = cfg();
        c.timeout_secs = 5;
        assert_eq!(
            advisor_timeout(&c, Duration::from_millis(300)),
            Duration::from_millis(300)
        );
        c.timeout_secs = 1;
        assert_eq!(
            advisor_timeout(&c, Duration::from_secs(30)),
            Duration::from_secs(1)
        );
        c.timeout_secs = 0;
        assert_eq!(advisor_timeout(&c, Duration::from_secs(30)), Duration::ZERO);
    }

    #[test]
    fn advice_from_response_requires_completed_status() {
        let ok = advice_from_response(&response("completed", Some("Use a channel."))).unwrap();
        assert_eq!(ok.0, "Use a channel.");
        assert_eq!(ok.1.unwrap().input_tokens, 7);
        for bad in ["failed", "incomplete", "in_progress", "queued", "cancelled"] {
            let err = advice_from_response(&response(bad, Some("Not real advice."))).unwrap_err();
            assert!(err.contains("status is"), "{bad}: {err}");
        }
        // Even a completed response with no text is a failure.
        assert!(
            advice_from_response(&response("completed", None))
                .unwrap_err()
                .contains("no text")
        );
    }

    #[test]
    fn find_advisor_call_ignores_other_tools() {
        let items: Vec<OutputItem> = serde_json::from_value(json!([
            {"type": "function_call", "call_id": "call_bash", "name": "Bash", "arguments": "{}"},
            {"type": "function_call", "call_id": "call_adv", "name": "advisor", "arguments": "{}"}
        ]))
        .unwrap();
        assert_eq!(
            find_advisor_call(&items),
            Some(("call_adv".to_owned(), "{}".to_owned()))
        );
        let plain: Vec<OutputItem> = serde_json::from_value(json!([
            {"type": "message", "role": "assistant", "content": []}
        ]))
        .unwrap();
        assert_eq!(find_advisor_call(&plain), None);
    }

    #[test]
    fn extract_advice_requires_non_empty_text() {
        let parse = |v: Value| serde_json::from_value::<ResponsesResponse>(v).unwrap();
        let base = json!({
            "id": "resp_a", "object": "response", "created_at": 1,
            "status": "completed", "model": "beam", "output": []
        });
        let mut with_text = base.clone();
        with_text["output"] = json!([{
            "type": "message", "role": "assistant",
            "content": [{"type": "output_text", "text": "  Use a channel.  ", "annotations": []}]
        }]);
        assert_eq!(
            extract_advice(&parse(with_text)).as_deref(),
            Some("Use a channel.")
        );
        let mut blank = base.clone();
        blank["output"] = json!([{
            "type": "message", "role": "assistant",
            "content": [{"type": "output_text", "text": "   ", "annotations": []}]
        }]);
        assert_eq!(extract_advice(&parse(blank)), None);
        assert_eq!(extract_advice(&parse(base)), None);
    }
}
