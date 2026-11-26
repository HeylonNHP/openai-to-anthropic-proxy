//! Upstream prompt-too-long re-worded into Anthropic's
//! phrasing (mirrors `src/context_overflow.rs`).

use crate::harness::*;

const OVERFLOW_BODY: &str = r#"{"error":{"message":"The prompt is too long: 1424454, model maximum context length: 1048576","type":"invalid_request_error"}}"#;

async fn post_overflow_probe(proxy_addr: SocketAddr, body: &'static str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(body))
        .send()
        .await
        .unwrap()
}

/// Claude Code recovers from an over-long prompt by recognising
/// Anthropic's own wording, so the upstream's must be re-worded to it.
#[tokio::test]
async fn context_overflow_is_reported_in_anthropics_wording() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned_error.lock().await = Some((StatusCode::BAD_REQUEST, OVERFLOW_BODY.into()));
    let proxy_addr = start_proxy(make_proxy_config(upstream_addr)).await;

    let res = post_overflow_probe(
        proxy_addr,
        r#"{"model":"gpt-4o","max_tokens":16,"messages":[{"role":"user","content":"x"}]}"#,
    )
    .await;

    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert_eq!(
        body["error"]["message"],
        "prompt is too long: 1424454 tokens > 1048576 maximum"
    );
}

#[tokio::test]
async fn context_overflow_on_a_streaming_request_is_still_a_400() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned_error.lock().await = Some((StatusCode::BAD_REQUEST, OVERFLOW_BODY.into()));
    let proxy_addr = start_proxy(make_proxy_config(upstream_addr)).await;

    let res = post_overflow_probe(
        proxy_addr,
        r#"{"model":"gpt-4o","max_tokens":16,"stream":true,"messages":[{"role":"user","content":"x"}]}"#,
    )
    .await;

    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(
        body["error"]["message"],
        "prompt is too long: 1424454 tokens > 1048576 maximum"
    );
}

#[tokio::test]
async fn context_overflow_is_never_retried_on_the_default_model() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned_error.lock().await = Some((StatusCode::BAD_REQUEST, OVERFLOW_BODY.into()));
    let mut config = make_proxy_config(upstream_addr);
    Arc::get_mut(&mut config)
        .unwrap()
        .model_aliases
        .default_model = Some("gpt-4o-mini".into());
    let proxy_addr = start_proxy(config).await;

    let res = post_overflow_probe(
        proxy_addr,
        r#"{"model":"gpt-4o","max_tokens":16,"messages":[{"role":"user","content":"x"}]}"#,
    )
    .await;

    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    assert_eq!(upstream.received_all.lock().await.len(), 1);
}

#[tokio::test]
async fn context_overflow_inside_a_stream_is_an_anthropic_error_event() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    let sse = r#"event: response.created
data: {"type":"response.created","response":{"id":"resp_1","object":"response","created_at":0,"status":"in_progress","model":"gpt-4o","output":[]}}

event: response.failed
data: {"type":"response.failed","response":{"id":"resp_1","object":"response","created_at":0,"status":"failed","model":"gpt-4o","output":[],"error":{"message":"The prompt is too long: 10, model maximum context length: 5","type":"invalid_request_error","code":"context_length_exceeded"}}}

"#;
    *upstream.canned_stream.lock().await = Some(sse.into());
    let proxy_addr = start_proxy(make_proxy_config(upstream_addr)).await;

    let res = post_overflow_probe(
        proxy_addr,
        r#"{"model":"gpt-4o","max_tokens":16,"stream":true,"messages":[{"role":"user","content":"x"}]}"#,
    )
    .await;

    assert_eq!(res.status(), StatusCode::OK);
    let text = res.text().await.unwrap();
    let error = text
        .split("\n\n")
        .find(|e| e.contains("event: error"))
        .expect("an error event");
    let data = error
        .lines()
        .find_map(|l| l.strip_prefix("data: "))
        .unwrap();
    let data: serde_json::Value = serde_json::from_str(data).unwrap();
    assert_eq!(data["error"]["type"], "invalid_request_error");
    assert_eq!(
        data["error"]["message"],
        "prompt is too long: 10 tokens > 5 maximum"
    );
}

#[tokio::test]
async fn an_unrelated_400_keeps_the_generic_upstream_wording() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned_error.lock().await = Some((
        StatusCode::BAD_REQUEST,
        r#"{"error":{"message":"bad input","type":"invalid_request_error"}}"#.into(),
    ));
    let proxy_addr = start_proxy(make_proxy_config(upstream_addr)).await;

    let res = post_overflow_probe(
        proxy_addr,
        r#"{"model":"gpt-4o","max_tokens":16,"messages":[{"role":"user","content":"x"}]}"#,
    )
    .await;

    let body: serde_json::Value = res.json().await.unwrap();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("upstream returned 400")
    );
}
