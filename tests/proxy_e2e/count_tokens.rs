//! The /v1/messages/count_tokens endpoint
//! (mirrors `src/count_tokens.rs`).

use crate::harness::*;

/// A fake upstream that DOES have a token-counting endpoint. It records
/// every body it is sent and counts how often it was hit.
#[derive(Clone, Default)]
struct FakeCounter {
    bodies: Arc<Mutex<Vec<serde_json::Value>>>,
}

async fn start_counting_upstream(
    status: StatusCode,
    reply: &'static str,
) -> (SocketAddr, FakeCounter) {
    let state = FakeCounter::default();
    let app = Router::new()
        .route(
            "/v1/responses/input_tokens",
            post(
                move |State(s): State<FakeCounter>, body: Bytes| async move {
                    s.bodies
                        .lock()
                        .await
                        .push(serde_json::from_slice(&body).unwrap());
                    (
                        status,
                        [(axum::http::header::CONTENT_TYPE, "application/json")],
                        reply,
                    )
                },
            ),
        )
        .with_state(state.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (addr, state)
}

const COUNT_REQUEST: &str = r#"{"model":"gpt-4o","system":"be brief","messages":[{"role":"user","content":"hello there"}]}"#;

async fn count_tokens(proxy_addr: SocketAddr) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages/count_tokens"))
        .header("content-type", "application/json")
        .body(ReqBody::from(COUNT_REQUEST))
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn count_tokens_uses_the_upstream_count_when_it_has_one() {
    let (addr, counter) = start_counting_upstream(
        StatusCode::OK,
        r#"{"object":"response.input_tokens","input_tokens":42}"#,
    )
    .await;
    let mut config = make_proxy_config(addr);
    Arc::get_mut(&mut config)
        .unwrap()
        .model_aliases
        .map
        .insert("gpt-4o".into(), "gpt-4o-mini".into());
    let proxy_addr = start_proxy(config).await;

    let res = count_tokens(proxy_addr).await;

    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body, serde_json::json!({"input_tokens": 42}));
    let sent = counter.bodies.lock().await.clone();
    assert_eq!(sent.len(), 1);
    // The alias-resolved model, the translated input, and nothing that
    // only a generation request would carry.
    assert_eq!(sent[0]["model"], "gpt-4o-mini");
    assert_eq!(sent[0]["instructions"], "be brief");
    for key in ["stream", "max_output_tokens", "store", "user", "reasoning"] {
        assert!(sent[0].get(key).is_none(), "{key} was sent: {}", sent[0]);
    }
}

#[tokio::test]
async fn count_tokens_estimates_and_remembers_when_the_upstream_has_no_endpoint() {
    // The plain fake upstream only routes `/v1/responses`, so the counting
    // path answers 404, like a gateway without the endpoint.
    let (addr, upstream) = start_fake_upstream().await;
    let proxy_addr = start_proxy(make_proxy_config(addr)).await;

    let first: serde_json::Value = count_tokens(proxy_addr).await.json().await.unwrap();
    let second: serde_json::Value = count_tokens(proxy_addr).await.json().await.unwrap();

    let tokens = first["input_tokens"].as_u64().expect("a number");
    assert!(tokens > 0, "{first}");
    assert_eq!(first, second);
    // Neither call generated anything upstream.
    assert_eq!(upstream.received_all.lock().await.len(), 0);
}

#[tokio::test]
async fn count_tokens_stops_asking_once_the_upstream_said_it_has_no_endpoint() {
    let (addr, counter) =
        start_counting_upstream(StatusCode::NOT_FOUND, r#"{"error":"nope"}"#).await;
    let proxy_addr = start_proxy(make_proxy_config(addr)).await;

    for _ in 0..3 {
        let res = count_tokens(proxy_addr).await;
        assert_eq!(res.status(), StatusCode::OK);
    }

    assert_eq!(counter.bodies.lock().await.len(), 1);
}

#[tokio::test]
async fn count_tokens_keeps_asking_after_a_model_not_found() {
    let (addr, counter) = start_counting_upstream(
        StatusCode::NOT_FOUND,
        r#"{"error":{"message":"model \"gpt-4o\" not found","type":"invalid_request_error","code":"model_not_found"}}"#,
    )
    .await;
    let proxy_addr = start_proxy(make_proxy_config(addr)).await;

    for _ in 0..2 {
        assert_eq!(count_tokens(proxy_addr).await.status(), StatusCode::OK);
    }

    assert_eq!(counter.bodies.lock().await.len(), 2);
}

#[tokio::test]
async fn count_tokens_estimates_when_the_upstream_fails() {
    let (addr, _counter) =
        start_counting_upstream(StatusCode::INTERNAL_SERVER_ERROR, r#"{"error":"boom"}"#).await;
    let proxy_addr = start_proxy(make_proxy_config(addr)).await;

    let res = count_tokens(proxy_addr).await;

    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = res.json().await.unwrap();
    assert!(body["input_tokens"].as_u64().unwrap() > 0);
}

#[tokio::test]
async fn count_tokens_rejects_a_malformed_body_and_requires_the_proxy_key() {
    let (addr, _counter) = start_counting_upstream(
        StatusCode::OK,
        r#"{"object":"response.input_tokens","input_tokens":1}"#,
    )
    .await;
    let mut config = make_proxy_config(addr);
    Arc::get_mut(&mut config).unwrap().proxy_key = Some("secret".into());
    let proxy_addr = start_proxy(config).await;
    let url = format!("http://{proxy_addr}/v1/messages/count_tokens");
    let client = reqwest::Client::new();

    let no_key = client
        .post(&url)
        .header("content-type", "application/json")
        .body(COUNT_REQUEST)
        .send()
        .await
        .unwrap();
    assert_eq!(no_key.status(), StatusCode::UNAUTHORIZED);

    let bad = client
        .post(&url)
        .header("x-proxy-key", "secret")
        .header("content-type", "application/json")
        .body("not json")
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
}
