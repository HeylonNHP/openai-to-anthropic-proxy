//! The advisor server tool: disabled no-op, loop, cache/reasoning,
//! failure modes, limits, wire shape, instruction sections, the
//! B1-B12 regression record, and advisor failure retries.

use crate::harness::*;

/// A completed Responses body with one `output_text` message.
fn advisor_executor_message(id: &str, text: &str) -> FakeResponse {
    FakeResponse {
        status: StatusCode::OK,
        body: Some(completed_response_with_id(id, text)),
    }
}

/// 1. The MOST important invariant: with `enabled: false` the proxy makes
/// exactly one upstream POST and answers exactly as it did before phase 3,
/// even when the inbound request declares the advisor tool. A second case
/// (enabled config but NO declaration) proves detection-triggering: an
/// `enabled: true` deployment still leaves every non-advisor request alone.
#[tokio::test]
async fn advisor_disabled_is_byte_for_byte_identical() {
    const BASH_DECL: &str =
        r#"{"name":"Bash","description":"run","input_schema":{"type":"object","properties":{}}}"#;
    for (label, enabled, declaration) in [
        ("disabled", false, ADVISOR_DECLARATION),
        ("enabled_no_declaration", true, BASH_DECL),
    ] {
        let (upstream_addr, upstream) = start_fake_upstream().await;
        *upstream.canned.lock().await =
            Some(completed_response_with_id("resp_same", "same answer"));
        let mut config = (*make_proxy_config(upstream_addr)).clone();
        config.advisor.enabled = enabled;
        let proxy_addr = start_proxy(Arc::new(config)).await;

        let res = reqwest::Client::new()
            .post(format!("http://{proxy_addr}/v1/messages"))
            .header("content-type", "application/json")
            .body(ReqBody::from(advisor_request_with_decl(false, declaration)))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{label}");
        let body: serde_json::Value = res.json().await.unwrap();
        assert_eq!(
            body,
            serde_json::json!({
                "id": "msg_resp_same",
                "type": "message",
                "role": "assistant",
                "content": [{"type": "text", "text": "same answer"}],
                "model": "gpt-4o-mini",
                "stop_reason": "end_turn",
                "stop_sequence": null,
                "usage": {
                    "input_tokens": 7,
                    "output_tokens": 3
                }
            }),
            "{label}: the client response must be byte-identical to the pre-advisor path"
        );

        let sent = upstream.received_all.lock().await.clone();
        assert_eq!(sent.len(), 1, "{label}: no extra POSTs");
        let received: serde_json::Value = serde_json::from_slice(&sent[0]).unwrap();
        assert_eq!(received["stream"], false);
        // The ordinary translator may legitimately forward the client's
        // declared tools; what must NEVER appear is a synthesized advisor
        // function tool (that only happens on the active advisor path).
        let upstream_tools = received["tools"].as_array().cloned().unwrap_or_default();
        assert!(
            !upstream_tools.iter().any(|t| t["name"] == "advisor"),
            "{label}: no synthesized advisor tool"
        );
        // B9: with the advisor pipeline INACTIVE (both cases here — the
        // shipped default `enabled:false`, or `enabled:true` with no real
        // declaration) the orphan `# Advisor Tool` section must NOT reach
        // the executor. Before the fix, translate.rs's name-based check
        // preserved it for the disabled + declaration case, telling the
        // model to call a tool that was dropped on translation.
        assert!(
            !received["instructions"]
                .as_str()
                .unwrap()
                .contains("Call advisor BEFORE substantive work"),
            "{label}: inactive advisor must strip the # Advisor Tool instructions"
        );
    }
}

/// An ordinary streaming request with `enabled: false` is also unchanged:
/// the upstream is still asked to stream (no forced buffering) and the
/// client still gets the real translated SSE, not the advisor's synthetic
/// sequence.
#[tokio::test]
async fn advisor_disabled_leaves_streaming_unchanged() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    let sse = [
        r#"event: response.created
data: {"type":"response.created","response":{"id":"resp_1","object":"response","created_at":0,"status":"in_progress","model":"gpt-4o","output":[]}}"#,
        r#"event: response.output_text.delta
data: {"type":"response.output_text.delta","item_id":"msg_x","output_index":0,"content_index":0,"delta":"Hi"}"#,
        r#"event: response.completed
data: {"type":"response.completed","response":{"id":"resp_1","object":"response","created_at":0,"status":"completed","model":"gpt-4o","output":[],"usage":{"input_tokens":4,"output_tokens":2,"total_tokens":6}}}"#,
    ]
    .join("\n\n")
        + "\n\n";
    *upstream.canned_stream.lock().await = Some(sse);
    let config = make_proxy_config(upstream_addr);
    assert!(!config.advisor.enabled);
    let proxy_addr = start_proxy(config).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(true)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers()
            .get(axum::http::header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap(),
        "text/event-stream"
    );
    let text = res.text().await.unwrap();
    let kinds: Vec<&str> = text
        .split("\n\n")
        .filter(|s| !s.trim().is_empty())
        .filter_map(|e| e.lines().find_map(|l| l.strip_prefix("event: ")))
        .collect();
    assert_eq!(kinds.iter().filter(|k| **k == "message_start").count(), 1);
    assert_eq!(kinds.iter().filter(|k| **k == "message_stop").count(), 1);

    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(sent.len(), 1, "no extra POST");
    let v: serde_json::Value = serde_json::from_slice(&sent[0]).unwrap();
    assert_eq!(v["stream"], true, "upstream streaming was NOT forced off");
    assert!(
        !v["tools"]
            .as_array()
            .unwrap_or(&Vec::new())
            .iter()
            .any(|t| t["name"] == "advisor"),
        "no synthesized advisor tool"
    );
}

/// The advisor request carries a `prompt_cache_key` only when the operator
/// turned prompt caching on for the advisor's model, and the key is the same
/// for every consultation of one conversation.
async fn advisor_cache_keys(models: &[&str], cache_key: Option<&str>) -> Vec<serde_json::Value> {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            advisor_call_response("resp_c1", "call_t1"),
            advisor_answer("first advice"),
            advisor_call_response("resp_c2", "call_t2"),
            advisor_answer("second advice"),
            advisor_executor_message("resp_final", "done"),
        ],
    );
    let mut config = (*advisor_config(upstream_addr)).clone();
    config.prompt_caching.models = models.iter().map(|m| (*m).to_owned()).collect();
    config.prompt_caching.cache_key = cache_key.map(str::to_owned);
    let proxy_addr = start_proxy(Arc::new(config)).await;
    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    upstream
        .received_all
        .lock()
        .await
        .iter()
        .map(|b| serde_json::from_slice::<serde_json::Value>(b).unwrap())
        .filter(|b| b["model"] == "beam")
        .map(|b| b["prompt_cache_key"].clone())
        .collect()
}

#[tokio::test]
async fn advisor_sends_no_cache_key_unless_caching_is_on_for_its_model() {
    let keys = advisor_cache_keys(&[], None).await;
    assert_eq!(keys.len(), 2, "two consultations");
    assert!(keys.iter().all(serde_json::Value::is_null), "{keys:?}");
    // Caching on for another model only: still none.
    let keys = advisor_cache_keys(&["gpt-4o-mini"], None).await;
    assert!(keys.iter().all(serde_json::Value::is_null), "{keys:?}");
}

#[tokio::test]
async fn advisor_uses_one_stable_cache_key_per_conversation() {
    let keys = advisor_cache_keys(&["beam"], None).await;
    assert_eq!(keys.len(), 2);
    let first = keys[0].as_str().expect("a derived key");
    assert!(first.starts_with("advisor-"), "{first}");
    assert_eq!(keys[0], keys[1], "both consultations share one key");
}

#[tokio::test]
async fn advisor_uses_the_configured_cache_key_as_given() {
    let keys = advisor_cache_keys(&["beam"], Some("my-key")).await;
    assert!(keys.iter().all(|k| k == "my-key"), "{keys:?}");
}

/// The `reasoning` object of each advisor request, for an advisor with the
/// given effort and the proxy's `reasoning.summary` setting.
async fn advisor_reasoning(
    effort: Option<&str>,
    summary: openai_to_anthropic_proxy::config::ReasoningSummary,
) -> Vec<serde_json::Value> {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            advisor_call_response("resp_c1", "call_t1"),
            advisor_answer("advice"),
            advisor_executor_message("resp_final", "done"),
        ],
    );
    let mut config = (*advisor_config(upstream_addr)).clone();
    config.advisor.effort = effort.map(str::to_owned);
    config.reasoning.summary = summary;
    let proxy_addr = start_proxy(Arc::new(config)).await;
    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    upstream
        .received_all
        .lock()
        .await
        .iter()
        .map(|b| serde_json::from_slice::<serde_json::Value>(b).unwrap())
        .filter(|b| b["model"] == "beam")
        .map(|b| b["reasoning"].clone())
        .collect()
}

#[tokio::test]
async fn advisor_follows_the_reasoning_summary_setting() {
    use openai_to_anthropic_proxy::config::ReasoningSummary;

    // Reasoning on → the advisor asks for a summary, like the main request.
    let on = advisor_reasoning(Some("low"), ReasoningSummary::Auto).await;
    assert_eq!(on.len(), 1);
    assert_eq!(on[0]["effort"], "low");
    assert_eq!(on[0]["summary"], "auto");

    // Switched off → effort still sent, no summary.
    let off = advisor_reasoning(Some("low"), ReasoningSummary::Off).await;
    assert_eq!(off[0]["effort"], "low");
    assert!(off[0].get("summary").is_none(), "{}", off[0]);

    // Effort "none" → no summary even when the setting is on.
    let none = advisor_reasoning(Some("none"), ReasoningSummary::Auto).await;
    assert!(none[0].get("summary").is_none(), "{}", none[0]);

    // No advisor effort → no `reasoning` object at all, so nothing to add to.
    let unset = advisor_reasoning(None, ReasoningSummary::Auto).await;
    assert!(unset[0].is_null(), "{}", unset[0]);
}

/// 2. Happy path: 3 POSTs (executor call → advisor text → final executor
/// text). The third body must carry the canonical `function_call_output`
/// advice, and the client sees one contiguous, id-matched block pair plus
/// `usage.iterations[]`.
#[tokio::test]
async fn advisor_happy_path() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            advisor_call_response("resp_call", "call_adv_1"),
            advisor_answer("Use a channel-based coordination pattern."),
            advisor_executor_message("resp_final", "Here is the implementation."),
        ],
    );
    let proxy_addr = start_proxy(advisor_config(upstream_addr)).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = res.json().await.unwrap();

    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(sent.len(), 3, "executor, advisor, executor");
    let bodies: Vec<serde_json::Value> = sent
        .iter()
        .map(|b| serde_json::from_slice(b).unwrap())
        .collect();

    // The advisor's own POST is separate, tool-free and explicitly modelled.
    assert_eq!(bodies[1]["model"], "beam");
    assert_eq!(bodies[1]["stream"], false);
    assert!(bodies[1]["tools"].is_null());
    // answer budget 64 + reasoning budget 100
    assert_eq!(bodies[1]["max_output_tokens"], 164);
    assert!(
        bodies[1]["input"][0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("transcript")
    );

    // The 3rd (final executor) request carries BOTH canonical input items.
    let items = bodies[2]["input"].as_array().unwrap();
    let call = items
        .iter()
        .find(|i| i["type"] == "function_call")
        .expect("function_call echo");
    assert_eq!(call["name"], "advisor");
    assert_eq!(call["call_id"], "call_adv_1");
    let output = items
        .iter()
        .find(|i| i["type"] == "function_call_output")
        .expect("function_call_output advice");
    assert_eq!(output["call_id"], "call_adv_1");
    assert_eq!(
        output["output"], "Use a channel-based coordination pattern.",
        "the advice must reach the executor's transcript"
    );
    let call_pos = items
        .iter()
        .position(|i| i["type"] == "function_call")
        .unwrap();
    let out_pos = items
        .iter()
        .position(|i| i["type"] == "function_call_output")
        .unwrap();
    assert!(call_pos < out_pos, "no orphan function_call");

    // One assistant message, contiguous pair, matching ids, end_turn.
    let content = body["content"].as_array().unwrap();
    assert_eq!(content.len(), 3, "pair + final executor text");
    assert_eq!(content[0]["type"], "server_tool_use");
    assert_eq!(content[0]["name"], "advisor");
    assert_eq!(content[1]["type"], "advisor_tool_result");
    assert_eq!(content[0]["id"], content[1]["tool_use_id"]);
    assert!(content[0]["id"].as_str().unwrap().starts_with("srvtoolu_"));
    assert_eq!(content[1]["content"]["type"], "advisor_result");
    assert_eq!(
        content[1]["content"]["text"],
        "Use a channel-based coordination pattern."
    );
    assert_eq!(content[2]["text"], "Here is the implementation.");
    assert_eq!(body["stop_reason"], "end_turn");
    assert!(
        !content
            .iter()
            .any(|b| b["type"] == "tool_use" && b["name"] == "advisor"),
        "no client tool_use named advisor"
    );

    // iterations[]: message, advisor_message, message — in order, with the
    // advisor's own tokens NOT folded into the top-level totals.
    let iterations = body["usage"]["iterations"].as_array().unwrap();
    assert_eq!(iterations.len(), 3);
    assert_eq!(iterations[0]["type"], "message");
    assert_eq!(iterations[1]["type"], "advisor_message");
    assert_eq!(iterations[1]["model"], "beam");
    assert_eq!(iterations[1]["input_tokens"], 21);
    assert_eq!(iterations[1]["output_tokens"], 13);
    assert_eq!(iterations[2]["type"], "message");
    // Top-level usage is the sum of the EXECUTOR iterations (as in
    // Anthropic's own example: 412 + 1348 = 1760), and never includes the
    // advisor's tokens.
    let executor_sum = |field: &str| {
        iterations
            .iter()
            .filter(|i| i["type"] == "message")
            .map(|i| i[field].as_u64().unwrap())
            .sum::<u64>()
    };
    assert_eq!(
        body["usage"]["input_tokens"],
        executor_sum("input_tokens"),
        "every executor run, not only the last"
    );
    assert_eq!(
        body["usage"]["output_tokens"],
        executor_sum("output_tokens")
    );
    assert_eq!(body["usage"]["input_tokens"], 16);
}

/// The executor's own words and reasoning from a round reach its re-run: the
/// re-run's input continues with what it said (reasoning folded in, under
/// the default `text` replay mode), then the call, then the call's output.
#[tokio::test]
async fn the_rerun_input_replays_what_the_executor_said_in_the_round() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    let first = FakeResponse {
        status: StatusCode::OK,
        body: Some(
            r#"{
                "id": "resp_call",
                "object": "response",
                "created_at": 1,
                "status": "completed",
                "model": "gpt-4o-mini",
                "output": [
                    {"type": "reasoning", "id": "rs_1",
                     "summary": [{"type": "summary_text", "text": "A second opinion would help."}],
                     "encrypted_content": "ENC"},
                    {"type": "message", "id": "msg_1", "role": "assistant",
                     "content": [{"type": "output_text", "text": "Let me check my plan.", "annotations": []}]},
                    {"type": "function_call", "call_id": "call_adv_1", "name": "advisor", "arguments": "{}"}
                ],
                "usage": {"input_tokens":9,"output_tokens":2,"total_tokens":11}
            }"#
            .to_owned(),
        ),
    };
    script(
        &upstream,
        vec![
            first,
            advisor_answer("Use a channel-based coordination pattern."),
            advisor_executor_message("resp_final", "Here is the implementation."),
        ],
    );
    let proxy_addr = start_proxy(advisor_config(upstream_addr)).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(sent.len(), 3, "executor, advisor, executor");
    let first_body: serde_json::Value = serde_json::from_slice(&sent[0]).unwrap();
    let rerun: serde_json::Value = serde_json::from_slice(&sent[2]).unwrap();
    let before = first_body["input"].as_array().unwrap().len();
    let items = rerun["input"].as_array().unwrap();
    let added = &items[before..];

    assert_eq!(added.len(), 3, "message, call, output: {added:#?}");
    assert_eq!(added[0]["type"], "message");
    assert_eq!(added[0]["role"], "assistant");
    assert_eq!(
        added[0]["content"][0]["text"],
        "<thinking>\nA second opinion would help.\n</thinking>\n\nLet me check my plan."
    );
    assert_eq!(added[1]["type"], "function_call");
    assert_eq!(added[1]["call_id"], "call_adv_1");
    assert_eq!(added[2]["type"], "function_call_output");
    assert_eq!(added[2]["call_id"], "call_adv_1");
    assert_eq!(
        items
            .iter()
            .filter(|i| i["type"] == "function_call")
            .count(),
        1,
        "the call is replayed exactly once"
    );
}

/// The advisor sees the round it was called from: the executor's words
/// that led to the consultation and the triggering call itself.
#[tokio::test]
async fn the_advisor_transcript_includes_the_round_that_called_it() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    let first = FakeResponse {
        status: StatusCode::OK,
        body: Some(
            r#"{
                "id": "resp_call",
                "object": "response",
                "created_at": 1,
                "status": "completed",
                "model": "gpt-4o-mini",
                "output": [
                    {"type": "message", "id": "msg_1", "role": "assistant",
                     "content": [{"type": "output_text", "text": "My plan: use a mutex per shard.", "annotations": []}]},
                    {"type": "function_call", "call_id": "call_adv_1", "name": "advisor", "arguments": "{}"}
                ],
                "usage": {"input_tokens":9,"output_tokens":2,"total_tokens":11}
            }"#
            .to_owned(),
        ),
    };
    script(
        &upstream,
        vec![
            first,
            advisor_answer("Use a channel-based coordination pattern."),
            advisor_executor_message("resp_final", "Here is the implementation."),
        ],
    );
    let proxy_addr = start_proxy(advisor_config(upstream_addr)).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(sent.len(), 3, "executor, advisor, executor");
    let advisor_body: serde_json::Value = serde_json::from_slice(&sent[1]).unwrap();
    assert_eq!(advisor_body["model"], "beam");
    let transcript = advisor_body["input"][0]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(
        transcript.contains("My plan: use a mutex per shard."),
        "the executor's words from the round: {transcript}"
    );
    assert!(
        transcript.contains("call_adv_1"),
        "the triggering call: {transcript}"
    );
    assert!(
        !transcript.contains("Use a channel-based"),
        "no advice before it is given"
    );
}

/// A call dropped because an earlier call in the same response exhausted its
/// tool is already in the executor's input, so it is answered with a "not
/// run" output instead of being left dangling.
#[tokio::test]
async fn a_call_dropped_after_exhaustion_mid_response_is_still_answered() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    let calls: Vec<String> = (1..=4)
        .map(|n| {
            format!(
                r#"{{"type": "function_call", "call_id": "call_adv_{n}", "name": "advisor", "arguments": "{{}}"}}"#
            )
        })
        .collect();
    let first = FakeResponse {
        status: StatusCode::OK,
        body: Some(format!(
            r#"{{
                "id": "resp_call",
                "object": "response",
                "created_at": 1,
                "status": "completed",
                "model": "gpt-4o-mini",
                "output": [{}],
                "usage": {{"input_tokens":9,"output_tokens":2,"total_tokens":11}}
            }}"#,
            calls.join(",")
        )),
    };
    // `max_uses: 2`: calls 1 and 2 consult, call 3 is refused (exhausting
    // the tool), call 4 is dropped.
    script(
        &upstream,
        vec![
            first,
            advisor_answer("Advice one."),
            advisor_answer("Advice two."),
            advisor_executor_message("resp_final", "Done."),
        ],
    );
    let proxy_addr = start_proxy(advisor_config(upstream_addr)).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(
        body["content"].as_array().unwrap().last().unwrap()["text"],
        "Done."
    );

    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(sent.len(), 4, "executor, advisor x2, executor");
    let rerun: serde_json::Value = serde_json::from_slice(&sent[3]).unwrap();
    let items = rerun["input"].as_array().unwrap();
    for n in 1..=4 {
        let id = format!("call_adv_{n}");
        let calls = items
            .iter()
            .filter(|i| i["type"] == "function_call" && i["call_id"] == id)
            .count();
        let outputs: Vec<&serde_json::Value> = items
            .iter()
            .filter(|i| i["type"] == "function_call_output" && i["call_id"] == id)
            .collect();
        assert_eq!(calls, 1, "{id} replayed once");
        assert_eq!(outputs.len(), 1, "{id} answered exactly once");
        if n == 4 {
            assert!(
                outputs[0]["output"]
                    .as_str()
                    .unwrap()
                    .starts_with("This call was not run"),
                "{:?}",
                outputs[0]
            );
        }
    }
}

/// 3. Fail closed. A 404 from the advisor (a model rejection shape!) must
/// NOT trigger `default_model`, must NOT fail the client, and must yield
/// `error_code: "unavailable"` with the executor's final answer intact.
#[tokio::test]
async fn advisor_failure_degrades_to_unavailable() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            advisor_call_response("resp_call", "call_adv_fail"),
            FakeResponse {
                status: StatusCode::NOT_FOUND,
                body: Some(REAL_GATEWAY_MODEL_404.into()),
            },
            advisor_executor_message("resp_final", "Answer without advice."),
        ],
    );
    // A default_model IS configured: if the advisor error ever went through
    // Attempt::send_success, this would produce a 4th POST.
    let mut config = (*advisor_config(upstream_addr)).clone();
    config.model_aliases = ModelAliases {
        map: BTreeMap::new(),
        default_model: Some("gpt-4o-mini".into()),
    };
    let proxy_addr = start_proxy(Arc::new(config)).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK, "fail closed, never an error");
    let body: serde_json::Value = res.json().await.unwrap();

    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(sent.len(), 3, "no fallback POST, no 4th request");
    let bodies: Vec<serde_json::Value> = sent
        .iter()
        .map(|b| serde_json::from_slice(b).unwrap())
        .collect();
    // The advisor 404 is a MODEL-REJECTION shape. If it had gone through the
    // executor's fallback path there would be an extra POST; the exact
    // 3-request sequence (and the unchanged executor model on the last
    // request) is the proof that no fallback POST ever happened.
    assert_eq!(
        bodies[1]["model"], "beam",
        "the 404 was the advisor's own POST"
    );
    assert_eq!(
        bodies[2]["model"], "gpt-4o-mini",
        "unchanged executor model"
    );
    assert!(
        !bodies[2]["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["type"] == "function_call" && i["name"] != "advisor"),
        "no re-translated fallback request"
    );
    // The executor still receives a (non-advice) function_call_output so the
    // upstream never sees an orphan function_call.
    let items = bodies[2]["input"].as_array().unwrap();
    assert_eq!(
        items
            .iter()
            .filter(|i| i["type"] == "function_call_output")
            .count(),
        1
    );
    let failure_output = items
        .iter()
        .find(|i| i["type"] == "function_call_output")
        .unwrap()["output"]
        .as_str()
        .unwrap();
    assert!(
        failure_output.contains("could not be consulted")
            && failure_output.contains("request to it was refused"),
        "the executor is told why, and not that a review happened: {failure_output}"
    );

    let content = body["content"].as_array().unwrap();
    assert_eq!(content[0]["type"], "server_tool_use");
    assert_eq!(content[1]["type"], "advisor_tool_result");
    assert_eq!(content[0]["id"], content[1]["tool_use_id"]);
    assert_eq!(content[1]["content"]["type"], "advisor_tool_result_error");
    assert_eq!(content[1]["content"]["error_code"], "unavailable");
    assert!(
        content
            .iter()
            .any(|b| b["text"] == "Answer without advice."),
        "the executor's final answer still arrives"
    );
    assert_eq!(body["stop_reason"], "end_turn");
    let iterations = body["usage"]["iterations"].as_array().unwrap();
    assert_eq!(
        iterations.len(),
        2,
        "two executor messages, no advisor billing"
    );
    assert!(iterations.iter().all(|i| i["type"] == "message"));
}

/// 3c. The advisor's per-request timeout really is enforced: the fake
/// upstream stalls ONLY the advisor POST (attempt 2) while the executor
/// calls stay fast, so a 1-second `timeout_secs` must cut it off without
/// failing the client or touching `default_model`.
#[tokio::test]
async fn advisor_timeout_degrades_gracefully() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            advisor_call_response("resp_call", "call_timeout"),
            advisor_answer("never arrives"),
            advisor_answer("never arrives"),
            advisor_executor_message("resp_final", "never arrives"),
        ],
    );
    // Attempt 1 = executor (fast); attempts 2 and 3 = the advisor and its
    // one retry (both stalled); attempt 4 = final executor (fast).
    *upstream.delay_per_attempt.lock().await = VecDeque::from([
        Duration::ZERO,
        Duration::from_secs(3),
        Duration::from_secs(3),
        Duration::ZERO,
    ]);
    let mut config = (*advisor_config(upstream_addr)).clone();
    config.advisor.timeout_secs = Some(1);
    let proxy_addr = start_proxy(Arc::new(config)).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK, "fail closed on a timeout");
    let body: serde_json::Value = res.json().await.unwrap();
    let result = body["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["type"] == "advisor_tool_result")
        .unwrap();
    assert_eq!(result["content"]["error_code"], "execution_time_exceeded");
    assert!(
        body["content"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["text"] == "never arrives"),
        "the executor's NEXT canned answer arrived; the timeout did not fail the turn"
    );
    // The fake upstream records each stalled advisor POST before the client
    // times out: the advisor and its ONE retry (2nd and 3rd), then the final
    // executor call (4th). No 5th request: a timeout never triggers a
    // default_model fallback, and the advisor is retried only once.
    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(sent.len(), 4, "one retry, no fallback POST after it");
    let bodies: Vec<serde_json::Value> = sent
        .iter()
        .map(|b| serde_json::from_slice(b).unwrap())
        .collect();
    assert_eq!(bodies[0]["model"], "gpt-4o-mini");
    assert_eq!(bodies[1]["model"], "beam", "the timed-out advisor POST");
    assert_eq!(bodies[2]["model"], "beam", "its retry");
    assert_eq!(bodies[3]["model"], "gpt-4o-mini");
}

/// 3b. Repeated advisor failures stay fail-closed, and at most ONE
/// operator-visible warning is emitted for the whole turn (the loop
/// consults twice, both times unsuccessfully).
/// A tracing writer that collects everything into a shared buffer.
#[derive(Clone, Default)]
struct LogCapture(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for LogCapture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogCapture {
    type Writer = Self;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Every failed consultation reaches the tracing log (the operator's record
/// of how often the advisor fails), including the failed attempt that was
/// retried, while the TUI still gets a single line (see below).
#[tokio::test]
async fn advisor_logs_every_failure_even_though_the_tui_shows_one() {
    // A global subscriber: tests run on separate threads, and a thread-local
    // one is defeated by tracing's callsite cache once another thread has hit
    // the same callsite with no subscriber. Other tests' warnings land in the
    // buffer too, so this test picks out its own lines by a unique marker in
    // the upstream error text.
    static CAPTURE: std::sync::OnceLock<LogCapture> = std::sync::OnceLock::new();
    let capture = CAPTURE
        .get_or_init(|| {
            let capture = LogCapture::default();
            tracing::subscriber::set_global_default(
                tracing_subscriber::fmt()
                    .with_writer(capture.clone())
                    .with_ansi(false)
                    .with_max_level(tracing::Level::WARN)
                    .finish(),
            )
            .expect("no other test installs a global subscriber");
            capture
        })
        .clone();
    const MARK: &str = "overloaded-7f3a91";

    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            advisor_call_response("resp_c1", "call_t1"),
            // A 503 is retried once, and the retry fails too.
            FakeResponse {
                status: StatusCode::SERVICE_UNAVAILABLE,
                body: Some(format!(r#"{{"error":{{"message":"{MARK}"}}}}"#)),
            },
            FakeResponse {
                status: StatusCode::SERVICE_UNAVAILABLE,
                body: Some(format!(r#"{{"error":{{"message":"{MARK}"}}}}"#)),
            },
            advisor_call_response("resp_c2", "call_t2"),
            FakeResponse {
                status: StatusCode::NOT_FOUND,
                body: Some(REAL_GATEWAY_MODEL_404.replace("claude-sonnet-5", MARK)),
            },
            advisor_executor_message("resp_final", "Answer without advice."),
        ],
    );
    let mut config = (*advisor_config(upstream_addr)).clone();
    config.advisor.max_uses = Some(2);
    let proxy_addr = start_proxy(Arc::new(config)).await;
    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let log = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    let failures: Vec<&str> = log
        .lines()
        .filter(|l| l.contains(MARK))
        .filter(|l| l.contains("advisor consultation failed; continuing without advice"))
        .collect();
    assert_eq!(
        failures.len(),
        2,
        "one log line per failed consultation:\n{log}"
    );
    assert!(
        failures[0].contains("failure=\"overloaded\""),
        "{}",
        failures[0]
    );
    assert!(
        failures[0].contains("failures_this_turn=1"),
        "{}",
        failures[0]
    );
    assert!(failures[0].contains("prompt_chars="), "{}", failures[0]);
    assert!(
        failures[1].contains("failure=\"unavailable\""),
        "{}",
        failures[1]
    );
    assert!(
        failures[1].contains("failures_this_turn=2"),
        "{}",
        failures[1]
    );
    assert_eq!(
        log.lines()
            .filter(|l| l.contains(MARK))
            .filter(|l| l.contains("attempt failed; retrying once"))
            .count(),
        1,
        "the failed attempt that was retried is logged:\n{log}"
    );
}

#[tokio::test]
async fn advisor_repeated_failure_emits_one_warning() {
    use openai_to_anthropic_proxy::SessionStatsStore;
    use openai_to_anthropic_proxy::tui::LogKind;

    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            advisor_call_response("resp_c1", "call_t1"),
            // A 503 is retried once, and the retry fails too.
            FakeResponse {
                status: StatusCode::SERVICE_UNAVAILABLE,
                body: Some(r#"{"error":{"message":"overloaded"}}"#.into()),
            },
            FakeResponse {
                status: StatusCode::SERVICE_UNAVAILABLE,
                body: Some(r#"{"error":{"message":"overloaded"}}"#.into()),
            },
            advisor_call_response("resp_c2", "call_t2"),
            // A 404 is permanent, so it is not retried.
            FakeResponse {
                status: StatusCode::NOT_FOUND,
                body: Some(REAL_GATEWAY_MODEL_404.into()),
            },
            advisor_executor_message("resp_final", "Answer without advice."),
        ],
    );
    let mut config = (*advisor_config(upstream_addr)).clone();
    config.advisor.max_uses = Some(2);
    let stats = Arc::new(SessionStatsStore::new());
    let (output, log_rx) = openai_to_anthropic_proxy::OutputSink::tui();
    let proxy_addr = start_proxy_with_captured_stores(Arc::new(config), stats, output).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = res.json().await.unwrap();
    let errors: Vec<&serde_json::Value> = body["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|b| b["type"] == "advisor_tool_result")
        .collect();
    assert_eq!(errors.len(), 2);
    // The 503 (retried, still failing) is "overloaded"; the 404 is a refused
    // request, which has no more specific client code than "unavailable".
    let codes: Vec<&serde_json::Value> =
        errors.iter().map(|e| &e["content"]["error_code"]).collect();
    assert_eq!(codes, ["overloaded", "unavailable"]);
    assert!(
        body["content"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["text"] == "Answer without advice."),
        "the executor's answer survives two advisor failures"
    );
    assert_eq!(
        body["usage"]["iterations"].as_array().unwrap().len(),
        3,
        "three executor samplings, no advisor billing"
    );
    let lines: Vec<_> = std::iter::from_fn(|| log_rx.try_recv().ok()).collect();
    let warnings = lines
        .iter()
        .filter(|l| l.kind == LogKind::Warning && l.text.contains("advisor"))
        .count();
    assert_eq!(
        warnings, 1,
        "at most one advisor warning for the whole turn"
    );
}

/// 4. `max_uses` caps the loop. The executor keeps asking for advice; the
/// proxy must stop after `max_uses` consultations, inject a
/// `function_call_output` for the pending (3rd) call and re-run the executor
/// ONE final time for a real answer. That extra re-run is TERMINAL: if it
/// asks for advice again, no more POSTs are made and the orphan tool_use is
/// stripped. Six POSTs is the hard bound for this script.
#[tokio::test]
async fn advisor_max_uses_caps_the_loop() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            advisor_call_response("resp_c1", "call_c1"),
            advisor_answer("Advice one."),
            advisor_call_response("resp_c2", "call_c2"),
            advisor_answer("Advice two."),
            // The cap-triggering 3rd advisor call (no consultation).
            advisor_call_response("resp_c3", "call_c3"),
            // The ONE terminal re-run: still asks for advice. Render it.
            advisor_executor_message("resp_final", "Final answer after the cap."),
        ],
    );
    let mut config = (*advisor_config(upstream_addr)).clone();
    config.advisor.max_uses = Some(2);
    let proxy_addr = start_proxy(Arc::new(config)).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = res.json().await.unwrap();

    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(
        sent.len(),
        6,
        "2 executor + 2 advisor + capped 3rd executor + ONE terminal re-run; no 7th POST"
    );
    let bodies: Vec<serde_json::Value> = sent
        .iter()
        .map(|b| serde_json::from_slice(b).unwrap())
        .collect();
    // The terminal re-run carries the FunctionCallOutput for the cap call.
    assert!(
        bodies[5]["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["type"] == "function_call_output"
                && i["call_id"] == "call_c3"
                && i["output"].as_str().unwrap().contains("limit reached")),
        "the cap-triggering call got a FunctionCallOutput"
    );
    // No orphan advisor function_call in ANY upstream request.
    for (i, b) in bodies.iter().enumerate() {
        assert!(
            !b["input"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["type"] == "function_call"
                    && item["name"] == "advisor"
                    && !b["input"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|o| o["type"] == "function_call_output"
                            && o["call_id"] == item["call_id"])),
            "attempt {i}: every advisor function_call has a matching output"
        );
    }
    let content = body["content"].as_array().unwrap();
    let results: Vec<&serde_json::Value> = content
        .iter()
        .filter(|b| b["type"] == "advisor_tool_result")
        .collect();
    assert_eq!(results.len(), 3, "2 successful pairs + the cap result");
    assert_eq!(results[0]["content"]["type"], "advisor_result");
    assert_eq!(results[1]["content"]["type"], "advisor_result");
    assert_eq!(results[2]["content"]["type"], "advisor_tool_result_error");
    assert_eq!(results[2]["content"]["error_code"], "max_uses_exceeded");
    // Every pair is contiguous with a matching id (the first 6 blocks).
    for pair in content.chunks(2).take(3) {
        assert_eq!(pair[0]["type"], "server_tool_use");
        assert_eq!(pair[1]["type"], "advisor_tool_result");
        assert_eq!(pair[0]["id"], pair[1]["tool_use_id"]);
    }
    assert!(
        content
            .iter()
            .any(|b| b["text"] == "Final answer after the cap."),
        "the terminal re-run's real answer must be rendered"
    );
    assert_eq!(body["stop_reason"], "end_turn");
}

/// `max_uses = 0` means "no limit": the consultation loop is bounded ONLY
/// by the executor's behaviour. This test drives the executor to ask 12
/// times (past the runaway threshold of 10) and proves that EVERY ask is
/// answered — no `max_uses_exceeded`, no early stop. The advisor imposes no
/// ceiling of its own; it only warns (once at startup, and once per turn past
/// 10 consultations). The SHARED round limit (`server_tools.max_rounds`, 10 by
/// default) is lifted too, or it would end the turn first.
#[tokio::test]
async fn advisor_unlimited_max_uses_never_hits_a_cap() {
    const CONSULTATIONS: usize = 12;
    let (upstream_addr, upstream) = start_fake_upstream().await;
    let mut scripted: Vec<FakeResponse> = Vec::new();
    for i in 0..CONSULTATIONS {
        scripted.push(advisor_call_response(
            &format!("resp_c{i}"),
            &format!("call_c{i}"),
        ));
        scripted.push(advisor_answer(&format!("Advice number {i}.")));
    }
    scripted.push(advisor_executor_message(
        "resp_final",
        "Done after many calls.",
    ));
    script(&upstream, scripted);

    let mut config = (*advisor_config(upstream_addr)).clone();
    config.advisor.max_uses = None;
    // Unlimited consultations also need the shared round limit lifted: by
    // default a turn runs at most 10 rounds, like Anthropic's own loop.
    config.server_tools.max_rounds = None;
    let proxy_addr = start_proxy(Arc::new(config)).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = res.json().await.unwrap();

    let content = body["content"].as_array().unwrap();
    let results: Vec<&serde_json::Value> = content
        .iter()
        .filter(|b| b["type"] == "advisor_tool_result")
        .collect();
    assert_eq!(
        results.len(),
        CONSULTATIONS,
        "every consultation was answered — the loop has no ceiling"
    );
    for (i, r) in results.iter().enumerate() {
        assert_eq!(r["content"]["type"], "advisor_result");
        assert_eq!(r["content"]["text"], format!("Advice number {i}."));
    }
    assert!(
        !content
            .iter()
            .any(|b| b["content"]["error_code"] == "max_uses_exceeded"),
        "an unlimited max_uses must never produce a cap error"
    );
    assert!(
        content
            .iter()
            .any(|b| b["text"] == "Done after many calls.")
    );

    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(
        sent.len(),
        CONSULTATIONS * 2 + 1,
        "12 executor calls + 12 advisor calls + the final executor message"
    );
}

/// DEFECT 3: a resolved `Some(0)` must mean exactly what a CONFIGURED `0`
/// means — "no limit". Before the fix, `Some(0)` meant "zero
/// consultations" (the cap fires on the first ask) and "a zero-token
/// answer", so the same number had two meanings depending on how the
/// config was built.
#[tokio::test]
async fn a_zero_limit_in_a_built_config_means_no_limit() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            advisor_call_response("resp_call1", "call_zero1"),
            advisor_answer("Advice one."),
            advisor_call_response("resp_call2", "call_zero2"),
            advisor_answer("Advice two."),
            advisor_executor_message("resp_final", "Done after two consultations."),
        ],
    );
    let mut config = (*advisor_config(upstream_addr)).clone();
    config.advisor.max_uses = Some(0);
    config.advisor.max_tokens = Some(0);
    let proxy_addr = start_proxy(Arc::new(config)).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = res.json().await.unwrap();
    let content = body["content"].as_array().unwrap();
    let results = content
        .iter()
        .filter(|b| b["type"] == "advisor_tool_result")
        .count();
    assert_eq!(
        results, 2,
        "both consultations were answered — `Some(0)` is not a zero-consultation cap"
    );
    assert!(
        !content
            .iter()
            .any(|b| b["content"]["error_code"] == "max_uses_exceeded"),
        "an unlimited max_uses must never produce a cap error"
    );

    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(
        sent.len(),
        5,
        "2 executor calls + 2 advisor calls + the final executor message"
    );
    let bodies: Vec<serde_json::Value> = sent
        .iter()
        .map(|b| serde_json::from_slice(b).unwrap())
        .collect();
    // `max_tokens: Some(0)` = "no limit" => the proxy omits the cap and the
    // upstream's own default applies.
    assert!(
        bodies[1].get("max_output_tokens").is_none(),
        "no cap on the advisor's own request: {}",
        bodies[1]
    );
    assert!(
        bodies[3].get("max_output_tokens").is_none(),
        "no cap on the advisor's own request: {}",
        bodies[3]
    );
}

/// DEFECT 3: `timeout_secs: Some(0)` in a built config means "no limit",
/// exactly like a configured `0` — no per-request timeout override, so the
/// GLOBAL `request_timeout` is the only bound. Proof: the advisor POST is
/// stalled for 2s — longer than the 1s a per-request timeout would allow —
/// and it still succeeds.
#[tokio::test]
async fn a_zero_timeout_in_a_built_config_means_no_limit() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            advisor_call_response("resp_call", "call_zero_timeout"),
            advisor_answer("Slow but useful advice."),
            advisor_executor_message("resp_final", "Here is the implementation."),
        ],
    );
    // Attempt 1 = executor (fast); attempt 2 = advisor (stalled 2s, which
    // is LONGER than the 1s per-request timeout the bounded case uses);
    // attempt 3 = final executor (fast).
    *upstream.delay_per_attempt.lock().await =
        VecDeque::from([Duration::ZERO, Duration::from_secs(2), Duration::ZERO]);
    let mut config = (*advisor_config(upstream_addr)).clone();
    config.advisor.timeout_secs = Some(0);
    // The global bound still applies — it is simply the ONLY bound.
    config.request_timeout = Duration::from_secs(10);
    let proxy_addr = start_proxy(Arc::new(config)).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK, "fail closed on a timeout");
    let body: serde_json::Value = res.json().await.unwrap();
    let result = body["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["type"] == "advisor_tool_result")
        .unwrap();
    assert_eq!(
        result["content"]["type"], "advisor_result",
        "the stalled advisor call still completed: no per-request timeout was applied"
    );
    assert_eq!(result["content"]["text"], "Slow but useful advice.");
}

/// `max_tokens = 0` means "no limit": the proxy must OMIT
/// `max_output_tokens` from the advisor's OWN upstream request, so the
/// UPSTREAM's default applies (it may well be smaller than 2048). The
/// executor's own requests are untouched — they never carried the field.
#[tokio::test]
async fn advisor_unlimited_max_tokens_omits_the_cap() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            advisor_call_response("resp_call", "call_adv_unlim"),
            advisor_answer("Use a channel-based coordination pattern."),
            advisor_executor_message("resp_final", "Here is the implementation."),
        ],
    );
    let mut config = (*advisor_config(upstream_addr)).clone();
    config.advisor.max_tokens = None;
    let proxy_addr = start_proxy(Arc::new(config)).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = res.json().await.unwrap();
    assert!(
        body["content"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["type"] == "advisor_tool_result"),
        "the consultation still happened"
    );

    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(sent.len(), 3, "executor, advisor, executor");
    let bodies: Vec<serde_json::Value> = sent
        .iter()
        .map(|b| serde_json::from_slice(b).unwrap())
        .collect();
    assert_eq!(bodies[1]["model"], "beam", "the advisor's own POST");
    assert!(
        bodies[1].get("max_output_tokens").is_none(),
        "no limit => no cap on the advisor's own request: {}",
        bodies[1]
    );
    // The bounded case (the default config) DOES send the cap — proven
    // here so the omission above cannot be an accident of the wiring.
    let (bounded_addr, bounded) = start_fake_upstream().await;
    script(
        &bounded,
        vec![
            advisor_call_response("resp_call", "call_adv_bounded"),
            advisor_answer("Advice."),
            advisor_executor_message("resp_final", "Done."),
        ],
    );
    let bounded_addr = start_proxy(advisor_config(bounded_addr)).await;
    let res = reqwest::Client::new()
        .post(format!("http://{bounded_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let sent = bounded.received_all.lock().await.clone();
    let bodies: Vec<serde_json::Value> = sent
        .iter()
        .map(|b| serde_json::from_slice(b).unwrap())
        .collect();
    assert_eq!(
        bodies[1]["max_output_tokens"], 164,
        "the bounded case: answer 64 + reasoning 100"
    );
}

/// `timeout_secs = 0` means "no limit": no per-request timeout override is
/// sent, so the GLOBAL `request_timeout` (the reqwest client's own timeout)
/// is the only bound. Proof: the advisor POST is stalled for 2s — longer
/// than the 1s a per-request timeout would allow — and it still succeeds.
/// (The bounded counterpart is `advisor_timeout_degrades_gracefully`: the
/// same stall with `timeout_secs: 1` degrades to `unavailable`.)
#[tokio::test]
async fn advisor_unlimited_timeout_sends_no_per_request_timeout() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            advisor_call_response("resp_call", "call_adv_unlim"),
            advisor_answer("Slow but useful advice."),
            advisor_executor_message("resp_final", "Here is the implementation."),
        ],
    );
    // Attempt 1 = executor (fast); attempt 2 = advisor (stalled 2s, which
    // is LONGER than the 1s per-request timeout the bounded case uses);
    // attempt 3 = final executor (fast).
    *upstream.delay_per_attempt.lock().await =
        VecDeque::from([Duration::ZERO, Duration::from_secs(2), Duration::ZERO]);
    let mut config = (*advisor_config(upstream_addr)).clone();
    config.advisor.timeout_secs = None;
    // The global bound still applies — it is simply the ONLY bound.
    config.request_timeout = Duration::from_secs(10);
    let proxy_addr = start_proxy(Arc::new(config)).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK, "fail closed on a timeout");
    let body: serde_json::Value = res.json().await.unwrap();
    let result = body["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["type"] == "advisor_tool_result")
        .unwrap();
    assert_eq!(
        result["content"]["type"], "advisor_result",
        "the stalled advisor call still completed: no per-request timeout was applied"
    );
    assert_eq!(result["content"]["text"], "Slow but useful advice.");
}

/// A `default_model` fallback RE-TRANSLATES the executor request through
/// `build_outbound`, so the advisor `prepare` hook must re-apply there too:
/// the fallback body still carries exactly one zero-argument advisor tool
/// and still forces `stream:false`. Without this, an advisor turn that
/// triggers the fallback would silently lose the tool.
#[tokio::test]
async fn advisor_fallback_retranslation_keeps_the_tool() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            FakeResponse {
                status: StatusCode::NOT_FOUND,
                body: Some(REAL_GATEWAY_MODEL_404.into()),
            },
            advisor_call_response("resp_call", "call_fb"),
            advisor_answer("Advice after fallback."),
            advisor_executor_message("resp_final", "Done after fallback."),
        ],
    );
    let mut config = (*advisor_config(upstream_addr)).clone();
    config.model_aliases = ModelAliases {
        map: BTreeMap::new(),
        default_model: Some("gpt-4o-mini".into()),
    };
    let proxy_addr = start_proxy(Arc::new(config)).await;
    // The request must name a DIFFERENT model than `default_model` for the
    // fallback to fire; `REAL_GATEWAY_MODEL_404` names claude-sonnet-5.
    let request = advisor_request(false).replace(
        "\"model\": \"gpt-4o-mini\"",
        "\"model\": \"claude-sonnet-5\"",
    );

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(request))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["stop_reason"], "end_turn");
    assert_eq!(body["content"][0]["type"], "server_tool_use");
    assert_eq!(
        body["content"][1]["content"]["text"],
        "Advice after fallback."
    );

    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(sent.len(), 4, "404 + fallback + advisor + final executor");
    let bodies: Vec<serde_json::Value> = sent
        .iter()
        .map(|b| serde_json::from_slice(b).unwrap())
        .collect();
    assert_eq!(
        bodies[0]["model"], "claude-sonnet-5",
        "rejected first model"
    );
    assert_eq!(bodies[1]["model"], "gpt-4o-mini", "fallback model");
    assert_eq!(bodies[2]["model"], "beam", "the advisor's own POST");
    assert!(bodies[2]["tools"].is_null(), "advisor POST has no tools");
    assert_eq!(bodies[3]["model"], "gpt-4o-mini");
    for i in [0usize, 1, 3] {
        let v = &bodies[i];
        let tools = v["tools"].as_array().unwrap();
        let advisor_tools: Vec<_> = tools.iter().filter(|t| t["name"] == "advisor").collect();
        assert_eq!(
            advisor_tools.len(),
            1,
            "attempt {i}: exactly one synthesized advisor tool"
        );
        assert_eq!(
            advisor_tools[0]["parameters"]["properties"],
            serde_json::json!({})
        );
        assert_eq!(v["stream"], false, "attempt {i}: still buffered");
    }
    // The 4th (final executor) POST carries the advice injection.
    assert!(
        bodies[3]["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["type"] == "function_call_output"
                && i["output"] == "Advice after fallback.")
    );
}

/// 5. `stream:true` still yields a proper Anthropic SSE sequence even though
/// every upstream request was forced to `stream:false`.
#[tokio::test]
async fn advisor_stream_true_emits_sse() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            advisor_call_response("resp_call", "call_sse"),
            advisor_answer("Keep it simple."),
            advisor_executor_message("resp_final", "Done."),
        ],
    );
    let proxy_addr = start_proxy(advisor_config(upstream_addr)).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(true)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers()
            .get(axum::http::header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap(),
        "text/event-stream",
        "hard-coded; the upstream answered application/json"
    );
    assert_eq!(
        res.headers()
            .get(axum::http::header::CACHE_CONTROL)
            .unwrap()
            .to_str()
            .unwrap(),
        "no-cache"
    );
    let text = res.text().await.unwrap();
    assert!(!text.contains("[DONE]"));

    let events: Vec<&str> = text
        .split("\n\n")
        .filter(|s| !s.trim().is_empty())
        .collect();
    let kinds: Vec<&str> = events
        .iter()
        .filter_map(|e| e.lines().find_map(|l| l.strip_prefix("event: ")))
        .collect();
    assert_eq!(kinds.iter().filter(|k| **k == "message_start").count(), 1);
    assert_eq!(kinds.iter().filter(|k| **k == "message_stop").count(), 1);
    assert_eq!(kinds.iter().filter(|k| **k == "message_delta").count(), 1);
    assert_eq!(kinds.first(), Some(&"message_start"));
    assert_eq!(kinds.last(), Some(&"message_stop"));

    // The advisor Raw blocks arrive whole (start + stop, no delta), and the
    // final message_delta carries stop_reason + usage.iterations[].
    let mut deltas = 0usize;
    let mut raw_starts = Vec::new();
    for e in &events {
        let Some(data) = e.lines().find_map(|l| l.strip_prefix("data: ")) else {
            continue;
        };
        let v: serde_json::Value = serde_json::from_str(data).unwrap();
        match v["type"].as_str().unwrap() {
            "content_block_start" => {
                let t = v["content_block"]["type"].as_str().unwrap();
                if t == "server_tool_use" || t == "advisor_tool_result" {
                    raw_starts.push(t.to_owned());
                }
            }
            "content_block_delta" => deltas += 1,
            "message_delta" => {
                assert_eq!(v["delta"]["stop_reason"], "end_turn");
                let iterations = v["usage"]["iterations"].as_array().unwrap();
                assert!(
                    iterations
                        .iter()
                        .any(|i| i["type"] == "advisor_message" && i["model"] == "beam")
                );
            }
            _ => {}
        }
    }
    assert_eq!(
        raw_starts,
        vec!["server_tool_use", "advisor_tool_result"],
        "the pair arrives in one message_start … message_stop"
    );
    assert_eq!(deltas, 1, "only the final executor text gets a delta");

    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(sent.len(), 3);
    // The executor runs are streamed upstream (the client streams); the
    // advisor's own consultation is not.
    let streams: Vec<bool> = sent
        .iter()
        .map(|body| {
            serde_json::from_slice::<serde_json::Value>(body).unwrap()["stream"]
                .as_bool()
                .unwrap()
        })
        .collect();
    assert_eq!(streams, vec![true, false, true]);
}

/// 6. Claude Code echoes both advisor blocks verbatim on the next request.
/// `ContentBlockParam` now parses them explicitly; the proxy must not 400,
/// must not emit an orphan `function_call` upstream, and must replay the
/// prior advice (see `advisor_echo_prior_advice_reaches_upstream`).
#[tokio::test]
async fn advisor_inbound_echo_does_not_400() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned.lock().await = Some(completed_response_with_id("resp_echo", "ok"));

    let request = format!(
        r#"{{
            "model": "gpt-4o-mini",
            "max_tokens": 64,
            "tools": [{ADVISOR_DECLARATION}],
            "messages": [
                {{"role":"user","content":"Do the task"}},
                {{"role":"assistant","content":[
                    {{"type":"text","text":"Let me consult the advisor."}},
                    {{"type":"server_tool_use","id":"srvtoolu_echo","name":"advisor","input":{{}}}},
                    {{"type":"advisor_tool_result","tool_use_id":"srvtoolu_echo",
                      "content":{{"type":"advisor_result","text":"Prior advice."}}}}
                ]}}
            ]
        }}"#
    );
    for enabled in [false, true] {
        let mut config = (*make_proxy_config(upstream_addr)).clone();
        config.advisor.enabled = enabled;
        let proxy_addr = start_proxy(Arc::new(config)).await;
        let res = reqwest::Client::new()
            .post(format!("http://{proxy_addr}/v1/messages"))
            .header("content-type", "application/json")
            .body(ReqBody::from(request.clone()))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "enabled={enabled}");
        let body: serde_json::Value = res.json().await.unwrap();
        assert_eq!(body["content"][0]["text"], "ok", "enabled={enabled}");
    }

    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(sent.len(), 2, "one POST per request");
    for body in &sent {
        let v: serde_json::Value = serde_json::from_slice(body).unwrap();
        let items = v["input"].as_array().expect("input is an Items array");
        assert!(
            items.iter().all(|i| i["type"] != "function_call"),
            "the echoed advisor blocks must not leak in as orphan function_call items"
        );
        assert!(
            items
                .iter()
                .any(|i| i["type"] == "message" && i["role"] == "assistant"),
            "the echoed assistant message is still well-formed"
        );
        assert!(
            items.iter().all(|i| i["type"] != "function_call_output"),
            "no synthetic tool result for an echoed block"
        );
    }
}

/// THE fidelity gap: prior advice must survive into the upstream transcript.
/// Claude Code echoes `server_tool_use` + `advisor_tool_result` back on the
/// next turn. With the advisor DISABLED no new consultation fires, so the
/// upstream sees exactly ONE POST whose assistant `output_text` carries the
/// prior advice — and no orphan `function_call`. A control request without
/// the blocks must stay clean.
#[tokio::test]
async fn advisor_echo_prior_advice_reaches_upstream() {
    const PRIOR: &str = "Use a channel-based coordination pattern.";
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned.lock().await = Some(completed_response_with_id("resp_echo2", "ok"));
    // Advisor DISABLED (make_proxy_config's default). No declaration either,
    // so the ordinary non-advisor pipeline is used for both requests.
    let config = make_proxy_config(upstream_addr);
    assert!(!config.advisor.enabled);
    let proxy_addr = start_proxy(config).await;

    let with_echo = r#"{
        "model": "gpt-4o-mini",
        "max_tokens": 64,
        "messages": [
            {"role":"user","content":"Do the task"},
            {"role":"assistant","content":[
                {"type":"text","text":"Let me consult the advisor."},
                {"type":"server_tool_use","id":"srvtoolu_echo2",
                 "name":"advisor","input":{}},
                {"type":"advisor_tool_result","tool_use_id":"srvtoolu_echo2",
                 "content":{"type":"advisor_result",
                            "text":"Use a channel-based coordination pattern."}}
            ]},
            {"role":"user","content":"Continue"}
        ]
    }"#;
    let without_echo = r#"{
        "model": "gpt-4o-mini",
        "max_tokens": 64,
        "messages": [
            {"role":"user","content":"Do the task"},
            {"role":"assistant","content":[
                {"type":"text","text":"Let me consult the advisor."}
            ]},
            {"role":"user","content":"Continue"}
        ]
    }"#;

    for (label, body) in [("with echo", with_echo), ("control", without_echo)] {
        let res = reqwest::Client::new()
            .post(format!("http://{proxy_addr}/v1/messages"))
            .header("content-type", "application/json")
            .body(ReqBody::from(body))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{label}");
        let answer: serde_json::Value = res.json().await.unwrap();
        assert_eq!(answer["type"], "message", "{label}");
        assert_eq!(answer["content"][0]["type"], "text", "{label}");
        assert_eq!(answer["content"][0]["text"], "ok", "{label}");
        assert_eq!(answer["stop_reason"], "end_turn", "{label}");
    }

    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(
        sent.len(),
        2,
        "one upstream POST per request; no consultation"
    );
    let v: serde_json::Value = serde_json::from_slice(&sent[0]).unwrap();
    let items = v["input"].as_array().expect("input is an Items array");
    let assistant_parts: Vec<&serde_json::Value> = items
        .iter()
        .filter(|i| i["type"] == "message" && i["role"] == "assistant")
        .flat_map(|i| i["content"].as_array().unwrap())
        .collect();
    assert_eq!(assistant_parts.len(), 1, "one assistant content part");
    // Responses requires `output_text` on assistant messages (`input_text`
    // is rejected upstream), so the advice lives in that part's `text`.
    assert_eq!(assistant_parts[0]["type"], "output_text");
    let assistant_text = assistant_parts[0]["text"].as_str().unwrap();
    assert_eq!(
        assistant_text,
        format!("Let me consult the advisor.\n[Advisor advice from a previous turn: {PRIOR}]\n"),
        "prior advice missing or not clearly delimited: {assistant_text:?}"
    );
    assert!(
        items.iter().all(|i| i["type"] != "function_call"),
        "no orphan function_call from the echoed server_tool_use"
    );
    assert!(
        items.iter().all(|i| i["type"] != "function_call_output"),
        "no synthetic tool result for the echoed advisor block"
    );

    let control: serde_json::Value = serde_json::from_slice(&sent[1]).unwrap();
    let control_text = serde_json::to_string(&control["input"]).unwrap();
    assert!(
        !control_text.contains(PRIOR),
        "control request must carry no advice text"
    );
    assert!(!control_text.contains("Advisor advice"));
}

/// 7. The zero-argument function tool reaches the upstream with an empty
/// schema, and the `# Advisor Tool` system section is NOT stripped.
#[tokio::test]
async fn advisor_tool_declaration_reaches_upstream() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned.lock().await = Some(completed_response_with_id("resp_tool", "ok"));
    let proxy_addr = start_proxy(advisor_config(upstream_addr)).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(sent.len(), 1, "no consultation was requested");
    let v: serde_json::Value = serde_json::from_slice(&sent[0]).unwrap();
    let tools = v["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 1);
    assert_eq!(
        tools[0],
        serde_json::json!({
            "type": "function",
            "name": "advisor",
            "description": "Consult a reviewer model before substantive work. Takes no arguments.",
            "strict": true,
            "parameters": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }
        })
    );
    assert_eq!(
        tools[0]["parameters"]["properties"]
            .as_object()
            .unwrap()
            .len(),
        0
    );
    assert_eq!(v["stream"], false, "advisor path buffers the executor");
    let instructions = v["instructions"].as_str().unwrap();
    assert!(instructions.contains("# Advisor Tool"), "section preserved");
    assert!(instructions.contains("Call advisor BEFORE substantive work."));
    assert!(instructions.contains("<system-reminder>skills</system-reminder>"));
}

/// An executor response with BOTH visible text and an advisor call (B3).
fn advisor_call_with_text(id: &str, call_id: &str, text: &str) -> FakeResponse {
    FakeResponse {
        status: StatusCode::OK,
        body: Some(format!(
            r#"{{
                "id": "{id}", "object": "response", "created_at": 1,
                "status": "completed", "model": "gpt-4o-mini",
                "output": [
                    {{"type": "message", "role": "assistant",
                      "content": [{{"type": "output_text", "text": "{text}", "annotations": []}}]}},
                    {{"type": "function_call", "call_id": "{call_id}",
                      "name": "advisor", "arguments": "{{}}"}}
                ],
                "usage": {{"input_tokens":9,"output_tokens":4,"total_tokens":13}}
            }}"#
        )),
    }
}

/// An advisor response with a non-`completed` status but a plausible-looking
/// `output_text`. B2: this must NEVER be accepted as advice.
fn advisor_answer_with_status(status: &str, text: &str) -> FakeResponse {
    FakeResponse {
        status: StatusCode::OK,
        body: Some(format!(
            r#"{{
                "id": "resp_advice_bad", "object": "response", "created_at": 1,
                "status": "{status}", "model": "beam",
                "output": [{{
                    "type": "message", "role": "assistant",
                    "content": [{{"type": "output_text", "text": "{text}", "annotations": []}}]
                }}],
                "usage": {{"input_tokens":21,"output_tokens":13,"total_tokens":34}}
            }}"#
        )),
    }
}

/// An advisor call with NO `usage` field (B10).
fn advisor_call_without_usage(id: &str, call_id: &str) -> FakeResponse {
    FakeResponse {
        status: StatusCode::OK,
        body: Some(format!(
            r#"{{
                "id": "{id}", "object": "response", "created_at": 1,
                "status": "completed", "model": "gpt-4o-mini",
                "output": [{{
                    "type": "function_call", "call_id": "{call_id}",
                    "name": "advisor", "arguments": "{{}}"
                }}]
            }}"#
        )),
    }
}

/// A completed response with NO `usage` field (B10).
fn response_without_usage(id: &str, text: &str) -> FakeResponse {
    FakeResponse {
        status: StatusCode::OK,
        body: Some(format!(
            r#"{{
                "id": "{id}", "object": "response", "created_at": 1,
                "status": "completed", "model": "gpt-4o-mini",
                "output": [{{
                    "type": "message", "role": "assistant",
                    "content": [{{"type": "output_text", "text": "{text}", "annotations": []}}]
                }}]
            }}"#
        )),
    }
}

/// A terminal `incomplete` / `max_output_tokens` executor response (B7).
fn executor_max_tokens_response(id: &str) -> FakeResponse {
    FakeResponse {
        status: StatusCode::OK,
        body: Some(format!(
            r#"{{
                "id": "{id}", "object": "response", "created_at": 1,
                "status": "incomplete", "model": "gpt-4o-mini",
                "incomplete_details": {{"reason": "max_output_tokens"}},
                "output": [{{
                    "type": "message", "role": "assistant",
                    "content": [{{"type": "output_text", "text": "Truncated.", "annotations": []}}]
                }}],
                "usage": {{"input_tokens":7,"output_tokens":64,"total_tokens":71}}
            }}"#
        )),
    }
}

/// B1(a): an executor RE-RUN error must NOT fail the user's turn. The last
/// good response is rendered with an `unavailable` pair, and the executor's
/// own answer survives.
#[tokio::test]
async fn b1_rerun_500_degrades_to_last_good_with_unavailable() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            advisor_call_with_text_and_bash("resp_first", "call_b1", "Let me consult the advisor."),
            advisor_answer("Use a channel."),
            FakeResponse {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                body: Some(r#"{"error":{"message":"executor exploded"}}"#.into()),
            },
        ],
    );
    let proxy_addr = start_proxy(advisor_config(upstream_addr)).await;

    let started = std::time::Instant::now();
    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        StatusCode::OK,
        "never propagate a re-run error"
    );
    let body: serde_json::Value = res.json().await.unwrap();
    assert!(started.elapsed() < Duration::from_secs(5));

    let content = body["content"].as_array().unwrap();
    assert!(
        content
            .iter()
            .any(|b| b["text"] == "Let me consult the advisor."),
        "the last good executor answer survives"
    );
    let unavailable: Vec<_> = content
        .iter()
        .filter(|b| b["type"] == "advisor_tool_result")
        .collect();
    assert!(
        unavailable
            .iter()
            .any(|b| b["content"]["error_code"] == "unavailable"),
        "a fail-closed unavailable block is emitted for the failed re-run"
    );
    // The advisor's successful pair is still contiguous and id-matched.
    let mut pair_positions = Vec::new();
    for (i, b) in content.iter().enumerate() {
        if b["type"] == "server_tool_use" {
            let id = &b["id"];
            assert_eq!(
                content[i + 1]["type"],
                "advisor_tool_result",
                "server_tool_use must be immediately followed by its result"
            );
            assert_eq!(content[i + 1]["tool_use_id"], *id);
            pair_positions.push(i);
        }
    }
    assert_eq!(pair_positions.len(), 2, "consultation pair + degraded pair");
    assert_eq!(body["stop_reason"], "tool_use", "Bash is still pending");
}

/// B1(b): a re-run 400 naming ALL THREE beta-latching tokens must not leak
/// any of them into the client body. §18.4: such a 400 makes Claude Code
/// disable the advisor for the whole process.
#[tokio::test]
async fn b1_rerun_400_with_advisor_tokens_is_scrubbed() {
    const BETA_400: &str = r#"{"error":{"type":"invalid_request_error","message":"Input tag 'advisor_20260301' found; advisor-tool-2026-03-01 beta required for advisor_20260301"}}"#;
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            advisor_call_response("resp_first", "call_b1b"),
            advisor_answer("Use a channel."),
            FakeResponse {
                status: StatusCode::BAD_REQUEST,
                body: Some(BETA_400.into()),
            },
        ],
    );
    let proxy_addr = start_proxy(advisor_config(upstream_addr)).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK, "re-run error is fail-closed");
    let raw = res.text().await.unwrap();
    for token in ["advisor_20260301", "advisor-tool-2026-03-01", "Input tag"] {
        assert!(
            !raw.contains(token),
            "{token} must NEVER reach the client from the advisor path"
        );
    }
    let body: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert!(
        body["content"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["content"]["error_code"] == "unavailable"),
        "the turn degrades to an unavailable block instead"
    );
}

/// B1 (first-attempt variant): an advisor-active turn whose FIRST executor
/// POST returns a beta-naming 400 still propagates the error, but the body
/// must be scrubbed so Claude Code cannot latch the feature off.
#[tokio::test]
async fn b1_first_executor_400_is_scrubbed_before_reaching_client() {
    const BETA_400: &str = r#"{"error":{"type":"invalid_request_error","message":"Input tag 'advisor_20260301' found; advisor-tool-2026-03-01 beta required"}}"#;
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![FakeResponse {
            status: StatusCode::BAD_REQUEST,
            body: Some(BETA_400.into()),
        }],
    );
    let proxy_addr = start_proxy(advisor_config(upstream_addr)).await;
    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let raw = res.text().await.unwrap();
    for token in ["advisor_20260301", "advisor-tool-2026-03-01", "Input tag"] {
        assert!(!raw.contains(token), "{token} leaked on the first error");
    }
}

/// B2: a 200 with `status:"failed"`/`"incomplete"` and an `output_text`
/// must degrade to `unavailable`, and its text must NOT be injected into the
/// executor prompt or billed as an `advisor_message`.
#[tokio::test]
async fn b2_failed_advisor_status_is_not_accepted_as_advice() {
    for status in ["failed", "incomplete"] {
        let (upstream_addr, upstream) = start_fake_upstream().await;
        script(
            &upstream,
            vec![
                advisor_call_response("resp_call", "call_b2"),
                // The unusable response is retried once; the retry is bad too.
                advisor_answer_with_status(status, "FAKE ADVICE FROM A FAILED RESPONSE"),
                advisor_answer_with_status(status, "FAKE ADVICE FROM A FAILED RESPONSE"),
                advisor_executor_message("resp_final", "Answer without advice."),
            ],
        );
        let proxy_addr = start_proxy(advisor_config(upstream_addr)).await;
        let res = reqwest::Client::new()
            .post(format!("http://{proxy_addr}/v1/messages"))
            .header("content-type", "application/json")
            .body(ReqBody::from(advisor_request(false)))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "status={status}");
        let body: serde_json::Value = res.json().await.unwrap();
        let content = body["content"].as_array().unwrap();
        let result = content
            .iter()
            .find(|b| b["type"] == "advisor_tool_result")
            .unwrap();
        assert_eq!(result["content"]["error_code"], "unavailable", "{status}");
        assert!(result["content"]["text"].is_null(), "{status}");
        assert!(
            content
                .iter()
                .any(|b| b["text"] == "Answer without advice."),
            "{status}: the executor still answers"
        );
        let iterations = body["usage"]["iterations"].as_array().unwrap();
        assert!(
            iterations.iter().all(|i| i["type"] == "message"),
            "{status}: a failed review must not be billed"
        );

        // The failed text was NOT injected into the executor's transcript.
        let sent = upstream.received_all.lock().await.clone();
        assert_eq!(
            sent.len(),
            4,
            "{status}: the advisor and ONE retry, no more"
        );
        let final_body: serde_json::Value = serde_json::from_slice(&sent[3]).unwrap();
        let injected = final_body["input"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["type"] == "function_call_output")
            .unwrap()["output"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(
            !injected.contains("FAKE ADVICE"),
            "{status}: failed advice leaked into the executor prompt"
        );
        assert!(
            injected.contains("returned no advice"),
            "{status}: {injected}"
        );
    }
}

/// B3: at `max_uses`, the pending advisor call gets a FunctionCallOutput,
/// the executor is re-run ONCE for a real answer, and no orphan advisor
/// `function_call` reaches the upstream. The total POST count is bounded.
#[tokio::test]
async fn b3_cap_injects_output_reruns_once_and_bounds_the_loop() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            // max_uses = 1: the first advisor call IS consulted…
            advisor_call_response("resp_first", "call_first"),
            advisor_answer("Advice one."),
            // …the second one hits the cap and gets a FunctionCallOutput…
            advisor_call_response("resp_cap", "call_cap"),
            // …and the ONE terminal re-run STILL asks for advice, but also
            // gives a real text answer. No further consultation and no
            // further POST is allowed.
            advisor_call_with_text(
                "resp_rerun_asks_again",
                "call_cap_again",
                "Terminal answer despite another advisor request.",
            ),
            advisor_executor_message("resp_never", "should not be reached"),
        ],
    );
    let mut config = (*advisor_config(upstream_addr)).clone();
    config.advisor.max_uses = Some(1);
    let proxy_addr = start_proxy(Arc::new(config)).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = res.json().await.unwrap();

    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(
        sent.len(),
        4,
        "executor + advisor + cap executor + ONE terminal re-run; no 5th POST"
    );
    let bodies: Vec<serde_json::Value> = sent
        .iter()
        .map(|b| serde_json::from_slice(b).unwrap())
        .collect();
    // The cap-triggering call is answered with a FunctionCallOutput on the
    // terminal re-run (and on no other request).
    let caps: Vec<&serde_json::Value> = bodies[3]["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["type"] == "function_call_output" && i["call_id"] == "call_cap")
        .collect();
    assert_eq!(caps.len(), 1, "exactly one injected cap output");
    assert!(
        caps[0]["output"]
            .as_str()
            .unwrap()
            .contains("limit reached")
    );
    for (i, b) in bodies.iter().enumerate() {
        for item in b["input"].as_array().unwrap() {
            if item["type"] == "function_call" && item["name"] == "advisor" {
                assert!(
                    b["input"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|o| o["type"] == "function_call_output"
                            && o["call_id"] == item["call_id"]),
                    "attempt {i}: orphan advisor function_call"
                );
            }
        }
    }
    let content = body["content"].as_array().unwrap();
    assert!(
        !content
            .iter()
            .any(|b| b["type"] == "tool_use" && b["name"] == "advisor"),
        "no orphan client tool_use named advisor"
    );
    assert!(
        content
            .iter()
            .any(|b| b["content"]["error_code"] == "max_uses_exceeded"),
        "the cap is reported to the client"
    );
    assert!(
        content.iter().all(|b| b["type"] != "pause_turn"),
        "never pause_turn"
    );
    assert!(
        content.iter().any(|b| b["type"] == "text"
            && b["text"] == "Terminal answer despite another advisor request."),
        "a text block from the terminal re-run is rendered"
    );
    assert_eq!(body["stop_reason"], "end_turn");
}

/// B3 (real-answer variant): the terminal re-run produces a plain text
/// answer. It must be rendered (not discarded) and no orphan call lingers.
#[tokio::test]
async fn b3_cap_terminal_rerun_renders_a_real_answer() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            advisor_call_response("resp_first", "call_first2"),
            advisor_answer("Advice one."),
            // Cap-triggering call: inject FunctionCallOutput, re-run ONCE.
            advisor_call_response("resp_cap", "call_cap2"),
            advisor_executor_message("resp_answer", "A real answer after the cap."),
        ],
    );
    let mut config = (*advisor_config(upstream_addr)).clone();
    config.advisor.max_uses = Some(1);
    let proxy_addr = start_proxy(Arc::new(config)).await;
    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = res.json().await.unwrap();
    let content = body["content"].as_array().unwrap();
    assert!(
        content
            .iter()
            .any(|b| b["text"] == "A real answer after the cap."),
        "the terminal re-run's answer must be rendered"
    );
    let results: Vec<&serde_json::Value> = content
        .iter()
        .filter(|b| b["type"] == "advisor_tool_result")
        .collect();
    assert_eq!(results.len(), 2, "one successful pair + one cap result");
    assert_eq!(results[0]["content"]["type"], "advisor_result");
    assert_eq!(results[1]["content"]["error_code"], "max_uses_exceeded");
    assert_eq!(
        upstream.received_all.lock().await.len(),
        4,
        "no POST beyond the bound"
    );
}

/// HIGH regression: the terminal cap re-run asked for `advisor` again. Its
/// content must be rendered EXACTLY ONCE: no second `intermediate_blocks`
/// splice, no THIRD `server_tool_use` / `max_uses_exceeded` pair, and no POST
/// after the bound. With `max_uses = 1` the exact sequence is the consulted
/// pair, the (already emitted) cap pair, then the terminal text. The advisor-
/// only `function_call` is stripped from the final response, and the previous
/// cap call still has NO_MORE_ADVICE in the upstream transcript.
#[tokio::test]
async fn cap_terminal_rerun_with_another_advisor_call_renders_once() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            // max_uses = 1: first call is consulted, second hits the cap.
            advisor_call_response("resp_first", "call_first"),
            advisor_answer("Advice one."),
            advisor_call_response("resp_cap", "call_cap"),
            // Terminal re-run: real text AND another advisor call. This
            // response must render once, from final_resp only.
            advisor_call_with_text("resp_terminal", "call_terminal_again", "Terminal answer."),
            advisor_executor_message("resp_never", "must not be reached"),
        ],
    );
    let mut config = (*advisor_config(upstream_addr)).clone();
    config.advisor.max_uses = Some(1);
    let proxy_addr = start_proxy(Arc::new(config)).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = res.json().await.unwrap();
    let content = body["content"].as_array().unwrap();

    let types: Vec<&str> = content
        .iter()
        .map(|b| b["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        types,
        vec![
            "server_tool_use",
            "advisor_tool_result",
            "server_tool_use",
            "advisor_tool_result",
            "text",
        ],
        "exact block sequence: consulted pair + cap pair + ONE terminal text"
    );
    assert_eq!(content[4]["text"], "Terminal answer.");
    assert_eq!(
        content
            .iter()
            .filter(|b| b["type"] == "text" && b["text"] == "Terminal answer.")
            .count(),
        1,
        "terminal text appears exactly once"
    );
    assert_eq!(content[1]["content"]["type"], "advisor_result");
    assert_eq!(content[1]["content"]["text"], "Advice one.");
    assert_eq!(content[1]["tool_use_id"], content[0]["id"]);
    assert_eq!(content[3]["content"]["error_code"], "max_uses_exceeded");
    assert_eq!(content[3]["tool_use_id"], content[2]["id"]);
    assert_ne!(content[0]["id"], content[2]["id"]);
    assert!(
        !content[0]["id"].as_str().unwrap().is_empty()
            && !content[2]["id"].as_str().unwrap().is_empty(),
        "consultation ids are non-empty"
    );
    assert!(
        !content
            .iter()
            .any(|b| b["type"] == "tool_use" && b["name"] == "advisor"),
        "no orphan tool_use named advisor"
    );
    assert_eq!(
        content
            .iter()
            .filter(|b| b["content"]["error_code"] == "max_uses_exceeded")
            .count(),
        1,
        "exactly ONE cap result, never a third pair"
    );
    assert_eq!(body["stop_reason"], "end_turn");
    assert_eq!(
        body["usage"]["iterations"].as_array().unwrap().len(),
        4,
        "executor, advisor, cap executor, terminal executor"
    );

    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(sent.len(), 4, "no POST beyond the bound");
    let bodies: Vec<serde_json::Value> = sent
        .iter()
        .map(|b| serde_json::from_slice(b).unwrap())
        .collect();
    // NO_MORE_ADVICE answers the cap call on the terminal re-run.
    let cap_outputs: Vec<&serde_json::Value> = bodies[3]["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["type"] == "function_call_output" && i["call_id"] == "call_cap")
        .collect();
    assert_eq!(cap_outputs.len(), 1);
    assert!(
        cap_outputs[0]["output"]
            .as_str()
            .unwrap()
            .contains("limit reached")
    );
    // No orphan advisor function_call in ANY upstream input.
    for (i, b) in bodies.iter().enumerate() {
        for item in b["input"].as_array().unwrap() {
            if item["type"] == "function_call" && item["name"] == "advisor" {
                assert!(
                    b["input"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|o| o["type"] == "function_call_output"
                            && o["call_id"] == item["call_id"]),
                    "attempt {i}: orphan advisor function_call"
                );
            }
        }
    }
}

/// The consultation cap: the executor asks a SECOND time, the cap is
/// reached, and the turn still ends with exactly ONE cap pair and ONE
/// text block.
///
/// `max_uses: Some(1)` — NOT `Some(0)`: `0` means "no limit" everywhere in
/// the codebase (the same meaning as `None`), so the cap is forced by the
/// executor asking twice, not by a zero bound.
#[tokio::test]
async fn cap_terminal_rerun_emits_exactly_one_cap_pair_and_text() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            // 1. the executor asks for advice
            advisor_call_response("resp_call1", "call_first"),
            // 2. the advisor answers
            advisor_answer("Advice one."),
            // 3. the executor asks AGAIN — this hits the cap
            advisor_call_response("resp_call2", "call_cap"),
            // 4. the ONE terminal re-run finally answers
            advisor_executor_message("resp_terminal", "Terminal answer."),
            advisor_executor_message("resp_never", "must not be reached"),
        ],
    );
    let mut config = (*advisor_config(upstream_addr)).clone();
    config.advisor.max_uses = Some(1);
    let proxy_addr = start_proxy(Arc::new(config)).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = res.json().await.unwrap();
    let content = body["content"].as_array().unwrap();
    let types: Vec<&str> = content
        .iter()
        .map(|b| b["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        types,
        vec![
            "server_tool_use",
            "advisor_tool_result",
            "server_tool_use",
            "advisor_tool_result",
            "text",
        ],
        "exact block sequence: consulted pair + cap pair + ONE terminal text"
    );
    // EXACTLY ONE cap pair: one `max_uses_exceeded` result, closing the
    // `server_tool_use` that opened it.
    let caps: Vec<&serde_json::Value> = content
        .iter()
        .filter(|b| b["content"]["error_code"] == "max_uses_exceeded")
        .collect();
    assert_eq!(caps.len(), 1, "exactly one cap result");
    assert_eq!(caps[0]["tool_use_id"], content[2]["id"]);
    // EXACTLY ONE text block: the terminal answer.
    let texts: Vec<&serde_json::Value> = content.iter().filter(|b| b["type"] == "text").collect();
    assert_eq!(texts.len(), 1, "exactly one text block");
    assert_eq!(texts[0]["text"], "Terminal answer.");
    assert_eq!(body["stop_reason"], "end_turn");
    assert_eq!(
        upstream.received_all.lock().await.len(),
        4,
        "executor + advisor + cap executor + ONE terminal re-run; no 5th POST"
    );
}

/// B4: `timeout_secs > request_timeout` must NOT extend the operator's
/// timeout. A large advisor timeout is clamped to `request_timeout`, so a
/// stalled advisor is cut off quickly and the turn degrades.
#[tokio::test]
async fn b4_advisor_timeout_is_clamped_to_request_timeout() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            advisor_call_response("resp_call", "call_b4"),
            advisor_answer("never arrives"),
            advisor_answer("never arrives"),
            advisor_executor_message("resp_final", "Continued without advice."),
        ],
    );
    // The advisor stalls on both its attempt and its one retry.
    *upstream.delay_per_attempt.lock().await = VecDeque::from([
        Duration::ZERO,
        Duration::from_secs(3),
        Duration::from_secs(3),
        Duration::ZERO,
    ]);
    let mut config = (*advisor_config(upstream_addr)).clone();
    config.request_timeout = Duration::from_millis(300);
    config.advisor.timeout_secs = Some(5);
    let proxy_addr = start_proxy(Arc::new(config)).await;

    let started = std::time::Instant::now();
    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    let elapsed = started.elapsed();
    assert_eq!(res.status(), StatusCode::OK, "degrades, never errors");
    assert!(
        elapsed < Duration::from_millis(1500),
        "the 5s advisor timeout must be clamped to 300ms; turn took {elapsed:?}"
    );
    let body: serde_json::Value = res.json().await.unwrap();
    let result = body["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["type"] == "advisor_tool_result")
        .unwrap();
    assert_eq!(result["content"]["error_code"], "execution_time_exceeded");
    assert!(
        body["content"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["text"] == "Continued without advice."),
        "the executor re-run still answers"
    );
}

/// B5: a `default_model` fallback on the FINAL executor re-run must preserve
/// the injected advice (`function_call_output`) in the accumulator.
#[tokio::test]
async fn b5_mid_session_fallback_preserves_injected_advice() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            advisor_call_response("resp_call", "call_b5"),
            advisor_answer("Advice that must survive the fallback."),
            // The FINAL executor call first returns a model-rejection 404.
            FakeResponse {
                status: StatusCode::NOT_FOUND,
                body: Some(REAL_GATEWAY_MODEL_404.into()),
            },
            // Then succeeds via `default_model`.
            advisor_executor_message("resp_final", "Done after fallback."),
        ],
    );
    let mut config = (*advisor_config(upstream_addr)).clone();
    config.model_aliases = ModelAliases {
        map: BTreeMap::new(),
        default_model: Some("gpt-4o-mini".into()),
    };
    let proxy_addr = start_proxy(Arc::new(config)).await;
    let request = advisor_request(false).replace(
        "\"model\": \"gpt-4o-mini\"",
        "\"model\": \"claude-sonnet-5\"",
    );

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(request))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["stop_reason"], "end_turn");
    assert!(
        body["content"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["text"] == "Done after fallback.")
    );

    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(sent.len(), 4, "call, advisor, rejected final, fallback");
    let bodies: Vec<serde_json::Value> = sent
        .iter()
        .map(|b| serde_json::from_slice(b).unwrap())
        .collect();
    assert_eq!(bodies[2]["model"], "claude-sonnet-5");
    assert_eq!(bodies[3]["model"], "gpt-4o-mini", "fallback model");
    let items = bodies[3]["input"].as_array().unwrap();
    assert_eq!(
        items.len(),
        3,
        "original translation + injected function_call/output pair"
    );
    assert!(
        items.iter().any(|i| i["type"] == "function_call_output"
            && i["output"] == "Advice that must survive the fallback."),
        "the injected advice survives the re-translation"
    );
    let call = items.iter().find(|i| i["type"] == "function_call").unwrap();
    assert_eq!(call["call_id"], "call_b5");
    let output = items
        .iter()
        .find(|i| i["type"] == "function_call_output")
        .unwrap();
    assert_eq!(output["call_id"], "call_b5", "no orphan function_call");
}

/// B6: intermediate executor text and a sibling client `tool_use` must
/// survive to the final Anthropic message, with `stop_reason == tool_use`.
#[tokio::test]
async fn b6_intermediate_content_and_sibling_tool_use_survive() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            advisor_call_with_text_and_bash(
                "resp_first",
                "call_b6",
                "Let me consult the advisor first.",
            ),
            advisor_answer("Use a channel."),
            advisor_executor_message("resp_final", "Here is the implementation."),
        ],
    );
    let proxy_addr = start_proxy(advisor_config(upstream_addr)).await;
    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = res.json().await.unwrap();
    let content = body["content"].as_array().unwrap();

    assert!(
        content
            .iter()
            .any(|b| b["text"] == "Let me consult the advisor first."),
        "intermediate executor text survives"
    );
    assert!(
        content
            .iter()
            .any(|b| b["type"] == "tool_use" && b["name"] == "Bash"),
        "sibling client tool_use survives"
    );
    assert!(
        content
            .iter()
            .any(|b| b["text"] == "Here is the implementation."),
        "the final executor text survives"
    );
    // The pair remains contiguous and id-matched.
    let use_pos = content
        .iter()
        .position(|b| b["type"] == "server_tool_use")
        .unwrap();
    assert_eq!(content[use_pos + 1]["type"], "advisor_tool_result");
    assert_eq!(content[use_pos + 1]["tool_use_id"], content[use_pos]["id"]);
    assert_eq!(content[use_pos + 1]["content"]["text"], "Use a channel.");
    // Order: intermediate text/tool_use precede the consultation pair.
    assert!(
        content
            .iter()
            .position(|b| b["text"] == "Let me consult the advisor first.")
            .unwrap()
            < use_pos
    );
    assert_eq!(
        body["stop_reason"], "tool_use",
        "a client tool is still pending"
    );
}

/// B7: a terminal `incomplete` / `max_output_tokens` reason must survive
/// the response assembly (not be clobbered to `end_turn`); a normal `completed`
/// response still yields `end_turn`.
#[tokio::test]
async fn b7_stop_reason_is_preserved_for_incomplete_and_completed() {
    // (a) incomplete / max_output_tokens → max_tokens
    {
        let (upstream_addr, upstream) = start_fake_upstream().await;
        script(
            &upstream,
            vec![
                advisor_call_response("resp_call", "call_b7"),
                advisor_answer("Be concise."),
                executor_max_tokens_response("resp_incomplete"),
            ],
        );
        let proxy_addr = start_proxy(advisor_config(upstream_addr)).await;
        let res = reqwest::Client::new()
            .post(format!("http://{proxy_addr}/v1/messages"))
            .header("content-type", "application/json")
            .body(ReqBody::from(advisor_request(false)))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body: serde_json::Value = res.json().await.unwrap();
        assert_eq!(body["stop_reason"], "max_tokens");
        assert!(
            body["content"]
                .as_array()
                .unwrap()
                .iter()
                .any(|b| b["text"] == "Truncated.")
        );
    }
    // (b) a normal completed turn → end_turn
    {
        let (upstream_addr, upstream) = start_fake_upstream().await;
        script(
            &upstream,
            vec![
                advisor_call_response("resp_call", "call_b7b"),
                advisor_answer("Be concise."),
                advisor_executor_message("resp_done", "Done."),
            ],
        );
        let proxy_addr = start_proxy(advisor_config(upstream_addr)).await;
        let res = reqwest::Client::new()
            .post(format!("http://{proxy_addr}/v1/messages"))
            .header("content-type", "application/json")
            .body(ReqBody::from(advisor_request(false)))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body: serde_json::Value = res.json().await.unwrap();
        assert_eq!(body["stop_reason"], "end_turn");
    }
}

/// B8: an ordinary function tool named `advisor` (WITH an input_schema) must
/// NOT activate the advisor pipeline — one POST, normal streaming preserved.
#[tokio::test]
async fn b8_ordinary_function_tool_named_advisor_does_not_activate() {
    const ORDINARY_ADVISOR_TOOL: &str = r#"{"name":"advisor","description":"an ordinary client tool","input_schema":{"type":"object","properties":{}}}"#;
    let (upstream_addr, upstream) = start_fake_upstream().await;
    let sse = [
        r#"event: response.created
data: {"type":"response.created","response":{"id":"resp_1","object":"response","created_at":0,"status":"in_progress","model":"gpt-4o","output":[]}}"#,
        r#"event: response.output_text.delta
data: {"type":"response.output_text.delta","item_id":"msg_x","output_index":0,"content_index":0,"delta":"Hi"}"#,
        r#"event: response.completed
data: {"type":"response.completed","response":{"id":"resp_1","object":"response","created_at":0,"status":"completed","model":"gpt-4o","output":[],"usage":{"input_tokens":4,"output_tokens":2,"total_tokens":6}}}"#,
    ]
    .join("\n\n")
        + "\n\n";
    *upstream.canned_stream.lock().await = Some(sse);
    // Advisor config ENABLED: the discriminator must be the declaration
    // shape, not the config flag.
    let proxy_addr = start_proxy(advisor_config(upstream_addr)).await;
    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request_with_decl(
            true,
            ORDINARY_ADVISOR_TOOL,
        )))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers()
            .get(axum::http::header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap(),
        "text/event-stream",
        "the REAL translated SSE stream, not the synthetic advisor sequence"
    );
    let text = res.text().await.unwrap();
    let kinds: Vec<&str> = text
        .split("\n\n")
        .filter(|s| !s.trim().is_empty())
        .filter_map(|e| e.lines().find_map(|l| l.strip_prefix("event: ")))
        .collect();
    assert_eq!(kinds.iter().filter(|k| **k == "message_start").count(), 1);
    assert_eq!(kinds.iter().filter(|k| **k == "message_stop").count(), 1);

    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(sent.len(), 1, "NO extra advisor POST");
    let v: serde_json::Value = serde_json::from_slice(&sent[0]).unwrap();
    assert_eq!(v["stream"], true, "normal streaming is preserved");
    let tools = v["tools"].as_array().unwrap();
    assert_eq!(
        tools.iter().filter(|t| t["name"] == "advisor").count(),
        1,
        "exactly one (the client's own) advisor function tool; no duplicate"
    );
    assert_eq!(tools[0]["parameters"]["type"], "object");
    // B9: inactive advisor → the orphan section is removed.
    assert!(
        !v["instructions"]
            .as_str()
            .unwrap()
            .contains("Call advisor BEFORE substantive work")
    );
}

/// B9: with `enabled:false` (the shipped default) and an inbound advisor
/// declaration, the orphan `# Advisor Tool` instructions must NOT reach the
/// executor; with `enabled:true` they MUST be kept.
#[tokio::test]
async fn b9_disabled_and_declared_strips_orphan_advisor_instructions() {
    for (label, enabled, expect_instructions) in
        [("disabled", false, false), ("enabled", true, true)]
    {
        let (upstream_addr, upstream) = start_fake_upstream().await;
        *upstream.canned.lock().await = Some(completed_response_with_id("resp_b9", "ok"));
        let mut config = (*advisor_config(upstream_addr)).clone();
        config.advisor.enabled = enabled;
        let proxy_addr = start_proxy(Arc::new(config)).await;
        let res = reqwest::Client::new()
            .post(format!("http://{proxy_addr}/v1/messages"))
            .header("content-type", "application/json")
            .body(ReqBody::from(advisor_request(false)))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{label}");
        let sent = upstream.received_all.lock().await.clone();
        let v: serde_json::Value = serde_json::from_slice(&sent[0]).unwrap();
        let instructions = v["instructions"].as_str().unwrap();
        assert_eq!(
            instructions.contains("Call advisor BEFORE substantive work"),
            expect_instructions,
            "{label}: advisor instructions policy"
        );
        if expect_instructions {
            assert!(instructions.contains("# Advisor Tool"), "{label}");
        } else {
            assert!(!instructions.contains("# Advisor Tool"), "{label}");
            // Unrelated system sections must survive the strip.
            assert!(instructions.contains("<system-reminder>skills</system-reminder>"));
            assert!(instructions.contains("Git status: clean"));
        }
    }
}

/// B10: a zero-usage advisor-active turn must emit NO `iterations` field.
#[tokio::test]
async fn b10_zero_usage_turn_omits_iterations() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            advisor_call_without_usage("resp_call", "call_b10"),
            FakeResponse {
                status: StatusCode::OK,
                body: Some(
                    r#"{
                        "id": "resp_advice_nousage", "object": "response", "created_at": 1,
                        "status": "completed", "model": "beam",
                        "output": [{"type": "message", "role": "assistant",
                          "content": [{"type": "output_text", "text": "Be concise.", "annotations": []}]}]
                    }"#
                    .into(),
                ),
            },
            response_without_usage("resp_final", "Done."),
        ],
    );
    let proxy_addr = start_proxy(advisor_config(upstream_addr)).await;
    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let raw = res.text().await.unwrap();
    assert!(
        !raw.contains("\"iterations\""),
        "empty iterations must be None, not an empty array: {raw}"
    );
    let body: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert!(body["usage"]["iterations"].is_null());
    assert!(
        body["content"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["text"] == "Done.")
    );
}

/// B12: the `advisor_message` iteration carries the explicit
/// `cache_creation: null` field Claude Code sees on the wire (§3.5).
#[tokio::test]
async fn b12_advisor_iteration_has_cache_creation_null() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            advisor_call_response("resp_call", "call_b12"),
            advisor_answer("Be concise."),
            advisor_executor_message("resp_final", "Done."),
        ],
    );
    let proxy_addr = start_proxy(advisor_config(upstream_addr)).await;
    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let raw = res.text().await.unwrap();
    assert!(raw.contains("\"cache_creation\":null"), "{raw}");
    let body: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let iterations = body["usage"]["iterations"].as_array().unwrap();
    let advisor = iterations
        .iter()
        .find(|i| i["type"] == "advisor_message")
        .unwrap();
    assert!(advisor["cache_creation"].is_null());
    assert_eq!(advisor["cache_creation_input_tokens"], 0);
}

/// HARD GATE: `enabled:false` with NO advisor declaration is byte-for-byte
/// identical to the pre-advisor (`17f8329`) behaviour for ordinary
/// non-streaming requests. The B9 post-translation strip is a no-op here
/// (translate.rs already removed the section when no advisor tool exists).
#[tokio::test]
async fn b9_disabled_without_declaration_is_still_byte_identical() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned.lock().await = Some(completed_response_with_id("resp_gate", "same answer"));
    let config = make_proxy_config(upstream_addr);
    assert!(!config.advisor.enabled);
    let proxy_addr = start_proxy(config).await;

    let request = advisor_request_with_decl(
        false,
        r#"{"name":"Bash","description":"run","input_schema":{"type":"object","properties":{}}}"#,
    );
    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(request))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.json::<serde_json::Value>().await.unwrap(),
        serde_json::json!({
            "id": "msg_resp_gate",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "same answer"}],
            "model": "gpt-4o-mini",
            "stop_reason": "end_turn",
            "stop_sequence": null,
            "usage": {"input_tokens": 7, "output_tokens": 3}
        })
    );
    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(sent.len(), 1, "no extra POST");
    let v: serde_json::Value = serde_json::from_slice(&sent[0]).unwrap();
    assert_eq!(v["stream"], false);
    assert!(v["tools"].as_array().unwrap().len() == 1);
    assert_eq!(v["tools"][0]["name"], "Bash");
    assert!(
        !v["instructions"]
            .as_str()
            .unwrap()
            .contains("Call advisor BEFORE substantive work"),
        "the pre-existing strip already removed the section"
    );
    assert!(
        v["instructions"]
            .as_str()
            .unwrap()
            .contains("<system-reminder>skills</system-reminder>")
    );
}

// ─── Two-section instruction-strip regressions (byte identity + B9) ─────────
//
// `17f8329` stripped only the FIRST line-start `# Advisor Tool` section.
// A post-translation strip must NEVER run on the ordinary no-declaration
// path: that is what deleted a SECOND unrelated section in `ddac5de`.

/// Two `# Advisor Tool` sections, separated by `<system-reminder>`.
const TWO_ADVISOR_SECTIONS: &str = "Git status: clean\n\n\
# Advisor Tool\n\nCall advisor ONE.\n\n\
<system-reminder>skills</system-reminder>\n\n\
# Advisor Tool\n\nCall advisor TWO.\n\n\
<system-reminder>agents</system-reminder>";

fn two_section_request(stream: bool, declaration: &str) -> String {
    let declaration: serde_json::Value = serde_json::from_str(declaration).unwrap();
    serde_json::json!({
        "model": "gpt-4o-mini",
        "max_tokens": 64,
        "stream": stream,
        "system": TWO_ADVISOR_SECTIONS,
        "tools": [declaration],
        "messages": [{"role": "user", "content": "Do the task"}],
    })
    .to_string()
}

/// `enabled:false` + NO declaration: the translator strips section ONE once
/// and the proxy does NOT strip again. Section TWO survives, exactly as it
/// did in `17f8329` (byte identity), for both non-streaming and streaming.
#[tokio::test]
async fn two_sections_disabled_no_declaration_preserves_second_section() {
    const BASH_DECL: &str =
        r#"{"name":"Bash","description":"run","input_schema":{"type":"object","properties":{}}}"#;
    for stream in [false, true] {
        let (upstream_addr, upstream) = start_fake_upstream().await;
        if stream {
            let created = serde_json::json!({
                "type": "response.created",
                "response": {"id": "resp_1", "object": "response",
                    "created_at": 0, "status": "in_progress",
                    "model": "gpt-4o", "output": []}
            })
            .to_string();
            let completed = serde_json::json!({
                "type": "response.completed",
                "response": {"id": "resp_1", "object": "response",
                    "created_at": 0, "status": "completed",
                    "model": "gpt-4o", "output": [],
                    "usage": {"input_tokens": 4, "output_tokens": 2, "total_tokens": 6}}
            })
            .to_string();
            *upstream.canned_stream.lock().await = Some(format!(
                "event: response.created\ndata: {created}\n\n\
                 event: response.completed\ndata: {completed}\n\n"
            ));
        } else {
            *upstream.canned.lock().await =
                Some(completed_response_with_id("resp_two_sec", "same answer"));
        }
        let config = make_proxy_config(upstream_addr);
        assert!(!config.advisor.enabled);
        let proxy_addr = start_proxy(config).await;
        let res = reqwest::Client::new()
            .post(format!("http://{proxy_addr}/v1/messages"))
            .header("content-type", "application/json")
            .body(ReqBody::from(two_section_request(stream, BASH_DECL)))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "stream={stream}");
        if stream {
            assert_eq!(
                res.headers()
                    .get(axum::http::header::CONTENT_TYPE)
                    .unwrap()
                    .to_str()
                    .unwrap(),
                "text/event-stream"
            );
            let _ = res.text().await.unwrap();
        } else {
            assert_eq!(
                res.json::<serde_json::Value>().await.unwrap(),
                serde_json::json!({
                    "id": "msg_resp_two_sec",
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "text", "text": "same answer"}],
                    "model": "gpt-4o-mini",
                    "stop_reason": "end_turn",
                    "stop_sequence": null,
                    "usage": {"input_tokens": 7, "output_tokens": 3}
                })
            );
        }
        let sent = upstream.received_all.lock().await.clone();
        assert_eq!(sent.len(), 1, "stream={stream}: no extra POST");
        let v: serde_json::Value = serde_json::from_slice(&sent[0]).unwrap();
        let instructions = v["instructions"].as_str().unwrap();
        assert!(
            !instructions.contains("Call advisor ONE"),
            "stream={stream}: section ONE was stripped once"
        );
        assert!(
            instructions.contains("# Advisor Tool\n\nCall advisor TWO."),
            "stream={stream}: section TWO survives, byte-identical to 17f8329: {instructions:?}"
        );
        assert!(instructions.starts_with("Git status: clean\n\n<system-reminder>skills"));
        assert!(instructions.contains("<system-reminder>agents</system-reminder>"));
        assert_eq!(v["stream"], stream);
        assert_eq!(v["tools"][0]["name"], "Bash");
    }
}

/// `enabled:false` + schema-less declaration (B9): the translator skips its
/// strip, so `build_outbound` applies ONE strip. Section ONE is removed; the
/// second section is defined to survive (matching the single-strip contract).
#[tokio::test]
async fn two_sections_disabled_schema_less_decl_strips_only_first() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned.lock().await = Some(completed_response_with_id("resp_b9_two", "ok"));
    let config = make_proxy_config(upstream_addr);
    assert!(!config.advisor.enabled);
    let proxy_addr = start_proxy(config).await;
    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(two_section_request(
            false,
            ADVISOR_DECLARATION,
        )))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(sent.len(), 1, "B9: no extra POST");
    let v: serde_json::Value = serde_json::from_slice(&sent[0]).unwrap();
    let instructions = v["instructions"].as_str().unwrap();
    assert!(
        !instructions.contains("Call advisor ONE"),
        "no orphan tool instructions from section ONE: {instructions:?}"
    );
    assert!(
        instructions.contains("Call advisor TWO"),
        "exactly one strip; section TWO survives"
    );
    assert!(instructions.contains("<system-reminder>skills</system-reminder>"));
    assert!(instructions.contains("<system-reminder>agents</system-reminder>"));
    assert!(
        !v["tools"]
            .as_array()
            .unwrap_or(&Vec::new())
            .iter()
            .any(|t| t["name"] == "advisor"),
        "schema-less declaration is dropped upstream"
    );
}

/// `enabled:true` + real declaration: the advisor pipeline is active, so
/// BOTH sections are preserved for the synthesized tool.
#[tokio::test]
async fn two_sections_enabled_declaration_preserves_both_sections() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned.lock().await = Some(completed_response_with_id("resp_active", "Done."));
    let proxy_addr = start_proxy(advisor_config(upstream_addr)).await;
    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(two_section_request(
            false,
            ADVISOR_DECLARATION,
        )))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(sent.len(), 1, "no consultation without an advisor call");
    let v: serde_json::Value = serde_json::from_slice(&sent[0]).unwrap();
    let instructions = v["instructions"].as_str().unwrap();
    assert_eq!(
        instructions, TWO_ADVISOR_SECTIONS,
        "active advisor keeps both sections byte-for-byte"
    );
    assert_eq!(
        v["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|t| t["name"] == "advisor")
            .count(),
        1,
        "exactly one synthesized advisor tool"
    );
}

/// Run one advisor turn against a scripted upstream with `max_uses` set;
/// returns the client's response body and every POST the upstream received.
async fn advisor_turn_with_max_uses(
    max_uses: u32,
    responses: Vec<FakeResponse>,
) -> (serde_json::Value, Vec<serde_json::Value>) {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(&upstream, responses);
    let mut config = (*advisor_config(upstream_addr)).clone();
    config.advisor.max_uses = Some(max_uses);
    let proxy_addr = start_proxy(Arc::new(config)).await;
    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = res.json().await.unwrap();
    let sent = upstream.received_all.lock().await.clone();
    let bodies = sent
        .iter()
        .map(|b| serde_json::from_slice(b).unwrap())
        .collect();
    (body, bodies)
}

fn upstream_failure(status: StatusCode) -> FakeResponse {
    FakeResponse {
        status,
        body: Some(r#"{"error":{"message":"advisor trouble"}}"#.into()),
    }
}

/// A transient failure (503) is retried once, and the retry's advice is used.
#[tokio::test]
async fn an_advisor_failure_that_a_retry_fixes_still_gives_advice() {
    let (body, sent) = advisor_turn_with_max_uses(
        2,
        vec![
            advisor_call_response("resp_call", "call_retry"),
            upstream_failure(StatusCode::SERVICE_UNAVAILABLE),
            advisor_answer("Use a channel."),
            advisor_executor_message("resp_final", "Done."),
        ],
    )
    .await;
    let results = advisor_results(&body);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["content"]["text"], "Use a channel.");
    assert_eq!(sent.len(), 4, "executor, advisor, its retry, executor");
    assert_eq!(sent[1]["model"], "beam");
    assert_eq!(sent[2]["model"], "beam");
}

/// A permanent failure (a 4xx) is NOT retried: it would fail the same way.
#[tokio::test]
async fn a_permanent_advisor_failure_is_not_retried() {
    let (body, sent) = advisor_turn_with_max_uses(
        2,
        vec![
            advisor_call_response("resp_call", "call_perm"),
            upstream_failure(StatusCode::UNPROCESSABLE_ENTITY),
            advisor_executor_message("resp_final", "Done."),
        ],
    )
    .await;
    assert_eq!(
        advisor_results(&body)[0]["content"]["error_code"],
        "unavailable"
    );
    assert_eq!(sent.len(), 3, "executor, ONE advisor POST, executor");
}

/// A consultation that failed does not use up `max_uses`: with a limit of 1,
/// a failed first consultation still leaves the second one available.
#[tokio::test]
async fn a_failed_consultation_does_not_use_up_max_uses() {
    let (body, _) = advisor_turn_with_max_uses(
        1,
        vec![
            advisor_call_response("resp_c1", "call_f1"),
            upstream_failure(StatusCode::UNPROCESSABLE_ENTITY),
            advisor_call_response("resp_c2", "call_f2"),
            advisor_answer("Second time lucky."),
            advisor_executor_message("resp_final", "Done."),
        ],
    )
    .await;
    let results = advisor_results(&body);
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["content"]["error_code"], "unavailable");
    assert_eq!(results[1]["content"]["text"], "Second time lucky.");
    assert!(
        results
            .iter()
            .all(|r| r["content"]["error_code"] != "max_uses_exceeded"),
        "the failure was not counted against max_uses"
    );
}

/// Failures are not free: after two failed consultations the advisor is
/// treated as down for the rest of the turn. The executor is told so, and a
/// further call to it is dropped without another advisor POST.
#[tokio::test]
async fn a_broken_advisor_is_given_up_on_after_two_failures() {
    let (body, sent) = advisor_turn_with_max_uses(
        5,
        vec![
            advisor_call_response("resp_c1", "call_g1"),
            upstream_failure(StatusCode::UNPROCESSABLE_ENTITY),
            advisor_call_response("resp_c2", "call_g2"),
            upstream_failure(StatusCode::UNPROCESSABLE_ENTITY),
            advisor_executor_message("resp_final", "Answer without advice."),
        ],
    )
    .await;
    assert_eq!(advisor_results(&body).len(), 2, "two failed consultations");
    assert!(
        body["content"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["text"] == "Answer without advice."),
        "the turn ends with the executor's answer"
    );
    // The executor's last run was told the advisor is gone for good.
    let last = sent.last().unwrap().to_string();
    assert!(last.contains("will not be consulted again"), "{last}");
    // A first failure was told only that it is temporary.
    assert!(sent[2].to_string().contains("request to it was refused"));
}

/// An executor that ignores that and calls the advisor a third time gets no
/// third advisor POST.
#[tokio::test]
async fn a_third_call_to_a_given_up_advisor_is_dropped() {
    let (_, sent) = advisor_turn_with_max_uses(
        5,
        vec![
            advisor_call_response("resp_c1", "call_h1"),
            upstream_failure(StatusCode::UNPROCESSABLE_ENTITY),
            advisor_call_response("resp_c2", "call_h2"),
            upstream_failure(StatusCode::UNPROCESSABLE_ENTITY),
            advisor_call_response("resp_c3", "call_h3"),
            advisor_executor_message("resp_final", "Never needed."),
        ],
    )
    .await;
    let advisor_posts = sent.iter().filter(|b| b["model"] == "beam").count();
    assert_eq!(advisor_posts, 2, "no advisor POST for the third call");
}

/// Each kind of advisor failure reaches the client as its own known error
/// code and tells the executor its own reason.
#[tokio::test]
async fn advisor_failures_report_their_own_code_and_reason() {
    let cases = [
        (
            FakeResponse {
                status: StatusCode::BAD_REQUEST,
                body: Some(
                    r#"{"error":{"message":"The prompt is too long: 1424454, model maximum context length: 1048576"}}"#
                        .into(),
                ),
            },
            1,
            "prompt_too_long",
            "too long for it",
        ),
        (
            upstream_failure(StatusCode::TOO_MANY_REQUESTS),
            2,
            "too_many_requests",
            "rate limited",
        ),
        (
            upstream_failure(StatusCode::BAD_GATEWAY),
            2,
            "overloaded",
            "overloaded or unreachable",
        ),
    ];
    for (failure, advisor_attempts, code, reason) in cases {
        // A too-long prompt is permanent (one attempt); 429 and 5xx are
        // retried once, so the retry fails the same way.
        let mut script = vec![advisor_call_response("resp_call", "call_kind")];
        script.extend((0..advisor_attempts).map(|_| FakeResponse {
            status: failure.status,
            body: failure.body.clone(),
        }));
        script.push(advisor_executor_message("resp_final", "Done."));
        let (body, sent) = advisor_turn_with_max_uses(2, script).await;
        assert_eq!(advisor_results(&body)[0]["content"]["error_code"], code);
        let told = sent.last().unwrap().to_string();
        assert!(told.contains(reason), "{code}: {told}");
        assert!(told.contains("without a second opinion"), "{code}: {told}");
    }
}
