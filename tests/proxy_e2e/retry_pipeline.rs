//! The proxy's retry loop: unsupported-parameter stripping,
//! retry ordering, and usage/stats attribution.

use crate::harness::*;

/// An upstream that rejects one optional parameter (here `temperature`)
/// must be retried once without it, and the fact must be remembered for
/// the rest of the process so later requests never send it again.
///
/// Asserts on the exact bodies the fake upstream received, so it proves
/// both halves of the feature: the one-shot retry, and the latch that
/// keeps the doomed parameter off the wire afterwards.
#[tokio::test]
async fn unsupported_parameter_retries_without_it_and_remembers() {
    let (upstream_addr, upstream) = start_fake_upstream().await;

    let ok_body = || {
        r#"{
            "id": "resp_ok",
            "object": "response",
            "created_at": 1,
            "status": "completed",
            "model": "gpt-6-astra",
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "ok", "annotations": []}]
            }],
            "usage": {"input_tokens":1,"output_tokens":1,"total_tokens":2}
        }"#
        .to_string()
    };

    // First upstream call rejects `temperature`; the second succeeds.
    // Anything past that (the third call, which exercises the latch)
    // falls through to `canned`.
    *upstream.canned_per_attempt.lock().await = VecDeque::from(vec![
        FakeResponse {
            status: StatusCode::BAD_REQUEST,
            body: Some(
                r#"{"message":"Unsupported parameter: temperature is not supported with this model.","type":"invalid_request_error","code":"unsupported_parameter","param":"temperature"}"#.into(),
            ),
        },
        FakeResponse {
            status: StatusCode::OK,
            body: Some(ok_body()),
        },
    ]);
    *upstream.canned.lock().await = Some(ok_body());

    let config = make_proxy_config(upstream_addr);
    let proxy_addr = start_proxy(config).await;
    let client = reqwest::Client::new();

    let request = r#"{"model":"gpt-6-astra","max_tokens":4,"temperature":0.7,"top_p":0.9,"messages":[{"role":"user","content":"a"}]}"#;

    // First client request: sent optimistically, rejected, retried.
    let res = client
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(request))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(sent.len(), 2, "expected exactly one parameter-strip retry");
    let first: serde_json::Value = serde_json::from_slice(&sent[0]).unwrap();
    let second: serde_json::Value = serde_json::from_slice(&sent[1]).unwrap();
    assert!(
        first.get("temperature").is_some(),
        "the optimistic first attempt must carry the parameter"
    );
    assert!(
        second.get("temperature").is_none(),
        "the retry must drop it"
    );
    assert!(
        second.get("top_p").is_some(),
        "only the named parameter is dropped; top_p must survive"
    );

    // Second client request, same model: the latch means the 400 is not
    // provoked again -- `temperature` is never put on the wire.
    let res = client
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(request))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(
        sent.len(),
        3,
        "a learned fact must avoid a second failed attempt"
    );
    let third: serde_json::Value = serde_json::from_slice(&sent[2]).unwrap();
    assert!(
        third.get("temperature").is_none(),
        "the learned fact must be applied before the first send"
    );
    assert!(
        third.get("top_p").is_some(),
        "unrelated parameters stay intact"
    );
}

/// A 400 that names a parameter but is a *value* problem, not a support
/// problem, must pass straight through: no retry, one upstream call.
/// This is the guard that stops the proxy silently stripping a
/// parameter the model actually honours.
#[tokio::test]
async fn invalid_parameter_value_is_not_treated_as_unsupported() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned_error.lock().await = Some((
        StatusCode::BAD_REQUEST,
        r#"{"message":"Invalid value for 'temperature': temperature must be between 0 and 2.","type":"invalid_request_error","code":"invalid_request_error","param":"temperature"}"#.to_string(),
    ));

    let config = make_proxy_config(upstream_addr);
    let proxy_addr = start_proxy(config).await;
    let client = reqwest::Client::new();

    let res = client
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(
            r#"{"model":"gpt-5.4-mini","max_tokens":4,"temperature":9.5,"messages":[{"role":"user","content":"a"}]}"#,
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        upstream.received_all.lock().await.len(),
        1,
        "a value error must not trigger a parameter-strip retry"
    );
}

/// The real-world always-reasoning shape: the upstream rejects
/// `temperature` and then `top_p` on consecutive attempts. The proxy
/// must strip each in turn and still deliver a successful response --
/// a single shared retry budget would fail this request.
#[tokio::test]
async fn rejects_both_sampling_params_one_at_a_time() {
    let (upstream_addr, upstream) = start_fake_upstream().await;

    let ok_body = || {
        r#"{
            "id": "resp_ok",
            "object": "response",
            "created_at": 1,
            "status": "completed",
            "model": "gpt-6-astra",
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "ok", "annotations": []}]
            }],
            "usage": {"input_tokens":1,"output_tokens":1,"total_tokens":2}
        }"#
        .to_string()
    };

    *upstream.canned_per_attempt.lock().await = VecDeque::from(vec![
        FakeResponse {
            status: StatusCode::BAD_REQUEST,
            body: Some(
                r#"{"message":"Unsupported parameter: temperature","code":"unsupported_parameter","param":"temperature"}"#.into(),
            ),
        },
        FakeResponse {
            status: StatusCode::BAD_REQUEST,
            body: Some(
                r#"{"message":"Unsupported parameter: top_p","code":"unsupported_parameter","param":"top_p"}"#.into(),
            ),
        },
        FakeResponse {
            status: StatusCode::OK,
            body: Some(ok_body()),
        },
    ]);

    let config = make_proxy_config(upstream_addr);
    let proxy_addr = start_proxy(config).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(
            r#"{"model":"gpt-6-astra","max_tokens":4,"temperature":0.7,"top_p":0.9,"messages":[{"role":"user","content":"a"}]}"#,
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(sent.len(), 3, "expected one retry per rejected parameter");
    let bodies: Vec<serde_json::Value> = sent
        .iter()
        .map(|b| serde_json::from_slice(b).unwrap())
        .collect();
    assert!(bodies[0].get("temperature").is_some() && bodies[0].get("top_p").is_some());
    assert!(bodies[1].get("temperature").is_none() && bodies[1].get("top_p").is_some());
    assert!(bodies[2].get("temperature").is_none() && bodies[2].get("top_p").is_none());
}

// ─── Phase-1 characterization tests ────────────────────────────────────────
//
// These pin the exact retry order, response attribution and streaming Drop
// behaviour of the request pipeline *before* it is reshaped into the reusable
// `Attempt` unit. All of them use the fake upstream above; none call a real API.

/// A shared non-streaming success body with authoritative usage.
fn completed_response(text: &str, input_tokens: u64, output_tokens: u64) -> String {
    format!(
        r#"{{
            "id": "resp_ok",
            "object": "response",
            "created_at": 1,
            "status": "completed",
            "model": "gpt-4o-mini",
            "output": [{{
                "type": "message",
                "role": "assistant",
                "content": [{{"type": "output_text", "text": "{text}", "annotations": []}}]
            }}],
            "usage": {{"input_tokens":{input_tokens},"output_tokens":{output_tokens},"total_tokens":{}}}
        }}"#,
        input_tokens + output_tokens
    )
}

/// Retry ORDER matters: an unsupported-parameter 400 strips the named
/// parameter, a following model rejection re-translates the *whole* request
/// for `default_model` (recomputing reasoning/caching/replay), and only then
/// does the succeeded request reach the client. This asserts the exact JSON
/// bodies the fake upstream received, in order — a change to any of the
/// three policies, or to the retry order, shows up here.
#[tokio::test]
async fn retry_order_matrix_records_exact_upstream_bodies() {
    use openai_to_anthropic_proxy::config::ReasoningReplayMode;

    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned_per_attempt.lock().await = VecDeque::from(vec![
        FakeResponse {
            status: StatusCode::BAD_REQUEST,
            body: Some(
                r#"{"message":"Unsupported parameter: temperature","code":"unsupported_parameter","param":"temperature"}"#.into(),
            ),
        },
        FakeResponse {
            status: StatusCode::BAD_REQUEST,
            body: Some(
                r#"{"message":"model not supported","type":"invalid_request_error","code":"model_not_supported"}"#.into(),
            ),
        },
        FakeResponse {
            status: StatusCode::OK,
            body: Some(completed_response("matrix ok", 7, 3)),
        },
    ]);

    let config = Arc::new(Config {
        model_info: Default::default(),
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        upstream_base_url: format!("http://{upstream_addr}"),
        upstream_api_key: "test-key".into(),
        upstream_path: "/v1/responses".into(),
        request_timeout: Duration::from_secs(10),
        max_request_body_bytes: openai_to_anthropic_proxy::config::DEFAULT_MAX_REQUEST_BODY_BYTES,
        reasoning_effort: None,
        reasoning: ReasoningConfig {
            default: Some("low".into()),
            models: BTreeMap::from([("gpt-4o-mini".into(), "none".into())]),
            ..ReasoningConfig::default()
        },
        reasoning_replay: openai_to_anthropic_proxy::config::ReasoningReplayConfig {
            default: ReasoningReplayMode::Text,
            models: BTreeMap::from([("gpt-4o-mini".into(), ReasoningReplayMode::Off)]),
            max_chars: 0,
        },
        model_aliases: ModelAliases {
            map: BTreeMap::new(),
            default_model: Some("gpt-4o-mini".into()),
        },
        prompt_caching: openai_to_anthropic_proxy::config::PromptCachingConfig {
            models: vec!["claude-sonnet-5".into()],
            cache_key: Some("cache-key".into()),
        },
        log_to_disk: false,
        proxy_key: None,
        advisor: AdvisorConfig::default(),
        web_fetch: WebFetchConfig::default(),
        web_search: WebSearchConfig::default(),
        server_tools: ServerToolsConfig::default(),
        prefix_models_for_discovery: false,
        tool_result_images: ToolResultImages::default(),
        forward_documents: false,
        min_output_tokens: openai_to_anthropic_proxy::config::DEFAULT_MIN_OUTPUT_TOKENS,
        max_concurrent_requests: 0,
        rate_limit_retries: 0,
        system_message_placement: Default::default(),
        warnings: Vec::new(),
    });
    let proxy_addr = start_proxy(config).await;

    // Carries a thinking block, a cache_control hint and both sampling
    // parameters so every recomputed policy has an observable field.
    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(
            r#"{
                "model":"claude-sonnet-5",
                "max_tokens":16,
                "temperature":0.7,
                "top_p":0.9,
                "messages":[
                    {"role":"user","content":[{"type":"text","text":"cached","cache_control":{"type":"ephemeral"}}]},
                    {"role":"assistant","content":[
                        {"type":"thinking","thinking":"the model reasoned about this","signature":"SIG"},
                        {"type":"text","text":"the answer"}
                    ]}
                ]
            }"#,
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["content"][0]["text"], "matrix ok");

    let sent = upstream.received_all.lock().await.clone();
    let bodies: Vec<serde_json::Value> = sent
        .iter()
        .map(|b| serde_json::from_slice(b).unwrap())
        .collect();
    assert_eq!(bodies.len(), 3, "unsupported-param → fallback → success");

    // Attempt 1: primary model, both sampling params, primary policies.
    let first = &bodies[0];
    assert_eq!(first["model"], "claude-sonnet-5");
    assert_eq!(first["temperature"], 0.7);
    assert_eq!(first["top_p"], 0.9);
    assert_eq!(first["reasoning"]["effort"], "low");
    assert_eq!(first["prompt_cache_key"], "cache-key");
    assert!(first["input"][0]["content"][0]["prompt_cache_breakpoint"].is_object());
    assert!(
        first["input"][1]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("the model reasoned about this")
    );

    // Attempt 2: only `temperature` was stripped for the parameter retry.
    let second = &bodies[1];
    assert_eq!(second["model"], "claude-sonnet-5");
    assert!(second.get("temperature").is_none());
    assert_eq!(second["top_p"], 0.9);
    assert_eq!(second["reasoning"]["effort"], "low");
    assert_eq!(second["prompt_cache_key"], "cache-key");

    // Attempt 3: a full re-translation for the fallback model. The rebuilt
    // request carries both sampling params again (the fallback model has no
    // learned facts), and its own reasoning/caching/replay policies.
    let third = &bodies[2];
    assert_eq!(third["model"], "gpt-4o-mini");
    assert_eq!(third["temperature"], 0.7);
    assert_eq!(third["top_p"], 0.9);
    assert_eq!(third["reasoning"]["effort"], "none");
    assert!(third.get("prompt_cache_key").is_none());
    assert!(
        third["input"][0]["content"][0]
            .get("prompt_cache_breakpoint")
            .is_none()
    );
    assert_eq!(third["input"][1]["content"][0]["text"], "the answer");
}

/// After a model fallback, the hidden validation/corrective retry still runs
/// on the *fallback* request. If the corrective retry's upstream call fails,
/// the first successful (fallback) response must be kept — not mapped into
/// an error, and not re-translated for a third time.
#[tokio::test]
async fn corrective_retry_after_fallback_keeps_first_success_on_retry_error() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    let bad_call = r#"{
        "id": "resp_bad",
        "object": "response",
        "created_at": 1,
        "status": "completed",
        "model": "gpt-4o-mini",
        "output": [{
            "type": "function_call",
            "id": "fc_1",
            "status": "completed",
            "call_id": "call_1",
            "name": "Workflow",
            "arguments": "{\"resumeFromRunId\":\"bogus\"}"
        }],
        "usage": {"input_tokens":1,"output_tokens":1,"total_tokens":2}
    }"#;
    *upstream.canned_per_attempt.lock().await = VecDeque::from(vec![
        FakeResponse {
            status: StatusCode::BAD_REQUEST,
            body: Some(
                r#"{"message":"model not supported","type":"invalid_request_error","code":"model_not_supported"}"#.into(),
            ),
        },
        FakeResponse {
            status: StatusCode::OK,
            body: Some(bad_call.into()),
        },
        FakeResponse {
            status: StatusCode::BAD_REQUEST,
            body: Some(r#"{"message":"corrective retry failed"}"#.into()),
        },
    ]);

    let proxy_addr = start_proxy(make_config_with_default_model(
        upstream_addr,
        BTreeMap::new(),
        Some("gpt-4o-mini"),
    ))
    .await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(
            r#"{
                "model": "claude-sonnet-5",
                "max_tokens": 64,
                "messages": [{"role": "user", "content": "run it"}],
                "tools": [{
                    "name": "Workflow",
                    "description": "Run a workflow",
                    "input_schema": {
                        "type": "object",
                        "properties": {"script": {"type": "string"}},
                        "required": ["script"]
                    }
                }]
            }"#,
        ))
        .send()
        .await
        .unwrap();

    // The first success wins: the client sees the invalid-but-repaired call
    // (not an error), even though the corrective retry's upstream 400 came
    // later.
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["content"][0]["id"], "call_1");

    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(sent.len(), 3, "fallback + one bounded corrective retry");
    let second: serde_json::Value = serde_json::from_slice(&sent[1]).unwrap();
    let third: serde_json::Value = serde_json::from_slice(&sent[2]).unwrap();
    assert_eq!(second["model"], "gpt-4o-mini");
    assert_eq!(third["model"], "gpt-4o-mini");
    let corrective = third["input"].as_array().unwrap().iter().any(|i| {
        i["type"] == "function_call_output"
            && i["output"]
                .as_str()
                .is_some_and(|o| o.contains("[TOOL_ERROR]"))
    });
    assert!(corrective, "the retry must carry the corrective input item");
}

/// A fallback completion is attributed exactly once, to the *actual* model
/// that produced it, and the response log line is emitted once.
#[tokio::test]
async fn fallback_usage_attribution_is_recorded_once() {
    use openai_to_anthropic_proxy::tui::LogKind;
    use openai_to_anthropic_proxy::{SessionStatsStore, TokenTotals};

    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned_per_attempt.lock().await = VecDeque::from(vec![
        FakeResponse {
            status: StatusCode::NOT_FOUND,
            body: Some(REAL_GATEWAY_MODEL_404.into()),
        },
        FakeResponse {
            status: StatusCode::OK,
            body: Some(completed_response("attributed", 11, 5)),
        },
    ]);

    let stats = Arc::new(SessionStatsStore::new());
    let (output, log_rx) = openai_to_anthropic_proxy::OutputSink::tui();
    let config =
        make_config_with_default_model(upstream_addr, BTreeMap::new(), Some("gpt-4o-mini"));
    let proxy_addr = start_proxy_with_captured_stores(config, stats.clone(), output).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(
            r#"{"model":"claude-sonnet-5","max_tokens":4,"messages":[{"role":"user","content":"a"}]}"#,
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let snapshot = stats.snapshot_sections();
    assert!(snapshot.inbound.is_empty(), "no inbound-model attribution");
    assert_eq!(snapshot.actual.len(), 1, "one actual-model bucket");
    assert_eq!(
        snapshot.actual.get("gpt-4o-mini"),
        Some(&TokenTotals {
            requests: 1,
            input_tokens: 11,
            output_tokens: 5,
            ..TokenTotals::default()
        })
    );

    let lines: Vec<_> = std::iter::from_fn(|| log_rx.try_recv().ok()).collect();
    let responses: Vec<_> = lines
        .iter()
        .filter(|l| l.kind == LogKind::Response)
        .collect();
    assert_eq!(responses.len(), 1, "exactly one response log line");
    assert!(responses[0].text.contains("11 in"));
    assert!(responses[0].text.contains("5 out"));
}

/// A stream that reaches its terminal event records session stats exactly
/// once — even if the client disconnects immediately after reading the
/// terminal bytes. This is the `TranslatorStream::Drop` contract.
#[tokio::test]
async fn streaming_terminal_event_records_stats_once() {
    use openai_to_anthropic_proxy::SessionStatsStore;

    let (upstream_addr, upstream) = start_fake_upstream().await;
    let sse = [
        r#"event: response.created
data: {"type":"response.created","response":{"id":"resp_1","object":"response","created_at":0,"status":"in_progress","model":"gpt-4o","output":[]}}"#,
        r#"event: response.output_item.added
data: {"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_x","status":"in_progress","role":"assistant","content":[]}}"#,
        r#"event: response.output_text.delta
data: {"type":"response.output_text.delta","item_id":"msg_x","output_index":0,"content_index":0,"delta":"Hi"}"#,
        r#"event: response.completed
data: {"type":"response.completed","response":{"id":"resp_1","object":"response","created_at":0,"status":"completed","model":"gpt-4o","output":[],"usage":{"input_tokens":4,"output_tokens":2,"total_tokens":6}}}"#,
    ]
    .join("\n\n")
        + "\n\n";
    *upstream.canned_stream.lock().await = Some(sse);

    let stats = Arc::new(SessionStatsStore::new());
    let proxy_addr = start_proxy_with_captured_stores(
        make_proxy_config(upstream_addr),
        stats.clone(),
        openai_to_anthropic_proxy::OutputSink::tui().0,
    )
    .await;

    let mut res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(
            r#"{"model":"gpt-4o","max_tokens":16,"stream":true,"messages":[{"role":"user","content":"a"}]}"#,
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    // Client disconnect: read only the first chunk, then drop the response.
    // The proxy's Drop hook must still have captured the terminal usage.
    let _chunk = res.chunk().await.unwrap();
    drop(res);

    // The upstream SSE body is finite; polling until Drop has run is enough
    // to make the assertion deterministic without sleeping on wall time.
    for _ in 0..100 {
        let totals = stats.snapshot_sections();
        if !totals.inbound.is_empty() {
            break;
        }
        tokio::task::yield_now().await;
    }
    let totals = stats.snapshot_sections();
    assert!(totals.actual.is_empty(), "no fallback was used");
    assert_eq!(totals.inbound.len(), 1);
    let usage = &totals.inbound["gpt-4o"];
    assert_eq!(usage.requests, 1, "terminal event recorded exactly once");
    assert_eq!(usage.input_tokens, 4);
    assert_eq!(usage.output_tokens, 2);
}
