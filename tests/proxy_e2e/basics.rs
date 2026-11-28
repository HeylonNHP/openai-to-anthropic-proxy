//! Core request round trips (streaming and not), upstream timeouts
//! and error envelopes, plus the request gate (body limit, proxy key).

use crate::harness::*;

#[tokio::test]
async fn upstream_timeout_returns_gateway_timeout() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.delay.lock().await = Some(Duration::from_millis(100));

    let mut config = (*make_proxy_config(upstream_addr)).clone();
    config.request_timeout = Duration::from_millis(20);
    let proxy_addr = start_proxy(Arc::new(config)).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(
            r#"{"model":"gpt-4o","max_tokens":4,"messages":[{"role":"user","content":"a"}]}"#,
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::GATEWAY_TIMEOUT);
}

#[tokio::test]
async fn non_streaming_round_trip() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned.lock().await = Some(
        r#"{
            "id": "resp_abc",
            "object": "response",
            "created_at": 1,
            "status": "completed",
            "model": "gpt-4o",
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [
                    {"type": "output_text", "text": "Hello from upstream!", "annotations": []}
                ]
            }],
            "usage": {
                "input_tokens": 5,
                "output_tokens": 3,
                "total_tokens": 8
            }
        }"#
        .into(),
    );

    let config = make_proxy_config(upstream_addr);
    let proxy_addr = start_proxy(config).await;

    let client = reqwest::Client::new();
    let res = client
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(
            r#"{
                "model": "gpt-4o",
                "max_tokens": 64,
                "messages": [{"role": "user", "content": "Hi"}]
            }"#,
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["type"], "message");
    assert_eq!(body["role"], "assistant");
    assert_eq!(body["stop_reason"], "end_turn");
    assert_eq!(body["content"][0]["type"], "text");
    assert_eq!(body["content"][0]["text"], "Hello from upstream!");
    assert_eq!(body["usage"]["input_tokens"], 5);
    assert_eq!(body["usage"]["output_tokens"], 3);

    // Verify the upstream got a translated Responses request.
    let received = upstream.received.lock().await.clone().unwrap();
    let received: serde_json::Value = serde_json::from_slice(&received).unwrap();
    assert_eq!(received["model"], "gpt-4o");
    assert_eq!(received["stream"], false);
    // input is a list of items; the first one is the user message.
    let items = received["input"].as_array().expect("input is array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["type"], "message");
    assert_eq!(items[0]["role"], "user");
}

#[tokio::test]
async fn truncated_stream_returns_error_instead_of_success() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    let sse = [
        r#"event: response.created
data: {"type":"response.created","response":{"id":"resp_1","object":"response","created_at":0,"status":"in_progress","model":"gpt-4o","output":[]}}"#,
        r#"event: response.output_item.added
data: {"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_x","status":"in_progress","role":"assistant","content":[]}}"#,
        r#"event: response.output_text.delta
data: {"type":"response.output_text.delta","item_id":"msg_x","output_index":0,"content_index":0,"delta":"partial"}"#, 
    ]
    .join("\n\n")
        + "\n\n";
    *upstream.canned_stream.lock().await = Some(sse);

    let config = make_proxy_config(upstream_addr);
    let proxy_addr = start_proxy(config).await;

    let client = reqwest::Client::new();
    let res = client
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(
            r#"{
                "model": "gpt-4o",
                "max_tokens": 64,
                "stream": true,
                "messages": [{"role": "user", "content": "Hi"}]
            }"#,
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

    assert_eq!(
        kinds,
        vec![
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_stop",
            "error",
            "message_stop",
        ]
    );
    assert!(!kinds.contains(&"message_delta"));
}

#[tokio::test]
async fn upstream_error_stream_returns_one_error_sequence() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    let sse = r#"event: response.created
data: {"type":"response.created","response":{"id":"resp_1","object":"response","created_at":0,"status":"in_progress","model":"gpt-4o","output":[]}}

event: error
data: {"type":"error","message":"upstream failed"}

"#;
    *upstream.canned_stream.lock().await = Some(sse.into());

    let config = make_proxy_config(upstream_addr);
    let proxy_addr = start_proxy(config).await;

    let client = reqwest::Client::new();
    let res = client
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(
            r#"{"model":"gpt-4o","max_tokens":16,"stream":true,"messages":[{"role": "user", "content":"x"}]}"#,
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);
    let text = res.text().await.unwrap();
    let kinds: Vec<&str> = text
        .split("\n\n")
        .filter(|s| !s.is_empty())
        .filter_map(|e| e.lines().find_map(|l| l.strip_prefix("event: ")))
        .collect();
    assert_eq!(kinds, vec!["message_start", "error", "message_stop"]);
}

#[tokio::test]
async fn late_upstream_error_after_completed_stream_is_ignored() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    let sse = [
        r#"event: response.created
 data: {"type":"response.created","response":{"id":"resp_1","object":"response","created_at":0,"status":"in_progress","model":"gpt-4o","output":[]}}"#,
        r#"event: response.completed
 data: {"type":"response.completed","response":{"id":"resp_1","object":"response","created_at":0,"status":"completed","model":"gpt-4o","output":[],"usage":{"input_tokens":4,"output_tokens":2,"total_tokens":6}}}"#,
        r#"event: error
 data: {"type":"error","message":"late upstream error"}"#,
    ]
    .join("\n\n")
    .replace(" data:", "data:")
        + "\n\n";
    *upstream.canned_stream.lock().await = Some(sse);

    let config = make_proxy_config(upstream_addr);
    let proxy_addr = start_proxy(config).await;

    let client = reqwest::Client::new();
    let res = client
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(
            r#"{"model":"gpt-4o","max_tokens":16,"stream":true,"messages":[{"role": "user", "content":"x"}]}"#,
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);
    let text = res.text().await.unwrap();
    let kinds: Vec<&str> = text
        .split("\n\n")
        .filter(|s| !s.is_empty())
        .filter_map(|e| e.lines().find_map(|l| l.strip_prefix("event: ")))
        .collect();
    assert_eq!(
        kinds,
        vec!["message_start", "message_delta", "message_stop"]
    );
}

#[tokio::test]
async fn streaming_round_trip() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    // Responses API SSE event format: `event: <type>\ndata: <json>\n\n`.
    // The terminal event is `response.completed` (not `[DONE]`).
    let sse = [
        r#"event: response.created
data: {"type":"response.created","response":{"id":"resp_1","object":"response","created_at":0,"status":"in_progress","model":"gpt-4o","output":[]}}"#,
        r#"event: response.output_item.added
data: {"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_x","status":"in_progress","role":"assistant","content":[]}}"#,
        r#"event: response.output_text.delta
data: {"type":"response.output_text.delta","item_id":"msg_x","output_index":0,"content_index":0,"delta":"Hi"}"#,
        r#"event: response.output_text.delta
data: {"type":"response.output_text.delta","item_id":"msg_x","output_index":0,"content_index":0,"delta":" there"}"#,
        r#"event: response.completed
data: {"type":"response.completed","response":{"id":"resp_1","object":"response","created_at":0,"status":"completed","model":"gpt-4o","output":[],"usage":{"input_tokens":4,"output_tokens":2,"total_tokens":6}}}"#,
    ].join("\n\n") + "\n\n";
    *upstream.canned_stream.lock().await = Some(sse);

    let config = make_proxy_config(upstream_addr);
    let proxy_addr = start_proxy(config).await;

    let client = reqwest::Client::new();
    let res = client
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(
            r#"{
                "model": "gpt-4o",
                "max_tokens": 64,
                "stream": true,
                "messages": [{"role": "user", "content": "Hi"}]
            }"#,
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);
    let text = res.text().await.unwrap();

    // Validate the SSE event sequence: message_start, content_block_start,
    // text_delta x 2, content_block_stop, message_delta, message_stop.
    let events: Vec<&str> = text.split("\n\n").filter(|s| !s.is_empty()).collect();

    // Each event block is "event: <kind>\ndata: <json>"
    let kinds: Vec<&str> = events
        .iter()
        .filter_map(|e| e.lines().find_map(|l| l.strip_prefix("event: ")))
        .collect();
    assert_eq!(
        kinds,
        vec![
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop",
        ]
    );
}

#[tokio::test]
async fn upstream_error_returns_error_envelope() {
    // The Responses API uses a flat error shape (no `error: { ... }`
    // wrapper). The proxy must still surface a usable error body to
    // the client.
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned_error.lock().await = Some((
        StatusCode::UNAUTHORIZED,
        r#"{"message":"bad key","type":"authentication_error","code":"invalid_api_key"}"#.into(),
    ));

    let config = make_proxy_config(upstream_addr);
    let proxy_addr = start_proxy(config).await;

    let client = reqwest::Client::new();
    let res = client
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(
            r#"{"model":"gpt-4o","max_tokens":16,"messages":[{"role":"user","content":"x"}]}"#,
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "authentication_error");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("bad key")
    );
}

/// A configured body limit must reject an oversized body with a 413 before
/// anything is forwarded upstream.
#[tokio::test]
async fn configured_small_request_body_limit_returns_413() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned.lock().await = Some(CANNED_RESPONSE.to_string());
    let mut config = (*make_proxy_config(upstream_addr)).clone();
    config.max_request_body_bytes = 64;
    let proxy_addr = start_proxy(Arc::new(config)).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(
            r#"{"model":"gpt-4o","max_tokens":16,"messages":[{"role":"user","content":"hello"}]}"#,
        )
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::PAYLOAD_TOO_LARGE);
    // The rejection is an Anthropic-shaped JSON error that names the knob,
    // not axum's bare plain-text 413.
    let json: serde_json::Value = res.json().await.expect("413 body must be JSON");
    assert_eq!(json["type"], "error");
    assert_eq!(json["error"]["type"], "request_too_large");
    let message = json["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("max_request_body_bytes"),
        "got {message:?}"
    );
    assert!(message.contains("64"), "got {message:?}");
    assert_eq!(
        *upstream.request_count.lock().await,
        0,
        "oversized body must be rejected before upstream forwarding"
    );
}

/// The `X-Proxy-Key` check must run BEFORE the body is buffered. With a body
/// limit smaller than the request, an unauthenticated client gets 401 (the
/// body was never read); if auth only ran after buffering, the body-limit
/// rejection (413) would win. An authenticated client still gets the 413.
#[tokio::test]
async fn proxy_key_is_checked_before_the_body_is_read() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned.lock().await = Some(CANNED_RESPONSE.to_string());
    let mut config = (*make_proxy_config(upstream_addr)).clone();
    config.max_request_body_bytes = 64;
    config.proxy_key = Some("s3cret".to_string());
    let proxy_addr = start_proxy(Arc::new(config)).await;
    let body =
        r#"{"model":"gpt-4o","max_tokens":16,"messages":[{"role":"user","content":"hello"}]}"#;

    let unauth = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED);

    let authed = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .header("x-proxy-key", "s3cret")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(authed.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

/// An uncached turn still carries every usage key Claude Code requires, in a
/// buffered body and in a streamed `message_start`.
#[tokio::test]
async fn usage_carries_the_required_keys_when_nothing_was_cached() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned.lock().await = Some(
        r#"{"id":"resp_u","object":"response","created_at":1,"status":"completed","model":"gpt-4o",
            "output":[],"usage":{"input_tokens":5,"output_tokens":3,"total_tokens":8}}"#
            .into(),
    );
    let proxy_addr = start_proxy(make_proxy_config(upstream_addr)).await;

    for stream in [false, true] {
        // The fake serves a canned stream to every request, so set it late.
        if stream {
            *upstream.canned_stream.lock().await = Some(
                r#"event: response.completed
data: {"type":"response.completed","response":{"id":"resp_u","object":"response","created_at":0,"status":"completed","model":"gpt-4o","output":[],"usage":{"input_tokens":4,"output_tokens":2,"total_tokens":6}}}

"#
                .into(),
            );
        }
        let res = reqwest::Client::new()
            .post(format!("http://{proxy_addr}/v1/messages"))
            .header("content-type", "application/json")
            .body(ReqBody::from(format!(
                r#"{{"model":"gpt-4o","max_tokens":16,"stream":{stream},"messages":[{{"role":"user","content":"x"}}]}}"#
            )))
            .send()
            .await
            .unwrap();
        let text = res.text().await.unwrap();
        let usage = if stream {
            let start = text
                .lines()
                .filter_map(|l| l.strip_prefix("data: "))
                .map(|d| serde_json::from_str::<serde_json::Value>(d).unwrap())
                .find(|v| v["type"] == "message_start")
                .expect("message_start");
            start["message"]["usage"].clone()
        } else {
            serde_json::from_str::<serde_json::Value>(&text).unwrap()["usage"].clone()
        };
        let keys = usage.as_object().expect("usage object");
        for key in [
            "cache_creation_input_tokens",
            "cache_read_input_tokens",
            "cache_creation",
            "service_tier",
        ] {
            assert!(keys.contains_key(key), "stream={stream} {key}: {usage}");
        }
    }
}
