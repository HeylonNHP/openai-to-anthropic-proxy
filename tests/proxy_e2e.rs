//! End-to-end test: drive the proxy with a real Anthropic-shaped
//! request, point it at a fake in-process OpenAI-shaped upstream, and
//! verify the proxy translates the request correctly and the response
//! comes back in Anthropic shape.
//!
//! The streaming path is exercised the same way, with the fake
//! upstream returning a server-sent event stream.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use bytes::Bytes;
use openai_to_anthropic_proxy::config::Config;
use reqwest::Body as ReqBody;
use tokio::net::TcpListener;
use tokio::sync::Mutex;

/// State for the fake upstream.
#[derive(Clone, Default)]
struct FakeUpstream {
    /// Canned JSON body to return for any POST.
    canned: Arc<Mutex<Option<String>>>,
    /// Canned SSE body to return for any POST.
    canned_stream: Arc<Mutex<Option<String>>>,
    /// Canned error response (status + body) to return.
    canned_error: Arc<Mutex<Option<(StatusCode, String)>>>,
    /// The most recent JSON body the proxy sent to the upstream.
    received: Arc<Mutex<Option<Bytes>>>,
}

async fn handle_fake(State(s): State<FakeUpstream>, body: Bytes) -> Response {
    *s.received.lock().await = Some(body);
    let err = s.canned_error.lock().await.clone();
    if let Some((status, body)) = err {
        return (
            status,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            body,
        )
            .into_response();
    }
    let stream = s.canned_stream.lock().await.clone();
    if let Some(stream) = stream {
        return Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .body(Body::from(stream))
            .unwrap();
    }
    let canned = s.canned.lock().await.clone();
    if let Some(canned) = canned {
        return Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .body(Body::from(canned))
            .unwrap();
    }
    (StatusCode::INTERNAL_SERVER_ERROR, "no canned response").into_response()
}

async fn start_fake_upstream() -> (SocketAddr, FakeUpstream) {
    let state = FakeUpstream::default();
    let app = Router::new()
        .route("/v1/chat/completions", post(handle_fake))
        .with_state(state.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (addr, state)
}

fn make_proxy_config(addr: SocketAddr) -> Arc<Config> {
    Arc::new(Config {
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        upstream_base_url: format!("http://{addr}"),
        upstream_api_key: "sk-fake".into(),
        upstream_path: "/v1/chat/completions".into(),
        request_timeout: Duration::from_secs(10),
    })
}

async fn start_proxy(config: Arc<Config>) -> SocketAddr {
    let client = reqwest::Client::builder()
        .timeout(config.request_timeout)
        .build()
        .unwrap();
    let app = openai_to_anthropic_proxy::proxy::router(config, client);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

#[tokio::test]
async fn non_streaming_round_trip() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned.lock().await = Some(
        r#"{
            "id": "chatcmpl-abc",
            "object": "chat.completion",
            "created": 1,
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "Hello from upstream!"
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 5,
                "completion_tokens": 3,
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

    // Verify the upstream got a translated OpenAI request.
    let received = upstream.received.lock().await.clone().unwrap();
    let received: serde_json::Value = serde_json::from_slice(&received).unwrap();
    assert_eq!(received["model"], "gpt-4o");
    assert_eq!(received["stream"], false);
    assert_eq!(received["messages"][0]["role"], "user");
}

#[tokio::test]
async fn streaming_round_trip() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    let sse = [
        r#"data: {"id":"c1","object":"chat.completion.chunk","created":0,"model":"gpt-4o","choices":[{"index":0,"delta":{"role":"assistant","content":null},"finish_reason":null}]}"#,
        r#"data: {"id":"c1","object":"chat.completion.chunk","created":0,"model":"gpt-4o","choices":[{"index":0,"delta":{"content":"Hi"},"finish_reason":null}]}"#,
        r#"data: {"id":"c1","object":"chat.completion.chunk","created":0,"model":"gpt-4o","choices":[{"index":0,"delta":{"content":" there"},"finish_reason":null}]}"#,
        r#"data: {"id":"c1","object":"chat.completion.chunk","created":0,"model":"gpt-4o","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":4,"completion_tokens":2,"total_tokens":6}}"#,
        r#"data: [DONE]"#,
    ].join("\n\n");
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
async fn upstream_error_returns_502_with_message() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned_error.lock().await = Some((
        StatusCode::UNAUTHORIZED,
        r#"{"error":{"message":"bad key","type":"authentication_error","code":"invalid_api_key"}}"#
            .into(),
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
