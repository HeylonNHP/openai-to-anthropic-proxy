//! Upstream model-rejection detection and the `default_model`
//! fallback (mirrors `src/model_rejection.rs`).

use crate::harness::*;

/// The same rejection as the gateway sent it on 2026-08-16 — **single**
/// quotes, and no "Try" hint.
const REAL_GATEWAY_MODEL_404_SINGLE_QUOTED: &str = r#"{"error":{"message":"model 'claude-opus-5' not found","type":"not_found_error","param":null,"code":null}}"#;

/// A real non-model upstream error (2026-09-29). It contains the words
/// "not supported", so a detector that matched those loosely would fire
/// the model fallback on a *tool* rejection.
const REAL_GATEWAY_WEB_SEARCH_400: &str = r#"{"error":{"message":"the web_search tool is not supported (ref: 490f64b3-1e45-4671-aaef-8e7ff3a71f4b)","type":"invalid_request_error","param":null,"code":null}}"#;

/// When the upstream rejects a model with a "not supported" error
/// AND a `default_model` is configured, the proxy must retry the
/// request once with the fallback model and surface the second
/// response. This is the safety net that keeps a workflow moving
/// when Claude Code subagents request a model the gateway doesn't
/// recognize.
#[tokio::test]
async fn model_not_supported_falls_back_to_default() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    // First call: airia-style "model not supported" rejection.
    // Second call: a normal Responses response, as if the
    // fallback model were valid.
    *upstream.canned_per_attempt.lock().await = VecDeque::from(vec![
        FakeResponse {
            // The real gateway rejection, verbatim.
            status: StatusCode::NOT_FOUND,
            body: Some(REAL_GATEWAY_MODEL_404.into()),
        },
        FakeResponse {
            status: StatusCode::OK,
            body: Some(
                r#"{
                    "id": "resp_ok",
                    "object": "response",
                    "created_at": 1,
                    "status": "completed",
                    "model": "gpt-4o-mini",
                    "output": [{
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": "fallback ok", "annotations": []}]
                    }],
                    "usage": {"input_tokens":1,"output_tokens":1,"total_tokens":2}
                }"#
                .into(),
            ),
        },
    ]);

    let config = Arc::new(Config {
        model_info: Default::default(),
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        upstream_base_url: format!("http://{upstream_addr}"),
        upstream_api_key: "sk-fake".into(),
        upstream_path: "/v1/responses".into(),
        request_timeout: Duration::from_secs(10),
        max_request_body_bytes: openai_to_anthropic_proxy::config::DEFAULT_MAX_REQUEST_BODY_BYTES,
        reasoning_effort: Some("none".into()),
        reasoning: Default::default(),
        model_aliases: ModelAliases {
            map: BTreeMap::new(),
            default_model: Some("gpt-4o-mini".into()),
        },
        prompt_caching: Default::default(),
        reasoning_replay: Default::default(),
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
        max_retry_wait: Duration::from_secs(60),
        system_message_placement: Default::default(),
        warnings: Vec::new(),
    });
    let proxy_addr = start_proxy(config).await;

    let client = reqwest::Client::new();
    let res = client
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(
            r#"{"model":"claude-sonnet-5","max_tokens":4,"messages":[{"role":"user","content":"a"}]}"#,
        ))
        .send()
        .await
        .unwrap();

    // Client sees the fallback response, not the upstream rejection.
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["content"][0]["text"], "fallback ok");

    // Upstream saw exactly two requests — the original and the retry.
    let sent_bodies = upstream.received_all.lock().await.clone();
    assert_eq!(sent_bodies.len(), 2, "expected exactly one retry");
    let first: serde_json::Value = serde_json::from_slice(&sent_bodies[0]).unwrap();
    let second: serde_json::Value = serde_json::from_slice(&sent_bodies[1]).unwrap();
    assert_eq!(first["model"], "claude-sonnet-5");
    assert_eq!(second["model"], "gpt-4o-mini");
}

/// Without a `default_model` configured, an upstream "model not
/// supported" rejection must pass through to the client unchanged.
/// The fallback path is opt-in: silently retrying without
/// configuration would mask the operator's misconfiguration.
#[tokio::test]
async fn model_not_supported_without_default_passes_error_through() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned_per_attempt.lock().await = VecDeque::from([FakeResponse {
        status: StatusCode::NOT_FOUND,
        body: Some(REAL_GATEWAY_MODEL_404.into()),
    }]);

    let config = Arc::new(Config {
        model_info: Default::default(),
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        upstream_base_url: format!("http://{upstream_addr}"),
        upstream_api_key: "sk-fake".into(),
        upstream_path: "/v1/responses".into(),
        request_timeout: Duration::from_secs(10),
        max_request_body_bytes: openai_to_anthropic_proxy::config::DEFAULT_MAX_REQUEST_BODY_BYTES,
        reasoning_effort: Some("none".into()),
        reasoning: Default::default(),
        // Note: no default_model — fallback disabled.
        model_aliases: Default::default(),
        prompt_caching: Default::default(),
        reasoning_replay: Default::default(),
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
        max_retry_wait: Duration::from_secs(60),
        system_message_placement: Default::default(),
        warnings: Vec::new(),
    });
    let proxy_addr = start_proxy(config).await;

    let client = reqwest::Client::new();
    let res = client
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(
            r#"{"model":"claude-sonnet-5","max_tokens":4,"messages":[{"role":"user","content":"a"}]}"#,
        ))
        .send()
        .await
        .unwrap();

    // The upstream rejection passes through to the client. We
    // don't fabricate a 502 here — the operator should see the
    // actual upstream status and body.
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    let body: serde_json::Value = res.json().await.unwrap();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("claude-sonnet-5"),
        "expected the upstream message, got {body}"
    );

    // Crucially: only one upstream call, no retry.
    let sent_bodies = upstream.received_all.lock().await.clone();
    assert_eq!(sent_bodies.len(), 1);
}

/// If the upstream returns a 400 for *some other reason* (e.g. a
/// bad parameter), the proxy must not silently fall back to a
/// default model — that would mask the real error. The proxy only
/// retries when the body specifically says the model is wrong.
#[tokio::test]
async fn unrelated_upstream_400_is_not_a_fallback_trigger() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned_per_attempt.lock().await = VecDeque::from([FakeResponse {
        status: StatusCode::BAD_REQUEST,
        body: Some(
            r#"{"message":"max_output_tokens is too high for this endpoint","type":"invalid_request_error"}"#.into(),
        ),
    }]);

    let config = Arc::new(Config {
        model_info: Default::default(),
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        upstream_base_url: format!("http://{upstream_addr}"),
        upstream_api_key: "sk-fake".into(),
        upstream_path: "/v1/responses".into(),
        request_timeout: Duration::from_secs(10),
        max_request_body_bytes: openai_to_anthropic_proxy::config::DEFAULT_MAX_REQUEST_BODY_BYTES,
        reasoning_effort: Some("none".into()),
        reasoning: Default::default(),
        model_aliases: ModelAliases {
            map: BTreeMap::new(),
            default_model: Some("gpt-4o-mini".into()),
        },
        prompt_caching: Default::default(),
        reasoning_replay: Default::default(),
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
        max_retry_wait: Duration::from_secs(60),
        system_message_placement: Default::default(),
        warnings: Vec::new(),
    });
    let proxy_addr = start_proxy(config).await;

    let client = reqwest::Client::new();
    let res = client
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(
            r#"{"model":"gpt-4o","max_tokens":4,"messages":[{"role":"user","content":"a"}]}"#,
        ))
        .send()
        .await
        .unwrap();

    // 400 passes through; no retry.
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let sent_bodies = upstream.received_all.lock().await.clone();
    assert_eq!(sent_bodies.len(), 1, "no retry should have been attempted");
}

/// A model rejection whose name is **single-quoted** — the shape the
/// gateway used on 2026-08-16. A detector that hard-codes double quotes
/// misses every one of those eight events.
#[tokio::test]
async fn model_not_found_with_single_quoted_name_falls_back_to_default() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned_per_attempt.lock().await = VecDeque::from(vec![
        FakeResponse {
            status: StatusCode::NOT_FOUND,
            body: Some(REAL_GATEWAY_MODEL_404_SINGLE_QUOTED.into()),
        },
        FakeResponse {
            status: StatusCode::OK,
            body: Some(
                r#"{
                    "id": "resp_ok",
                    "object": "response",
                    "created_at": 1,
                    "status": "completed",
                    "model": "gpt-4o-mini",
                    "output": [{
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": "fallback ok", "annotations": []}]
                    }],
                    "usage": {"input_tokens":1,"output_tokens":1,"total_tokens":2}
                }"#
                .into(),
            ),
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
            r#"{"model":"claude-opus-5","max_tokens":4,"messages":[{"role":"user","content":"a"}]}"#,
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let sent_bodies = upstream.received_all.lock().await.clone();
    assert_eq!(sent_bodies.len(), 2, "expected exactly one retry");
}

/// A rejection that mentions the model but whose **subject** is a tool
/// or a parameter must NOT trigger the fallback. The fallback answers
/// the request with a *different model*, so firing it on a request-scoped
/// error silently substitutes a model the operator never chose — and
/// masks the real error.
#[tokio::test]
async fn a_request_scoped_error_naming_the_model_is_not_a_fallback_trigger() {
    let bodies = [
        // The tool is the subject; the model is mentioned in passing.
        r#"{"error":{"message":"the web_search tool is not supported for model claude-sonnet-5","type":"invalid_request_error"}}"#,
        // The subject comes first.
        r#"{"error":{"message":"claude-sonnet-5 is not supported for the web_search tool","type":"invalid_request_error"}}"#,
        // A parameter, not the model.
        r#"{"error":{"message":"temperature is not supported by claude-sonnet-5","type":"invalid_request_error"}}"#,
        // A function, not the model.
        r#"{"error":{"message":"function calling is unsupported by claude-sonnet-5","type":"invalid_request_error"}}"#,
        // The model's *output* was not found — not the model.
        r#"{"error":{"message":"the model output is not found","type":"not_found_error"}}"#,
    ];
    for body in bodies {
        let (upstream_addr, upstream) = start_fake_upstream().await;
        *upstream.canned_per_attempt.lock().await = VecDeque::from([FakeResponse {
            status: StatusCode::BAD_REQUEST,
            body: Some(body.into()),
        }]);
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
                r#"{"model":"claude-sonnet-5","max_tokens":4,"messages":[{"role":"user","content":"a"}]}"#,
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "body was {body}");
        let sent_bodies = upstream.received_all.lock().await.clone();
        assert_eq!(
            sent_bodies.len(),
            1,
            "a request-scoped error must not trigger the model fallback: {body}"
        );
    }
}

/// `default_model` may be written as an **inbound** name. It has to go
/// through the alias map like any other model, or the retry would send
/// the alias key to the upstream and 404 again.
#[tokio::test]
async fn default_model_is_resolved_through_the_alias_map() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned_per_attempt.lock().await = VecDeque::from(vec![
        FakeResponse {
            status: StatusCode::NOT_FOUND,
            body: Some(REAL_GATEWAY_MODEL_404.into()),
        },
        FakeResponse {
            status: StatusCode::OK,
            body: Some(
                r#"{
                    "id": "resp_ok",
                    "object": "response",
                    "created_at": 1,
                    "status": "completed",
                    "model": "gpt-5.4-mini",
                    "output": [{
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": "fallback ok", "annotations": []}]
                    }],
                    "usage": {"input_tokens":1,"output_tokens":1,"total_tokens":2}
                }"#
                .into(),
            ),
        },
    ]);
    // `default_model` is the INBOUND name `claude-opus-5`, which the alias
    // map sends to `gpt-5.4-mini`.
    let proxy_addr = start_proxy(make_config_with_default_model(
        upstream_addr,
        BTreeMap::from([("claude-opus-5".to_string(), "gpt-5.4-mini".to_string())]),
        Some("claude-opus-5"),
    ))
    .await;

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
    let sent_bodies = upstream.received_all.lock().await.clone();
    assert_eq!(sent_bodies.len(), 2);
    let second: serde_json::Value = serde_json::from_slice(&sent_bodies[1]).unwrap();
    assert_eq!(
        second["model"], "gpt-5.4-mini",
        "the fallback must be alias-resolved, not sent as the alias key"
    );
}

/// A real upstream **tool** rejection. The body contains the words "not
/// supported", so a loose detector would fire the model fallback on it
/// and silently answer the request with a different model.
#[tokio::test]
async fn corpus_web_search_400_is_not_a_fallback_trigger() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned_per_attempt.lock().await = VecDeque::from([FakeResponse {
        status: StatusCode::BAD_REQUEST,
        body: Some(REAL_GATEWAY_WEB_SEARCH_400.into()),
    }]);
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
            r#"{"model":"gpt-4o","max_tokens":4,"messages":[{"role":"user","content":"a"}]}"#,
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let sent_bodies = upstream.received_all.lock().await.clone();
    assert_eq!(
        sent_bodies.len(),
        1,
        "a tool rejection must not trigger the model fallback"
    );
}

/// A 404 that is NOT about a model — a bad path on the upstream. It uses
/// the same `not_found_error` type but never mentions a model, so it
/// must not trigger the fallback.
#[tokio::test]
async fn non_model_404_does_not_fall_back() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned_per_attempt.lock().await = VecDeque::from([FakeResponse {
        status: StatusCode::NOT_FOUND,
        body: Some(
            r#"{"error":{"message":"Not Found","type":"not_found_error","param":null,"code":null}}"#
                .into(),
        ),
    }]);
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
            r#"{"model":"gpt-4o","max_tokens":4,"messages":[{"role":"user","content":"a"}]}"#,
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    let sent_bodies = upstream.received_all.lock().await.clone();
    assert_eq!(sent_bodies.len(), 1, "a non-model 404 must not retry");
}

/// If the **fallback** model is rejected too, the client gets that
/// second error — and there is no third attempt.
#[tokio::test]
async fn fallback_model_also_rejected_returns_the_second_error() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned_per_attempt.lock().await = VecDeque::from(vec![
        FakeResponse {
            status: StatusCode::NOT_FOUND,
            body: Some(REAL_GATEWAY_MODEL_404.into()),
        },
        FakeResponse {
            status: StatusCode::NOT_FOUND,
            body: Some(
                r#"{"error":{"message":"model \"gpt-4o-mini\" not found","type":"not_found_error","param":null,"code":null}}"#
                    .into(),
            ),
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
            r#"{"model":"claude-sonnet-5","max_tokens":4,"messages":[{"role":"user","content":"a"}]}"#,
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    let body: serde_json::Value = res.json().await.unwrap();
    // The client must see a readable message, not the raw JSON blob.
    let message = body["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("default_model") && message.contains("gpt-4o-mini"),
        "the error must name default_model as the problem, got {body}"
    );
    assert!(
        message.contains("not found"),
        "the upstream's own reason must be kept, got {body}"
    );
    assert!(
        !message.contains("{\"error\""),
        "the raw JSON body must not be pasted into the message: {body}"
    );
    let sent_bodies = upstream.received_all.lock().await.clone();
    assert_eq!(sent_bodies.len(), 2, "there must be no third attempt");
}

/// A retired `default_model` answers 410, not 404. The client must be told
/// the default model is the problem (the 2026-11-19 log: five requests died
/// on a retired default with the retirement notice as the only clue).
#[tokio::test]
async fn retired_default_model_is_named_in_the_error() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned_per_attempt.lock().await = VecDeque::from(vec![
        FakeResponse {
            status: StatusCode::NOT_FOUND,
            body: Some(REAL_GATEWAY_MODEL_404.into()),
        },
        FakeResponse {
            status: StatusCode::GONE,
            body: Some(
                r#"{"error":{"message":"gpt-4o-mini was retired on September 25, 2026. Try glm-5.3-flash:cloud.","type":"gone_error","param":null,"code":null}}"#
                    .into(),
            ),
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
            r#"{"model":"claude-sonnet-5","max_tokens":4,"messages":[{"role":"user","content":"a"}]}"#,
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::GONE);
    let body: serde_json::Value = res.json().await.unwrap();
    let message = body["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("default_model") && message.contains("retired"),
        "expected default_model named plus the retirement notice, got {body}"
    );
}

/// Retrying with the model that was just rejected would be pointless, so
/// the fallback is skipped.
#[tokio::test]
async fn default_model_equal_to_rejected_model_does_not_retry() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned_per_attempt.lock().await = VecDeque::from([FakeResponse {
        status: StatusCode::NOT_FOUND,
        body: Some(REAL_GATEWAY_MODEL_404.into()),
    }]);
    let proxy_addr = start_proxy(make_config_with_default_model(
        upstream_addr,
        BTreeMap::from([("claude-sonnet-5".to_string(), "gpt-4o-mini".to_string())]),
        Some("gpt-4o-mini"),
    ))
    .await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(
            r#"{"model":"claude-sonnet-5","max_tokens":4,"messages":[{"role":"user","content":"a"}]}"#,
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    let sent_bodies = upstream.received_all.lock().await.clone();
    assert_eq!(
        sent_bodies.len(),
        1,
        "retrying with the same model would be pointless"
    );
    let first: serde_json::Value = serde_json::from_slice(&sent_bodies[0]).unwrap();
    assert_eq!(first["model"], "gpt-4o-mini");
}

/// Streaming requests are also eligible for `default_model` fallback.
/// The retry happens before any SSE bytes are written to the client,
/// so the client only sees the successful fallback stream.
#[tokio::test]
async fn streaming_model_not_supported_falls_back_to_default() {
    let (upstream_addr, upstream) = start_fake_upstream().await;

    let fallback_sse = [
        r#"event: response.created
data: {"type":"response.created","response":{"id":"resp_fb","object":"response","created_at":0,"status":"in_progress","model":"gpt-4o-mini","output":[]}}"#,
        r#"event: response.output_item.added
data: {"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_fb","status":"in_progress","role":"assistant","content":[]}}"#,
        r#"event: response.output_text.delta
data: {"type":"response.output_text.delta","item_id":"msg_fb","output_index":0,"content_index":0,"delta":"fallback"}"#,
        r#"event: response.completed
data: {"type":"response.completed","response":{"id":"resp_fb","object":"response","created_at":0,"status":"completed","model":"gpt-4o-mini","output":[],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}"#,
    ]
    .join("\n\n")
        + "\n\n";

    *upstream.canned_per_attempt.lock().await = VecDeque::from(vec![
        FakeResponse {
            // The real gateway rejection, verbatim.
            status: StatusCode::NOT_FOUND,
            body: Some(REAL_GATEWAY_MODEL_404.into()),
        },
        FakeResponse {
            status: StatusCode::OK,
            body: Some(fallback_sse),
        },
    ]);

    let config = Arc::new(Config {
        model_info: Default::default(),
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        upstream_base_url: format!("http://{upstream_addr}"),
        upstream_api_key: "sk-fake".into(),
        upstream_path: "/v1/responses".into(),
        request_timeout: Duration::from_secs(10),
        max_request_body_bytes: openai_to_anthropic_proxy::config::DEFAULT_MAX_REQUEST_BODY_BYTES,
        reasoning_effort: Some("none".into()),
        reasoning: Default::default(),
        model_aliases: ModelAliases {
            map: BTreeMap::new(),
            default_model: Some("gpt-4o-mini".into()),
        },
        prompt_caching: Default::default(),
        reasoning_replay: Default::default(),
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
        max_retry_wait: Duration::from_secs(60),
        system_message_placement: Default::default(),
        warnings: Vec::new(),
    });
    let proxy_addr = start_proxy(config).await;

    let client = reqwest::Client::new();
    let res = client
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(
            r#"{"model":"claude-sonnet-5","max_tokens":4,"stream":true,"messages":[{"role":"user","content":"a"}]}"#,
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);
    let text = res.text().await.unwrap();
    let events: Vec<&str> = text.split("\n\n").filter(|s| !s.is_empty()).collect();
    let kinds: Vec<&str> = events
        .iter()
        .filter_map(|e| e.lines().find_map(|l| l.strip_prefix("event: ")))
        .collect();
    assert!(
        kinds.contains(&"content_block_delta"),
        "expected fallback stream to contain a text delta, got kinds={kinds:?}"
    );
    assert!(
        kinds.contains(&"message_stop"),
        "expected fallback stream to close cleanly, got kinds={kinds:?}"
    );

    let sent_bodies = upstream.received_all.lock().await.clone();
    assert_eq!(sent_bodies.len(), 2, "expected exactly one retry");
    let first: serde_json::Value = serde_json::from_slice(&sent_bodies[0]).unwrap();
    let second: serde_json::Value = serde_json::from_slice(&sent_bodies[1]).unwrap();
    assert_eq!(first["model"], "claude-sonnet-5");
    assert_eq!(second["model"], "gpt-4o-mini");
}

/// When falling back to `default_model`, the proxy must recompute the
/// `reasoning.effort` value for the *fallback* model, not reuse the
/// value from the original model. This matters when the config has
/// per-model reasoning entries.
#[tokio::test]
async fn fallback_recomputes_reasoning_for_default_model() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned_per_attempt.lock().await = VecDeque::from(vec![
        FakeResponse {
            status: StatusCode::BAD_REQUEST,
            body: Some(
                r#"{"message":"model not supported","type":"invalid_request_error","code":"model_not_supported"}"#.into(),
            ),
        },
        FakeResponse {
            status: StatusCode::OK,
            body: Some(
                r#"{
                    "id": "resp_ok",
                    "object": "response",
                    "created_at": 1,
                    "status": "completed",
                    "model": "gpt-4o-mini",
                    "output": [{
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": "fallback ok", "annotations": []}]
                    }],
                    "usage": {"input_tokens":1,"output_tokens":1,"total_tokens":2}
                }"#
                .into(),
            ),
        },
    ]);

    let config = Arc::new(Config {
        model_info: Default::default(),
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        upstream_base_url: format!("http://{upstream_addr}"),
        upstream_api_key: "sk-fake".into(),
        upstream_path: "/v1/responses".into(),
        request_timeout: Duration::from_secs(10),
        max_request_body_bytes: openai_to_anthropic_proxy::config::DEFAULT_MAX_REQUEST_BODY_BYTES,
        reasoning_effort: None,
        reasoning: ReasoningConfig {
            default: Some("low".into()),
            models: std::iter::once(("gpt-4o-mini".into(), "none".into())).collect(),
            ..ReasoningConfig::default()
        },
        model_aliases: ModelAliases {
            map: BTreeMap::new(),
            default_model: Some("gpt-4o-mini".into()),
        },
        prompt_caching: Default::default(),
        reasoning_replay: Default::default(),
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
        max_retry_wait: Duration::from_secs(60),
        system_message_placement: Default::default(),
        warnings: Vec::new(),
    });
    let proxy_addr = start_proxy(config).await;

    let client = reqwest::Client::new();
    let res = client
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(
            r#"{"model":"claude-sonnet-5","max_tokens":4,"messages":[{"role":"user","content":"a"}]}"#,
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);

    let sent_bodies = upstream.received_all.lock().await.clone();
    assert_eq!(sent_bodies.len(), 2);
    let first: serde_json::Value = serde_json::from_slice(&sent_bodies[0]).unwrap();
    let second: serde_json::Value = serde_json::from_slice(&sent_bodies[1]).unwrap();
    // Original model not in the reasoning map → falls back to the global default "low".
    assert_eq!(first["reasoning"]["effort"], "low");
    // Fallback model is in the map → gets its own per-model entry "none".
    assert_eq!(second["reasoning"]["effort"], "none");
}

#[tokio::test]
async fn fallback_recomputes_prompt_caching_for_default_model() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned_per_attempt.lock().await = VecDeque::from(vec![
        FakeResponse {
            status: StatusCode::BAD_REQUEST,
            body: Some(
                r#"{"message":"model not supported","type":"invalid_request_error","code":"model_not_supported"}"#.into(),
            ),
        },
        FakeResponse {
            status: StatusCode::OK,
            body: Some(
                r#"{
                    "id": "resp_ok",
                    "object": "response",
                    "created_at": 1,
                    "status": "completed",
                    "model": "gpt-4o-mini",
                    "output": [],
                    "usage": {"input_tokens":1,"output_tokens":1,"total_tokens":2}
                }"#
                .into(),
            ),
        },
    ]);

    let config = Arc::new(Config {
        model_info: Default::default(),
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        upstream_base_url: format!("http://{upstream_addr}"),
        upstream_api_key: "sk-fake".into(),
        upstream_path: "/v1/responses".into(),
        request_timeout: Duration::from_secs(10),
        max_request_body_bytes: openai_to_anthropic_proxy::config::DEFAULT_MAX_REQUEST_BODY_BYTES,
        reasoning_effort: Some("none".into()),
        reasoning: Default::default(),
        reasoning_replay: Default::default(),
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
        max_retry_wait: Duration::from_secs(60),
        system_message_placement: Default::default(),
        warnings: Vec::new(),
    });
    let proxy_addr = start_proxy(config).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(
            r#"{
                "model":"claude-sonnet-5",
                "max_tokens":4,
                "messages":[{"role":"user","content":[{"type":"text","text":"cached","cache_control":{"type":"ephemeral"}}]}]
            }"#,
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let sent_bodies = upstream.received_all.lock().await.clone();
    let first: serde_json::Value = serde_json::from_slice(&sent_bodies[0]).unwrap();
    let second: serde_json::Value = serde_json::from_slice(&sent_bodies[1]).unwrap();
    assert_eq!(first["prompt_cache_key"], "cache-key");
    assert!(first["input"][0]["content"][0]["prompt_cache_breakpoint"].is_object());
    assert!(second.get("prompt_cache_key").is_none());
    assert!(
        second["input"][0]["content"][0]
            .get("prompt_cache_breakpoint")
            .is_none()
    );
}

/// The `default_model` fallback rebuilds the request from scratch, so the
/// fallback model's reasoning-replay policy must be re-resolved — the
/// first model's policy must not leak into the retry.
#[tokio::test]
async fn fallback_recomputes_reasoning_replay_for_default_model() {
    use openai_to_anthropic_proxy::config::ReasoningReplayMode;

    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned_per_attempt.lock().await = VecDeque::from(vec![
        FakeResponse {
            status: StatusCode::BAD_REQUEST,
            body: Some(
                r#"{"message":"model not supported","type":"invalid_request_error","code":"model_not_supported"}"#
                    .into(),
            ),
        },
        FakeResponse {
            status: StatusCode::OK,
            body: Some(
                r#"{
                    "id": "resp_ok",
                    "object": "response",
                    "created_at": 1,
                    "status": "completed",
                    "model": "gpt-4o-mini",
                    "output": [],
                    "usage": {"input_tokens":1,"output_tokens":1,"total_tokens":2}
                }"#
                .into(),
            ),
        },
    ]);

    let config = Arc::new(Config {
        model_info: Default::default(),
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        upstream_base_url: format!("http://{upstream_addr}"),
        upstream_api_key: "sk-fake".into(),
        upstream_path: "/v1/responses".into(),
        request_timeout: Duration::from_secs(10),
        max_request_body_bytes: openai_to_anthropic_proxy::config::DEFAULT_MAX_REQUEST_BODY_BYTES,
        reasoning_effort: Some("none".into()),
        reasoning: Default::default(),
        reasoning_replay: openai_to_anthropic_proxy::config::ReasoningReplayConfig {
            default: ReasoningReplayMode::Text,
            models: BTreeMap::from([("gpt-4o-mini".into(), ReasoningReplayMode::Off)]),
            max_chars: 0,
        },
        model_aliases: ModelAliases {
            map: BTreeMap::new(),
            default_model: Some("gpt-4o-mini".into()),
        },
        prompt_caching: Default::default(),
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
        max_retry_wait: Duration::from_secs(60),
        system_message_placement: Default::default(),
        warnings: Vec::new(),
    });
    let proxy_addr = start_proxy(config).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(
            r#"{
                "model":"claude-sonnet-5",
                "max_tokens":4,
                "messages":[
                    {"role":"user","content":"x"},
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

    let sent_bodies = upstream.received_all.lock().await.clone();
    assert_eq!(sent_bodies.len(), 2, "expected a fallback retry");
    let first: serde_json::Value = serde_json::from_slice(&sent_bodies[0]).unwrap();
    let second: serde_json::Value = serde_json::from_slice(&sent_bodies[1]).unwrap();

    // The primary model uses the default (`text`) mode: the reasoning is
    // folded into the assistant message.
    let first_text = first["input"][1]["content"][0]["text"].as_str().unwrap();
    assert!(
        first_text.contains("the model reasoned about this"),
        "first attempt lost the reasoning: {first_text:?}"
    );

    // The fallback model is configured `off`: the reasoning must be gone.
    let second_text = second["input"][1]["content"][0]["text"].as_str().unwrap();
    assert_eq!(second_text, "the answer");
}
