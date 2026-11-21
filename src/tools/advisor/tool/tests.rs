use super::*;
use serde_json::json;

fn cfg() -> AdvisorConfig {
    AdvisorConfig {
        enabled: true,
        model: "beam".into(),
        effort: Some("medium".into()),
        max_uses: Some(2),
        max_tokens: Some(64),
        reasoning_max_tokens: Some(100),
        timeout_secs: Some(2),
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
    assert_eq!(v["max_output_tokens"], 164, "answer 64 + reasoning 100");
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
    // Unlimited: no per-request override at all — the global
    // `request_timeout` (the reqwest client's own timeout) applies.
    c.timeout_secs = None;
    assert_eq!(advisor_timeout(&c, Duration::from_millis(300)), None);
    assert_eq!(advisor_timeout(&c, Duration::from_secs(30)), None);
    // A bounded value is clamped to the global bound, never above it.
    c.timeout_secs = Some(5);
    assert_eq!(
        advisor_timeout(&c, Duration::from_millis(300)),
        Some(Duration::from_millis(300))
    );
    c.timeout_secs = Some(1);
    assert_eq!(
        advisor_timeout(&c, Duration::from_secs(30)),
        Some(Duration::from_secs(1))
    );
    // `Some(0)` means the SAME thing as `None` — "no limit" — whether it
    // came from the resolver or from a hand-built config. `0` has
    // exactly ONE meaning in the whole codebase.
    c.timeout_secs = Some(0);
    assert_eq!(advisor_timeout(&c, Duration::from_secs(30)), None);
}

#[test]
fn advisor_cap_is_unbounded_when_max_uses_is_unlimited() {
    let mut c = cfg();
    assert_eq!(advisor_cap(&c), 2);
    c.max_uses = None;
    assert_eq!(
        advisor_cap(&c),
        usize::MAX,
        "no limit means the loop is bounded only by the executor"
    );
    // `Some(0)` is the SAME thing as `None` — "no limit" — so `0` has
    // one meaning everywhere, not two.
    c.max_uses = Some(0);
    assert_eq!(advisor_cap(&c), usize::MAX);
    // A cap that does not fit `usize` on this platform means "no
    // limit" rather than a panic.
    c.max_uses = Some(u32::MAX);
    assert_eq!(
        advisor_cap(&c),
        usize::try_from(u32::MAX).unwrap_or(usize::MAX)
    );
}

/// DEFECT 3: `Some(0)` in a resolved `AdvisorConfig` must mean exactly
/// what a configured `0` means — "no limit" — and NOT "zero
/// consultations" / "zero seconds" / "a zero-token answer". One
/// interpretation of `0` in the whole codebase.
#[test]
fn a_zero_limit_means_no_limit_everywhere() {
    let mut c = cfg();

    // max_uses: 0 = no limit (the loop is bounded only by the executor).
    c.max_uses = Some(0);
    assert_eq!(advisor_cap(&c), usize::MAX);
    c.max_uses = None;
    assert_eq!(advisor_cap(&c), usize::MAX);
    // A positive bound still binds.
    c.max_uses = Some(3);
    assert_eq!(advisor_cap(&c), 3);

    // timeout_secs: 0 = no per-request override (the global timeout
    // applies).
    c.timeout_secs = Some(0);
    assert_eq!(advisor_timeout(&c, Duration::from_secs(30)), None);
    c.timeout_secs = Some(7);
    assert_eq!(
        advisor_timeout(&c, Duration::from_secs(30)),
        Some(Duration::from_secs(7))
    );

    // max_tokens: 0 = the proxy imposes no cap (the field is omitted).
    c.max_tokens = Some(0);
    let v = serde_json::to_value(advisor_request(&c, "transcript")).unwrap();
    assert!(
        v.get("max_output_tokens").is_none(),
        "0 means no limit, so the field is omitted: {v}"
    );
    c.max_tokens = Some(64);
    let v = serde_json::to_value(advisor_request(&c, "transcript")).unwrap();
    assert_eq!(v["max_output_tokens"], 164, "answer 64 + reasoning 100");
}

#[test]
fn advisor_request_omits_max_output_tokens_when_unlimited() {
    let mut c = cfg();
    c.max_tokens = None;
    let v = serde_json::to_value(advisor_request(&c, "transcript")).unwrap();
    assert!(
        v.get("max_output_tokens").is_none(),
        "no limit means the upstream's own default applies: {v}"
    );
    // The bounded case still sends the cap.
    c.max_tokens = Some(64);
    let v = serde_json::to_value(advisor_request(&c, "transcript")).unwrap();
    assert_eq!(v["max_output_tokens"], 164, "answer 64 + reasoning 100");
}

#[test]
fn runaway_warning_fires_once_past_the_threshold() {
    let mut warned = false;
    assert!(!should_warn_runaway(1, &mut warned));
    assert!(
        !should_warn_runaway(RUNAWAY_CONSULTATION_THRESHOLD, &mut warned),
        "the threshold itself is not yet a runaway"
    );
    assert!(should_warn_runaway(
        RUNAWAY_CONSULTATION_THRESHOLD + 1,
        &mut warned
    ));
    assert!(
        !should_warn_runaway(RUNAWAY_CONSULTATION_THRESHOLD + 5, &mut warned),
        "once per turn, not once per consultation"
    );
    let mut fresh = false;
    assert!(
        should_warn_runaway(RUNAWAY_CONSULTATION_THRESHOLD + 1, &mut fresh),
        "a new turn warns again"
    );
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

fn tool() -> AdvisorTool {
    AdvisorTool::new(&cfg())
}

fn kinds(prefix: &[ResponseContentBlock]) -> Vec<String> {
    prefix
        .iter()
        .map(|b| match b {
            ResponseContentBlock::Raw(v) => v["type"].as_str().unwrap_or("").to_owned(),
            _ => "other".to_owned(),
        })
        .collect()
}

#[test]
fn the_tool_answers_to_the_advisor_function() {
    assert_eq!(tool().executor_name(), "advisor");
}

#[test]
fn a_fresh_tool_is_not_exhausted_and_has_no_usage() {
    let t = tool();
    assert!(!t.exhausted());
    let mut usage = Usage::default();
    t.contribute_usage(&mut usage);
    assert!(usage.iterations.is_none(), "no iterations field at all");
}

#[test]
fn every_executor_sampling_is_an_iteration_in_order() {
    let mut t = tool();
    t.on_executor_response(&response("completed", None));
    t.on_executor_response(&response("completed", None));
    let mut usage = Usage::default();
    t.contribute_usage(&mut usage);
    let iterations = usage.iterations.expect("iterations");
    assert_eq!(iterations.len(), 2);
    assert!(iterations.iter().all(|i| i["type"] == "message"));
}

#[test]
fn a_response_without_usage_adds_no_iteration() {
    let mut t = tool();
    let mut resp = response("completed", None);
    resp.usage = None;
    t.on_executor_response(&resp);
    let mut usage = Usage::default();
    t.contribute_usage(&mut usage);
    assert!(usage.iterations.is_none());
}

#[test]
fn a_failed_rerun_after_a_consultation_leaves_an_unavailable_pair() {
    let mut t = tool();
    t.consulted = true;
    let mut prefix = Vec::new();
    t.on_rerun_failure(&mut prefix);
    assert_eq!(kinds(&prefix), ["server_tool_use", "advisor_tool_result"]);
    assert_eq!(
        match &prefix[1] {
            ResponseContentBlock::Raw(v) => v["content"]["error_code"].clone(),
            _ => Value::Null,
        },
        "unavailable"
    );
}

#[test]
fn a_failed_rerun_with_no_consultation_adds_nothing() {
    // The tool was declared but never called (the work was another tool's).
    let mut prefix = Vec::new();
    tool().on_rerun_failure(&mut prefix);
    assert!(prefix.is_empty());
}

#[test]
fn a_failed_rerun_at_the_cap_adds_nothing() {
    // The refusal pair was already sent; no consultation is pending.
    let mut t = tool();
    t.consulted = true;
    t.cap_reached = true;
    let mut prefix = Vec::new();
    t.on_rerun_failure(&mut prefix);
    assert!(prefix.is_empty());
    assert!(t.exhausted());
}

#[test]
fn upstream_errors_are_scrubbed_of_advisor_tokens() {
    let err = tool().scrub_error(AppError::BadRequest(
        "bad: advisor_20260301 not allowed".to_owned(),
    ));
    assert!(matches!(&err, AppError::BadRequest(m) if !m.contains("advisor_20260301")));
}

#[test]
fn encrypted_reasoning_is_stripped_from_the_advisor_transcript() {
    let mut input = json!([
        {"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "BIG-OPAQUE-BLOB"},
        {"type": "message", "role": "assistant",
         "content": [{"type": "output_text", "text": "plan"}]},
        {"nested": [{"encrypted_content": "x", "keep": 1}]},
    ]);
    strip_encrypted_content(&mut input);
    let text = input.to_string();
    assert!(!text.contains("encrypted_content"), "{text}");
    assert!(!text.contains("BIG-OPAQUE-BLOB"), "{text}");
    assert!(text.contains("rs_1") && text.contains("plan") && text.contains("\"keep\":1"));
}

fn tool_result(call_id: &str, output: &str) -> Value {
    json!({"type": "function_call_output", "call_id": call_id, "output": output})
}

fn user_message(text: &str) -> Value {
    json!({"type": "message", "role": "user",
           "content": [{"type": "input_text", "text": text}]})
}

#[test]
fn a_transcript_within_budget_is_left_alone() {
    let input = json!([user_message("task"), tool_result("c1", "short result")]);
    let out = build_transcript("sys", input, 100_000);
    assert!(
        out.contains("task") && out.contains("short result"),
        "{out}"
    );
    assert!(!out.contains("omitted"), "{out}");
}

#[test]
fn the_transcript_is_compact_json() {
    let out = build_transcript("sys", json!([user_message("task")]), 100_000);
    assert!(!out.contains("\n  "), "no pretty-print indentation: {out}");
}

#[test]
fn inline_image_data_is_replaced_by_a_placeholder() {
    let blob = format!("data:image/png;base64,{}", "A".repeat(50_000));
    let input = json!([{"type": "message", "role": "user",
        "content": [{"type": "input_image", "image_url": blob}]}]);
    let out = build_transcript("sys", input, 100_000);
    assert!(out.contains("[inline data omitted"), "{out}");
    assert!(
        out.len() < 1_000,
        "the blob is gone, got {} chars",
        out.len()
    );
}

#[test]
fn an_over_budget_transcript_shortens_tool_results_first() {
    let huge = "x".repeat(200_000);
    let input = json!([
        user_message("the original task"),
        tool_result("c1", &huge),
        user_message("the latest question"),
    ]);
    let out = build_transcript("sys", input, 60_000);
    assert!(out.len() <= 60_000, "fits the budget, got {}", out.len());
    // Every message survives; only the tool result is shortened.
    assert!(out.contains("the original task"), "{out}");
    assert!(out.contains("the latest question"), "{out}");
    assert!(out.contains("bytes omitted"), "{out}");
    assert!(
        !out.contains("earlier items were omitted"),
        "no item was dropped"
    );
}

#[test]
fn the_oldest_items_go_first_and_the_task_and_latest_stay() {
    let mut items = vec![user_message("the original task")];
    for i in 0..200 {
        items.push(tool_result(
            &format!("c{i}"),
            &format!("result number {i} {}", "y".repeat(900)),
        ));
    }
    items.push(user_message("the latest question"));
    let out = build_transcript("sys", Value::Array(items), 40_000);
    assert!(
        out.len() <= 40_000 + 200,
        "fits the budget, got {}",
        out.len()
    );
    assert!(out.contains("the original task"), "the task is kept");
    assert!(
        out.contains("the latest question"),
        "the latest item is kept"
    );
    assert!(out.contains("result number 199"), "recent results are kept");
    assert!(
        !out.contains("result number 0 "),
        "the oldest results are dropped"
    );
    assert!(
        out.contains("earlier items were omitted"),
        "a marker says so"
    );
}

#[test]
fn a_huge_user_message_is_capped_too() {
    let input = json!([user_message(&"z".repeat(500_000))]);
    let out = build_transcript("sys", input, 100_000);
    assert!(out.len() <= 100_000, "got {}", out.len());
}

#[test]
fn oversized_instructions_cannot_crowd_out_the_transcript() {
    let out = build_transcript(&"i".repeat(300_000), json!([user_message("task")]), 40_000);
    assert!(out.len() <= 40_000, "got {}", out.len());
    assert!(out.contains("task"));
}

#[test]
fn truncation_never_splits_a_multibyte_character() {
    let out = truncate_str(&"é".repeat(10), 3);
    assert!(
        out.starts_with('é') && out.contains("bytes omitted"),
        "{out}"
    );
}

fn incomplete_response(reason: &str, text: Option<&str>) -> ResponsesResponse {
    let mut v = json!({
        "id": "resp_a", "object": "response", "created_at": 1,
        "status": "incomplete", "incomplete_details": {"reason": reason},
        "model": "beam", "output": [],
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
fn the_request_limit_is_the_answer_budget_plus_the_reasoning_budget() {
    let c = AdvisorConfig {
        max_tokens: Some(4096),
        reasoning_max_tokens: Some(16384),
        ..cfg()
    };
    assert_eq!(output_token_limit(&c), Some(20480));
    let v = serde_json::to_value(advisor_request(&c, "t")).unwrap();
    assert_eq!(v["max_output_tokens"], 20480);
}

#[test]
fn either_budget_at_no_limit_omits_the_request_limit() {
    for (answer, reasoning) in [
        (None, Some(100)),
        (Some(100), None),
        (Some(0), Some(100)),
        (Some(100), Some(0)),
    ] {
        let c = AdvisorConfig {
            max_tokens: answer,
            reasoning_max_tokens: reasoning,
            ..cfg()
        };
        assert_eq!(output_token_limit(&c), None, "{answer:?} + {reasoning:?}");
    }
}

#[test]
fn an_answer_cut_at_the_output_limit_is_kept_and_marked() {
    let (text, usage) = advice_from_response(&incomplete_response(
        "max_output_tokens",
        Some("Use a channel, and"),
    ))
    .unwrap();
    assert!(text.starts_with("Use a channel, and"), "{text}");
    assert!(text.contains("cut off at the output limit"), "{text}");
    assert!(usage.is_some(), "the spend is still billed");
}

#[test]
fn reasoning_that_used_the_whole_budget_is_reported_as_such() {
    let err = advice_from_response(&incomplete_response("max_output_tokens", None)).unwrap_err();
    assert!(err.contains("reasoning used the whole budget"), "{err}");
}

#[test]
fn an_incomplete_response_stopped_for_another_reason_is_still_a_failure() {
    let err =
        advice_from_response(&incomplete_response("content_filter", Some("partial"))).unwrap_err();
    assert!(err.contains("status is incomplete"), "{err}");
}

#[test]
fn a_long_answer_is_held_to_the_answer_budget() {
    let c = AdvisorConfig {
        max_tokens: Some(10),
        ..cfg()
    };
    let out = cap_answer("word ".repeat(100), &c);
    assert!(out.chars().count() <= 10 * CHARS_PER_TOKEN + 4, "{out}");
    assert!(out.ends_with("[…]"), "{out}");
    // Within budget: untouched. No limit: untouched.
    assert_eq!(cap_answer("short".into(), &c), "short");
    let unlimited = AdvisorConfig {
        max_tokens: None,
        ..cfg()
    };
    assert_eq!(cap_answer("word ".repeat(100), &unlimited).len(), 500);
}

#[test]
fn the_reviewer_is_told_it_is_not_the_assistant_and_has_no_tools() {
    let v = serde_json::to_value(advisor_request(&cfg(), "t")).unwrap();
    let sys = v["instructions"].as_str().unwrap();
    assert!(sys.contains("NOT that assistant"), "{sys}");
    assert!(sys.contains("never call a function or tool"), "{sys}");
    assert!(
        sys.contains("`advisor`"),
        "it names the tool it must not call: {sys}"
    );
    assert!(sys.contains("plain text only"), "{sys}");
    assert!(v.get("tools").is_none(), "and it really is sent none");
}

#[test]
fn the_executors_instructions_are_labelled_as_not_the_reviewers() {
    let out = build_transcript("BE A CODER", json!([user_message("task")]), 100_000);
    let label = out.find("not for you").expect("the label");
    let body = out.find("BE A CODER").expect("the instructions");
    assert!(label < body, "the label comes first: {out}");
    assert!(out.contains("<assistant_instructions>"), "{out}");
    assert!(
        !out.contains("<system>"),
        "no look-alike system block: {out}"
    );
}

#[test]
fn the_request_ends_by_telling_the_reviewer_to_write_advice_not_call_a_tool() {
    let out = build_transcript("sys", json!([user_message("task")]), 100_000);
    let transcript_end = out.find("</transcript>").unwrap();
    let tail = &out[transcript_end..];
    assert!(tail.contains("do not call any tool"), "{tail}");
    assert!(tail.contains("advice"), "{tail}");
}

#[test]
fn the_advisor_tool_section_is_hidden_from_the_reviewer() {
    let executor = "You are a coder.\n\n# Advisor Tool\n\nCall advisor BEFORE substantive work.\n\n\
                    <system-reminder>keep me</system-reminder>";
    let seen = reviewer_view_of(Some(executor));
    assert!(!seen.contains("Call advisor BEFORE"), "{seen}");
    assert!(!seen.contains("# Advisor Tool"), "{seen}");
    assert!(
        seen.contains("You are a coder.") && seen.contains("keep me"),
        "{seen}"
    );
    assert_eq!(reviewer_view_of(None), "");
}

#[test]
fn a_tool_call_instead_of_advice_is_named_in_the_error() {
    let resp: ResponsesResponse = serde_json::from_value(json!({
        "id": "resp_a", "object": "response", "created_at": 1,
        "status": "completed", "model": "beam",
        "output": [{"type": "function_call", "call_id": "c1", "name": "advisor",
                    "arguments": "{}", "status": "completed"}]
    }))
    .unwrap();
    let err = advice_from_response(&resp).unwrap_err();
    assert!(err.contains("called a tool"), "{err}");
}

#[test]
fn http_failures_are_sorted_into_kinds() {
    let kind = FailureKind::from_http;
    assert_eq!(kind(429, "slow down"), FailureKind::TooManyRequests);
    assert_eq!(kind(504, ""), FailureKind::Timeout);
    assert_eq!(kind(502, "bad gateway"), FailureKind::Overloaded);
    assert_eq!(kind(503, "overloaded"), FailureKind::Overloaded);
    assert_eq!(kind(529, ""), FailureKind::Overloaded);
    assert_eq!(kind(404, "model not found"), FailureKind::Rejected);
    assert_eq!(kind(422, "bad parameter"), FailureKind::Rejected);
    // The real upstream message from the proxy log.
    assert_eq!(
        kind(
            400,
            "The prompt is too long: 1424454, model maximum context length: 1048576"
        ),
        FailureKind::PromptTooLong
    );
    assert_eq!(
        kind(400, "Input exceeds the CONTEXT WINDOW"),
        FailureKind::PromptTooLong
    );
    assert_eq!(kind(413, "request too long"), FailureKind::PromptTooLong);
}

#[test]
fn a_timeout_or_connection_error_from_the_shared_client_is_sorted() {
    let upstream = |status: u16| AppError::Upstream {
        status: axum::http::StatusCode::from_u16(status).unwrap(),
        body: "x".into(),
    };
    // `From<reqwest::Error>`: a timeout is 504, a connection failure 502.
    assert_eq!(
        FailureKind::from_app_error(&upstream(504)),
        FailureKind::Timeout
    );
    assert_eq!(
        FailureKind::from_app_error(&upstream(502)),
        FailureKind::Overloaded
    );
    assert_eq!(
        FailureKind::from_app_error(&AppError::Internal("x".into())),
        FailureKind::Overloaded
    );
}

const ALL_KINDS: [FailureKind; 6] = [
    FailureKind::PromptTooLong,
    FailureKind::TooManyRequests,
    FailureKind::Overloaded,
    FailureKind::Timeout,
    FailureKind::NoAdvice,
    FailureKind::Rejected,
];

#[test]
fn every_kind_maps_to_an_error_code_claude_code_knows() {
    for kind in ALL_KINDS {
        let code = kind.client_code();
        assert!(
            protocol::ADVISOR_ERROR_CODES.contains(&code),
            "{kind:?} -> {code}"
        );
        assert_ne!(code, protocol::ERROR_CODE_MAX_USES_EXCEEDED, "{kind:?}");
    }
    assert_eq!(FailureKind::PromptTooLong.client_code(), "prompt_too_long");
    assert_eq!(
        FailureKind::TooManyRequests.client_code(),
        "too_many_requests"
    );
    assert_eq!(FailureKind::Overloaded.client_code(), "overloaded");
    assert_eq!(
        FailureKind::Timeout.client_code(),
        "execution_time_exceeded"
    );
    assert_eq!(FailureKind::NoAdvice.client_code(), "unavailable");
    assert_eq!(FailureKind::Rejected.client_code(), "unavailable");
}

#[test]
fn what_the_executor_is_told_never_reads_like_advice() {
    let mut seen = std::collections::HashSet::new();
    for kind in ALL_KINDS {
        let text = kind.executor_text();
        assert!(
            text.starts_with("The advisor could not be consulted"),
            "{text}"
        );
        assert!(text.ends_with("without a second opinion."), "{text}");
        assert!(seen.insert(text), "each kind has its own reason: {text}");
    }
}

#[test]
fn only_failures_that_could_change_are_retried() {
    assert!(!FailureKind::PromptTooLong.retryable());
    assert!(!FailureKind::Rejected.retryable());
    for kind in [
        FailureKind::TooManyRequests,
        FailureKind::Overloaded,
        FailureKind::Timeout,
        FailureKind::NoAdvice,
    ] {
        assert!(kind.retryable(), "{kind:?}");
    }
}

// ── Prompt caching ───────────────────────────────────────────────────────

/// A `Config` with the given prompt-caching setting, built from the
/// resolver (the struct has too many fields to spell out here).
fn config_with_caching(models: &[&str], key: Option<&str>) -> crate::config::Config {
    let mut config = crate::config::Config::resolve(
        None,
        &crate::config::EnvInputs {
            upstream_base_url: Some("https://example.com".into()),
            upstream_api_key: Some("k".into()),
            ..Default::default()
        },
    )
    .unwrap();
    config.prompt_caching.models = models.iter().map(|m| (*m).to_owned()).collect();
    config.prompt_caching.cache_key = key.map(str::to_owned);
    config
}

#[test]
fn no_cache_key_is_sent_unless_caching_is_enabled_for_the_advisor_model() {
    let c = cfg();
    let prompt = "x<transcript>[]</transcript>";
    assert_eq!(
        advisor_cache_key(&config_with_caching(&[], None), &c, prompt),
        None
    );
    // Caching on for a DIFFERENT model only.
    assert_eq!(
        advisor_cache_key(&config_with_caching(&["other-model"], None), &c, prompt),
        None
    );
}

#[test]
fn a_configured_cache_key_is_used_as_given() {
    let c = cfg();
    let config = config_with_caching(&[c.model.as_str()], Some("my-key"));
    assert_eq!(
        advisor_cache_key(&config, &c, "anything").as_deref(),
        Some("my-key")
    );
}

#[test]
fn the_derived_cache_key_is_stable_across_one_session_and_differs_between_sessions() {
    let c = cfg();
    let config = config_with_caching(&[c.model.as_str()], None);
    let a = advisor_cache_key(&config, &c, "{\"text\":\"fix the bug\"}").unwrap();
    assert_eq!(
        a,
        advisor_cache_key(&config, &c, "{\"text\":\"fix the bug\"}").unwrap()
    );
    assert!(a.starts_with("advisor-"), "{a}");
    assert_ne!(
        a,
        advisor_cache_key(&config, &c, "{\"text\":\"write a poem\"}").unwrap()
    );
}

#[test]
fn the_session_fingerprint_is_the_first_item_only() {
    let task = |text: &str| InputItem::Message {
        role: "user".to_owned(),
        content: vec![InputContentPart::InputText {
            text: text.to_owned(),
            prompt_cache_breakpoint: None,
        }],
    };
    let follow_up = || InputItem::Message {
        role: "user".to_owned(),
        content: vec![InputContentPart::InputText {
            text: "and then".to_owned(),
            prompt_cache_breakpoint: None,
        }],
    };
    let short = Input::Items(vec![task("fix the bug")]);
    let longer = Input::Items(vec![task("fix the bug"), follow_up(), follow_up()]);
    let other = Input::Items(vec![task("write a poem")]);
    assert_eq!(session_fingerprint(&short), session_fingerprint(&longer));
    assert_ne!(session_fingerprint(&short), session_fingerprint(&other));
    assert_eq!(session_fingerprint(&Input::Items(Vec::new())), "");
}

#[test]
fn the_cut_point_stays_put_across_consecutive_consultations() {
    // The transcript grows by one item per step. Without the block-wise
    // drop, the "N earlier items were omitted" marker (and so the start of
    // the prompt) would change every step and no cache could ever hit.
    let build = |n: usize| {
        let mut items = vec![user_message("the original task")];
        for i in 0..n {
            items.push(tool_result(
                &format!("c{i}"),
                &format!("{i} {}", "y".repeat(900)),
            ));
        }
        build_transcript("sys", Value::Array(items), 40_000)
    };
    let marker = |out: &str| {
        let end = out.find(" earlier items were omitted").expect("trimmed");
        let start = out[..end].rfind('"').unwrap() + 1;
        out[start..end].to_owned()
    };
    let outputs: Vec<String> = (200..216).map(build).collect();
    let mut markers: Vec<String> = outputs.iter().map(|o| marker(o)).collect();
    markers.dedup();
    assert!(
        markers.len() <= 2,
        "the cut moved on {} of 16 steps: {markers:?}",
        markers.len() - 1
    );
    // Within one marker value, the prompts share everything but the new
    // item and the closing text.
    for pair in outputs.windows(2) {
        if marker(&pair[0]) == marker(&pair[1]) {
            let common = pair[0]
                .bytes()
                .zip(pair[1].bytes())
                .take_while(|(a, b)| a == b)
                .count();
            assert!(
                common + 600 >= pair[0].len(),
                "only {common} of {} bytes are shared",
                pair[0].len()
            );
        }
    }
}

#[test]
fn dropping_in_blocks_still_fits_the_budget_and_keeps_task_and_latest() {
    let mut items = vec![user_message("the original task")];
    for i in 0..60 {
        items.push(tool_result(
            &format!("c{i}"),
            &format!("{i} {}", "y".repeat(900)),
        ));
    }
    items.push(user_message("the latest question"));
    let out = build_transcript("sys", Value::Array(items), 20_000);
    assert!(out.len() <= 20_000 + 200, "got {}", out.len());
    assert!(out.contains("the original task") && out.contains("the latest question"));
}
