//! The proxy-native web_fetch server tool (mirrors
//! `src/tools/web_fetch`).

use crate::harness::*;

/// With `web_fetch` disabled, the request must be byte-for-byte identical to
/// the pre-feature path: no synthesized tool, no forced buffering, no extra
/// upstream call.
#[tokio::test]
async fn web_fetch_disabled_is_byte_for_byte_identical() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned.lock().await = Some(completed_response_with_id("resp_same", "same answer"));
    let mut config = (*make_proxy_config(upstream_addr)).clone();
    config.web_fetch.enabled = false;
    let proxy_addr = start_proxy(Arc::new(config)).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(web_fetch_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
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
        "the client response must be unchanged when web_fetch is disabled"
    );

    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(sent.len(), 1, "no extra upstream POSTs");
}

/// With `web_fetch` enabled and the tool declared, the proxy must
/// synthesize the function tool and force the upstream request to be
/// buffered (the executor has to be re-run with the fetched text).
#[tokio::test]
async fn web_fetch_enabled_synthesizes_the_tool_and_buffers() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned.lock().await = Some(completed_response_with_id("resp_done", "done"));
    let config = web_fetch_config(upstream_addr);
    let proxy_addr = start_proxy(config).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(web_fetch_request(true)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(sent.len(), 1);
    let received: serde_json::Value = serde_json::from_slice(&sent[0]).unwrap();
    // The client streams, so the executor run is streamed upstream too.
    assert_eq!(received["stream"], true);
    // The synthesized function tool is present, with the URL parameter.
    let tools = received["tools"].as_array().expect("tools");
    let fetch_tool = tools
        .iter()
        .find(|t| t["name"] == "web_fetch")
        .expect("the web_fetch function tool must be synthesized");
    assert_eq!(fetch_tool["type"], "function");
    assert_eq!(fetch_tool["parameters"]["required"][0], "url");
}

/// The full loop: the upstream asks for a fetch, the fetch fails (the SSRF
/// guard refuses the private address), and the proxy must DEGRADE — emitting
/// a `web_fetch_tool_result` error block, re-running the executor, and still
/// answering the client with HTTP 200.
#[tokio::test]
async fn a_failed_fetch_degrades_to_an_error_block_and_the_turn_continues() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    // Attempt 1: the executor asks for the page. Attempt 2: the executor
    // answers with the failure in context.
    *upstream.canned_per_attempt.lock().await = VecDeque::from([
        FakeResponse {
            status: StatusCode::OK,
            body: Some(web_fetch_call_response("http://127.0.0.1:9/page")),
        },
        FakeResponse {
            status: StatusCode::OK,
            body: Some(completed_response_with_id(
                "resp_after",
                "I could not retrieve it.",
            )),
        },
    ]);

    let config = web_fetch_config(upstream_addr);
    let proxy_addr = start_proxy(config).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(web_fetch_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let body: serde_json::Value = res.json().await.unwrap();
    let content = body["content"].as_array().expect("content");

    // The `server_tool_use` block opens the pair.
    let use_block = content
        .iter()
        .find(|b| b["type"] == "server_tool_use")
        .expect("a server_tool_use block must be present");
    assert_eq!(use_block["name"], "web_fetch");
    assert!(use_block["input"]["url"].is_string());

    // The paired result is an ERROR, because the private address is
    // refused by the SSRF guard.
    let result = content
        .iter()
        .find(|b| b["type"] == "web_fetch_tool_result")
        .expect("a web_fetch_tool_result block must be present");
    assert_eq!(result["tool_use_id"], use_block["id"]);
    assert_eq!(result["content"]["type"], "web_fetch_tool_result_error");
    assert_eq!(result["content"]["error_code"], "url_not_allowed");

    // The executor's final answer is still delivered.
    assert!(
        content
            .iter()
            .any(|b| b["type"] == "text" && b["text"] == "I could not retrieve it."),
        "the executor's answer must survive the failed fetch: {content:?}"
    );

    // Two upstream calls: the executor asked, then the executor answered.
    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(sent.len(), 2);
}

/// The client must never see a `tool_use` for `web_fetch`: it is a SERVER
/// tool, and the client only knows the `server_tool_use` /
/// `web_fetch_tool_result` pair.
#[tokio::test]
async fn the_client_never_sees_a_web_fetch_tool_use() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned_per_attempt.lock().await = VecDeque::from([
        FakeResponse {
            status: StatusCode::OK,
            body: Some(web_fetch_call_response("http://127.0.0.1:9/page")),
        },
        FakeResponse {
            status: StatusCode::OK,
            body: Some(completed_response_with_id("resp_after", "answer")),
        },
    ]);

    let config = web_fetch_config(upstream_addr);
    let proxy_addr = start_proxy(config).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(web_fetch_request(false)))
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = res.json().await.unwrap();
    let content = body["content"].as_array().expect("content");
    assert!(
        !content
            .iter()
            .any(|b| b["type"] == "tool_use" && b["name"] == "web_fetch"),
        "the internal web_fetch call must not surface as a client tool_use: {content:?}"
    );
}

/// A streaming client gets ONE synthetic Anthropic SSE sequence.
#[tokio::test]
async fn a_streaming_client_gets_one_synthetic_sse_sequence() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned_per_attempt.lock().await = VecDeque::from([
        FakeResponse {
            status: StatusCode::OK,
            body: Some(web_fetch_call_response("http://127.0.0.1:9/page")),
        },
        FakeResponse {
            status: StatusCode::OK,
            body: Some(completed_response_with_id("resp_after", "streamed answer")),
        },
    ]);

    let config = web_fetch_config(upstream_addr);
    let proxy_addr = start_proxy(config).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(web_fetch_request(true)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream"),
        "a streaming client must get an SSE response"
    );

    let body = res.text().await.unwrap();
    assert_eq!(
        body.matches("\"type\":\"message_start\"").count(),
        1,
        "exactly one message_start"
    );
    assert_eq!(
        body.matches("\"type\":\"message_stop\"").count(),
        1,
        "exactly one message_stop"
    );
    assert!(
        !body.contains("[DONE]"),
        "the synthetic sequence must not carry a [DONE] sentinel"
    );
}

/// A request that declares a CLIENT function tool named `web_fetch` (with
/// an `input_schema`) must not activate the server-tool pipeline.
#[tokio::test]
async fn a_client_function_tool_named_web_fetch_does_not_activate_the_pipeline() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned.lock().await = Some(completed_response_with_id("resp_plain", "plain answer"));
    let config = web_fetch_config(upstream_addr);
    let proxy_addr = start_proxy(config).await;

    let request = r#"{
        "model": "gpt-4o-mini",
        "max_tokens": 64,
        "tools": [{"name":"web_fetch","description":"d","input_schema":{"type":"object","properties":{"url":{"type":"string"}}}}],
        "messages": [{"role": "user", "content": "hello"}]
    }"#;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(request))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(sent.len(), 1, "no re-run: the pipeline must not activate");
    let received: serde_json::Value = serde_json::from_slice(&sent[0]).unwrap();
    // The client's own tool is forwarded upstream, unchanged.
    let tools = received["tools"].as_array().expect("tools");
    assert!(
        tools.iter().any(|t| t["name"] == "web_fetch"),
        "the client tool must be forwarded: {tools:?}"
    );
}

/// A `web_fetch` declaration with the shipped default config must not
/// change the pipeline at all: the feature is off by default.
#[tokio::test]
async fn the_default_config_leaves_the_pipeline_alone() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned.lock().await = Some(completed_response_with_id("resp_plain", "plain answer"));
    // The shipped default is `enabled: false`.
    let config = make_proxy_config(upstream_addr);
    let proxy_addr = start_proxy(config).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(web_fetch_request(true)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(sent.len(), 1, "no extra upstream POSTs");
    let received: serde_json::Value = serde_json::from_slice(&sent[0]).unwrap();
    // Streaming is NOT forced off when the feature is off.
    assert_eq!(received["stream"], true);
}
