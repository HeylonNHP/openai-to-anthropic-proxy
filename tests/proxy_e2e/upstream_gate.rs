//! 429 retry with backoff and the concurrency limit
//! (mirrors `src/upstream_gate.rs`).

use crate::harness::*;

// ── upstream concurrency limit and 429 retry ────────────

/// Post one tiny non-streaming request to the proxy.
async fn post_small(proxy_addr: SocketAddr) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(
            r#"{"model":"gpt-4o","max_tokens":16,"messages":[{"role":"user","content":"x"}]}"#,
        ))
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn a_429_that_clears_within_the_retries_never_reaches_the_client() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned.lock().await = Some(CANNED_RESPONSE.into());
    *upstream.canned_per_attempt.lock().await = VecDeque::from([
        FakeResponse {
            status: StatusCode::TOO_MANY_REQUESTS,
            body: Some(r#"{"error":"too many concurrent requests"}"#.into()),
        },
        FakeResponse {
            status: StatusCode::TOO_MANY_REQUESTS,
            body: Some(r#"{"error":"too many concurrent requests"}"#.into()),
        },
    ]);
    let mut config = (*make_proxy_config(upstream_addr)).clone();
    config.rate_limit_retries = 3;
    let proxy_addr = start_proxy(Arc::new(config)).await;

    let res = post_small(proxy_addr).await;

    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(*upstream.request_count.lock().await, 3);
}

#[tokio::test]
async fn a_429_that_outlasts_the_retries_is_reported_as_a_rate_limit() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned_error.lock().await = Some((
        StatusCode::TOO_MANY_REQUESTS,
        r#"{"error":"too many concurrent requests"}"#.into(),
    ));
    let mut config = (*make_proxy_config(upstream_addr)).clone();
    config.rate_limit_retries = 2;
    let proxy_addr = start_proxy(Arc::new(config)).await;

    let res = post_small(proxy_addr).await;

    assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["error"]["type"], "rate_limit_error");
    // The first send plus two retries.
    assert_eq!(*upstream.request_count.lock().await, 3);
}

#[tokio::test]
async fn only_a_429_is_retried() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned_error.lock().await = Some((
        StatusCode::SERVICE_UNAVAILABLE,
        r#"{"error":{"message":"busy"}}"#.into(),
    ));
    let mut config = (*make_proxy_config(upstream_addr)).clone();
    config.rate_limit_retries = 3;
    let proxy_addr = start_proxy(Arc::new(config)).await;

    let res = post_small(proxy_addr).await;

    assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(*upstream.request_count.lock().await, 1);
}

#[tokio::test]
async fn requests_past_the_concurrency_limit_wait_instead_of_failing() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned.lock().await = Some(CANNED_RESPONSE.into());
    *upstream.delay.lock().await = Some(Duration::from_millis(200));
    let mut config = (*make_proxy_config(upstream_addr)).clone();
    config.max_concurrent_requests = 1;
    let proxy_addr = start_proxy(Arc::new(config)).await;

    let started = std::time::Instant::now();
    let replies = futures_util::future::join_all((0..3).map(|_| post_small(proxy_addr))).await;
    let elapsed = started.elapsed();

    // Every client got a normal answer; none saw a queue or an error.
    for res in &replies {
        assert_eq!(res.status(), StatusCode::OK);
    }
    // One at a time: three 200 ms requests cannot finish in much under 600 ms.
    assert!(
        elapsed >= Duration::from_millis(550),
        "requests overlapped despite the limit: {elapsed:?}"
    );
    assert_eq!(*upstream.request_count.lock().await, 3);
}

#[tokio::test]
async fn no_configured_limit_lets_requests_overlap() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned.lock().await = Some(CANNED_RESPONSE.into());
    *upstream.delay.lock().await = Some(Duration::from_millis(300));
    let proxy_addr = start_proxy(make_proxy_config(upstream_addr)).await;

    let started = std::time::Instant::now();
    let replies = futures_util::future::join_all((0..3).map(|_| post_small(proxy_addr))).await;
    let elapsed = started.elapsed();

    for res in &replies {
        assert_eq!(res.status(), StatusCode::OK);
    }
    assert!(
        elapsed < Duration::from_millis(850),
        "requests were serialised although no limit is set: {elapsed:?}"
    );
}

// ── early start: a slow upstream never leaves the client in silence ──

/// Post a tiny streaming request to the proxy.
async fn post_streaming(proxy_addr: SocketAddr) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(
            r#"{"model":"gpt-4o","max_tokens":16,"stream":true,"messages":[{"role":"user","content":"x"}]}"#,
        ))
        .send()
        .await
        .unwrap()
}

/// A streaming request whose upstream answer takes longer than the
/// early-start deadline — queueing, 429 backoff, plain slowness — gets the
/// placeholder `message_start` and `ping`s instead of minutes of silence
/// that would trip the client's byte watchdog. The real reply then splices
/// in behind the placeholder: still exactly one `message_start`, and the
/// input-side usage survives in the closing `message_delta`.
#[tokio::test(start_paused = true)]
async fn a_slow_upstream_stream_starts_early_with_a_placeholder_and_pings() {
    use futures_util::StreamExt;

    let (upstream_addr, upstream) = start_fake_upstream().await;
    let sse = [
        r#"event: response.created
data: {"type":"response.created","response":{"id":"resp_1","object":"response","created_at":0,"status":"in_progress","model":"gpt-4o","output":[]}}"#,
        r#"event: response.output_item.added
data: {"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_x","status":"in_progress","role":"assistant","content":[]}}"#,
        r#"event: response.output_text.delta
data: {"type":"response.output_text.delta","item_id":"msg_x","output_index":0,"content_index":0,"delta":"Hi there"}"#,
        r#"event: response.completed
data: {"type":"response.completed","response":{"id":"resp_1","object":"response","created_at":0,"status":"completed","model":"gpt-4o","output":[],"usage":{"input_tokens":4,"output_tokens":2,"total_tokens":6}}}"#,
    ].join("\n\n") + "\n\n";
    *upstream.canned_stream.lock().await = Some(sse);
    // Far past `early_start::START_AFTER` (one ping interval), but inside
    // the request timeout below.
    *upstream.delay.lock().await = Some(Duration::from_secs(30));
    let mut config = (*make_proxy_config(upstream_addr)).clone();
    config.request_timeout = Duration::from_secs(60);
    let proxy_addr = start_proxy(Arc::new(config)).await;

    let res = post_streaming(proxy_addr).await;
    assert_eq!(res.status(), StatusCode::OK);

    let mut stream = res.bytes_stream();
    let first = stream.next().await.unwrap().unwrap();
    let first = String::from_utf8_lossy(&first);
    assert!(first.contains("event: message_start"), "{first}");
    assert!(first.contains(r#""model":"gpt-4o""#), "{first}");

    // The upstream is still silent a full ping interval later: a
    // keep-alive, not a hang.
    let second = stream.next().await.unwrap().unwrap();
    let second = String::from_utf8_lossy(&second);
    assert!(second.starts_with("event: ping"), "{second}");

    // The real reply splices in behind the placeholder.
    let mut rest = String::new();
    while let Some(chunk) = stream.next().await {
        rest.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
    }
    assert!(rest.contains("Hi there"), "{rest}");
    assert_eq!(
        first.matches("event: message_start").count()
            + rest.matches("event: message_start").count(),
        1,
        "the spliced translator must not reopen the message: {rest}"
    );
    assert_eq!(rest.matches("event: message_stop").count(), 1, "{rest}");
    // The placeholder's usage was zero; the closing delta carries the
    // turn's real input-side numbers, which is where a client reads them.
    assert!(rest.contains(r#""input_tokens":4"#), "{rest}");
}

/// Within the early-start deadline a failure is still a real HTTP status:
/// a streaming client gets 429 with its status-driven backoff-retry
/// treatment, not a committed 200 whose stream can only carry an
/// in-stream error. This is the rule the deadline exists to preserve.
#[tokio::test]
async fn a_fast_429_on_a_streaming_request_is_still_an_http_status() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned_error.lock().await = Some((
        StatusCode::TOO_MANY_REQUESTS,
        r#"{"error":"too many concurrent requests"}"#.into(),
    ));
    let mut config = (*make_proxy_config(upstream_addr)).clone();
    config.rate_limit_retries = 2;
    let proxy_addr = start_proxy(Arc::new(config)).await;

    // The retries' backoff (~1.75 s) fits inside the early-start deadline,
    // so the exhausted failure answers as the status it is.
    let res = post_streaming(proxy_addr).await;
    assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["error"]["type"], "rate_limit_error");
    // The first send plus two retries.
    assert_eq!(*upstream.request_count.lock().await, 3);
}
