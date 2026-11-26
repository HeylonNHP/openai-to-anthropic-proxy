//! The max_tokens floor behaviour.

use crate::harness::*;

// ── `max_tokens` floor and the `max_tokens: 0` cache pre-warm ──

/// POST `/v1/messages` with the given `max_tokens` (and optional `stream`) to a
/// proxy whose `min_output_tokens` is `floor`. Returns the proxy's response and
/// the `max_output_tokens` the upstream saw (`None` if it was never called).
async fn send_with_max_tokens(
    max_tokens: u32,
    stream: bool,
    floor: u32,
) -> (reqwest::Response, Option<serde_json::Value>) {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned.lock().await = Some(
        r#"{
            "id": "resp_ok", "object": "response", "created_at": 1,
            "status": "completed", "model": "any",
            "output": [{"type": "message", "role": "assistant",
                "content": [{"type": "output_text", "text": "hello", "annotations": []}]}],
            "usage": {"input_tokens":5,"output_tokens":7,"total_tokens":12}
        }"#
        .into(),
    );
    let mut config = (*make_proxy_config(upstream_addr)).clone();
    config.min_output_tokens = floor;
    let proxy_addr = start_proxy(Arc::new(config)).await;
    let body = format!(
        r#"{{"model":"gpt-4o","max_tokens":{max_tokens},"stream":{stream},
            "messages":[{{"role":"user","content":"hi"}}]}}"#
    );
    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(body))
        .send()
        .await
        .unwrap();
    let sent = upstream.received.lock().await.clone().map(|raw| {
        serde_json::from_slice::<serde_json::Value>(&raw).unwrap()["max_output_tokens"].clone()
    });
    (res, sent)
}

#[tokio::test]
async fn a_small_max_tokens_is_raised_to_the_floor_upstream() {
    let (res, sent) = send_with_max_tokens(1, false, 16).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(sent, Some(serde_json::json!(16)));
    // The client still gets the real reply: only the cap was raised.
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["content"][0]["text"], "hello");
}

#[tokio::test]
async fn a_max_tokens_at_or_above_the_floor_is_sent_as_it_is() {
    assert_eq!(
        send_with_max_tokens(16, false, 16).await.1,
        Some(serde_json::json!(16))
    );
    assert_eq!(
        send_with_max_tokens(100, false, 16).await.1,
        Some(serde_json::json!(100))
    );
}

#[tokio::test]
async fn a_floor_of_zero_sends_the_clients_value_unchanged() {
    assert_eq!(
        send_with_max_tokens(1, false, 0).await.1,
        Some(serde_json::json!(1))
    );
}

#[tokio::test]
async fn max_tokens_zero_primes_the_upstream_but_answers_with_no_content() {
    let (res, sent) = send_with_max_tokens(0, false, 16).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        sent,
        Some(serde_json::json!(16)),
        "upstream is asked for the floor"
    );
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["content"], serde_json::json!([]));
    assert_eq!(body["stop_reason"], "max_tokens");
    assert_eq!(body["usage"]["output_tokens"], 0, "no output is billed");
    assert_eq!(body["usage"]["input_tokens"], 5, "the prompt was processed");
}

#[tokio::test]
async fn max_tokens_zero_with_streaming_is_refused_before_the_upstream_is_called() {
    let (res, sent) = send_with_max_tokens(0, true, 16).await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("max_tokens: 0")
    );
    assert_eq!(sent, None, "the upstream was never called");
}
