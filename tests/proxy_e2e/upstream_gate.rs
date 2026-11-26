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
