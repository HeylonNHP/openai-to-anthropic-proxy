use super::*;
use serde_json::json;

fn cfg() -> AdvisorConfig {
    AdvisorConfig {
        enabled: true,
        model: "beam".into(),
        effort: Some("medium".into()),
        max_uses: Some(2),
        max_tokens: Some(64),
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
    assert_eq!(v["max_output_tokens"], 64);
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
    assert_eq!(v["max_output_tokens"], 64);
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
fn strip_advisor_call_removes_only_the_internal_call() {
    let mut resp = response("completed", Some("Terminal answer."));
    let mut extra: Vec<OutputItem> = serde_json::from_value(json!([
        {"type": "function_call", "call_id": "call_adv", "name": "advisor", "arguments": "{}"},
        {"type": "function_call", "call_id": "call_bash", "name": "Bash", "arguments": "{}"}
    ]))
    .unwrap();
    resp.output.append(&mut extra);
    strip_advisor_call(&mut resp);
    assert_eq!(resp.output.len(), 2);
    assert!(matches!(&resp.output[0], OutputItem::Message { .. }));
    assert!(matches!(
        &resp.output[1],
        OutputItem::FunctionCall { name, .. } if name == "Bash"
    ));
    let mut again = resp.clone();
    strip_advisor_call(&mut again);
    assert_eq!(
        format!("{again:?}"),
        format!("{resp:?}"),
        "pure helper is idempotent"
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
