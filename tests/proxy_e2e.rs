//! End-to-end test: drive the proxy with a real Anthropic-shaped
//! request, point it at a fake in-process OpenAI Responses-shaped
//! upstream, and verify the proxy translates the request correctly and
//! the response comes back in Anthropic shape.
//!
//! The streaming path is exercised the same way, with the fake
//! upstream returning a server-sent event stream in the Responses
//! event format.

use std::collections::{BTreeMap, VecDeque};
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
use openai_to_anthropic_proxy::config::{Config, ModelAliases, ReasoningConfig};
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
    /// Per-attempt canned responses, served in order. When the vec is
    /// exhausted the handler falls back to the legacy `canned` /
    /// `canned_error` fields. Lets tests drive multi-attempt
    /// scenarios (e.g. proxy's default_model fallback) where the
    /// first send must fail and the second must succeed.
    canned_per_attempt: Arc<Mutex<VecDeque<FakeResponse>>>,
    /// The most recent JSON body the proxy sent to the upstream.
    received: Arc<Mutex<Option<Bytes>>>,
    /// Every JSON body the proxy sent to the upstream, in order.
    /// Used to assert the proxy actually retried (vs. silently
    /// succeeding on the first attempt).
    received_all: Arc<Mutex<Vec<Bytes>>>,
    /// Number of times the handler has been hit. Mirrors the
    /// length of `received_all` but exposed for quick assertions.
    request_count: Arc<Mutex<usize>>,
}

/// A scripted response the fake upstream returns on a given attempt.
struct FakeResponse {
    status: StatusCode,
    body: Option<String>,
}

async fn handle_fake(State(s): State<FakeUpstream>, body: Bytes) -> Response {
    {
        let mut all = s.received_all.lock().await;
        all.push(body.clone());
        *s.request_count.lock().await = all.len();
    }
    *s.received.lock().await = Some(body);
    // Per-attempt responses take priority. Once exhausted, fall
    // through to the legacy single-shot fields. The MutexGuard is
    // bound to its own `let` so it doesn't live across the
    // `if let` (clippy::significant_drop_in_scrutinee).
    let next = s.canned_per_attempt.lock().await.pop_front();
    if let Some(resp) = next {
        return build_fake_response(resp);
    }
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

fn build_fake_response(resp: FakeResponse) -> Response {
    match resp.body {
        Some(body) => (
            resp.status,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            body,
        )
            .into_response(),
        None => (
            resp.status,
            [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
            String::new(),
        )
            .into_response(),
    }
}

async fn start_fake_upstream() -> (SocketAddr, FakeUpstream) {
    let state = FakeUpstream::default();
    let app = Router::new()
        .route("/v1/responses", post(handle_fake))
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
        upstream_path: "/v1/responses".into(),
        request_timeout: Duration::from_secs(10),
        reasoning_effort: Some("none".into()),
        reasoning: Default::default(),
        model_aliases: Default::default(),
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

/// When the proxy's config has a per-model `reasoning_effort` map,
/// the request's `model` field should drive the lookup. Two requests
/// for two different models should produce two different
/// `reasoning.effort` values in the upstream body — proving the
/// per-model selection happens at request time, not at proxy start.
#[tokio::test]
async fn per_model_reasoning_effort_lookup() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned.lock().await = Some(
        r#"{
            "id": "resp_ok",
            "object": "response",
            "created_at": 1,
            "status": "completed",
            "model": "any",
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "ok", "annotations": []}]
            }],
            "usage": {"input_tokens":1,"output_tokens":1,"total_tokens":2}
        }"#
        .into(),
    );

    let config = Arc::new(Config {
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        upstream_base_url: format!("http://{upstream_addr}"),
        upstream_api_key: "sk-fake".into(),
        upstream_path: "/v1/responses".into(),
        request_timeout: Duration::from_secs(10),
        reasoning_effort: None,
        reasoning: ReasoningConfig {
            default: Some("medium".into()),
            models: std::iter::once(("gpt-5.6-luna".into(), "none".into())).collect(),
        },
        model_aliases: Default::default(),
    });
    let proxy_addr = start_proxy(config).await;

    let client = reqwest::Client::new();

    // Model with an explicit entry → "none".
    let _ = client
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(
            r#"{"model":"gpt-5.6-luna","max_tokens":4,"messages":[{"role":"user","content":"a"}]}"#,
        ))
        .send()
        .await
        .unwrap();
    let first = upstream.received.lock().await.clone().unwrap();
    let first: serde_json::Value = serde_json::from_slice(&first).unwrap();
    assert_eq!(first["reasoning"]["effort"], "none");

    // Model not in the map → falls back to default "medium".
    let _ = client
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(
            r#"{"model":"gpt-5.4-mini","max_tokens":4,"messages":[{"role":"user","content":"b"}]}"#,
        ))
        .send()
        .await
        .unwrap();
    let second = upstream.received.lock().await.clone().unwrap();
    let second: serde_json::Value = serde_json::from_slice(&second).unwrap();
    assert_eq!(second["reasoning"]["effort"], "medium");
}

/// When the proxy's config has a `model_aliases` map, an inbound
/// `model` field that matches an alias key is rewritten to the alias
/// value before being sent to the upstream. This is the fix for
/// `claude-sonnet-5` (sent by Claude Code subagents) hitting an
/// airia gateway that only knows OpenAI-family model names.
#[tokio::test]
async fn model_alias_rewrites_inbound_model() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned.lock().await = Some(
        r#"{
            "id": "resp_ok",
            "object": "response",
            "created_at": 1,
            "status": "completed",
            "model": "any",
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "ok", "annotations": []}]
            }],
            "usage": {"input_tokens":1,"output_tokens":1,"total_tokens":2}
        }"#
        .into(),
    );

    let config = Arc::new(Config {
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        upstream_base_url: format!("http://{upstream_addr}"),
        upstream_api_key: "sk-fake".into(),
        upstream_path: "/v1/responses".into(),
        request_timeout: Duration::from_secs(10),
        reasoning_effort: Some("none".into()),
        reasoning: Default::default(),
        model_aliases: ModelAliases {
            map: std::iter::once(("claude-sonnet-5".into(), "gpt-5.4-mini".into())).collect(),
            default_model: None,
        },
    });
    let proxy_addr = start_proxy(config).await;

    let client = reqwest::Client::new();
    let _ = client
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(
            r#"{"model":"claude-sonnet-5","max_tokens":4,"messages":[{"role":"user","content":"a"}]}"#,
        ))
        .send()
        .await
        .unwrap();

    let body = upstream.received.lock().await.clone().unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    // The upstream saw the alias-resolved name, not the inbound one.
    assert_eq!(body["model"], "gpt-5.4-mini");
}

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
            status: StatusCode::BAD_REQUEST,
            body: Some(
                r#"{"message":"The requested model claude-sonnet-5 is not supported for the selected provider.","type":"invalid_request_error","code":"model_not_supported"}"#.into(),
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
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        upstream_base_url: format!("http://{upstream_addr}"),
        upstream_api_key: "sk-fake".into(),
        upstream_path: "/v1/responses".into(),
        request_timeout: Duration::from_secs(10),
        reasoning_effort: Some("none".into()),
        reasoning: Default::default(),
        model_aliases: ModelAliases {
            map: BTreeMap::new(),
            default_model: Some("gpt-4o-mini".into()),
        },
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
        status: StatusCode::BAD_REQUEST,
        body: Some(
            r#"{"message":"The requested model claude-sonnet-5 is not supported for the selected provider.","type":"invalid_request_error"}"#.into(),
        ),
    }]);

    let config = Arc::new(Config {
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        upstream_base_url: format!("http://{upstream_addr}"),
        upstream_api_key: "sk-fake".into(),
        upstream_path: "/v1/responses".into(),
        request_timeout: Duration::from_secs(10),
        reasoning_effort: Some("none".into()),
        reasoning: Default::default(),
        // Note: no default_model — fallback disabled.
        model_aliases: Default::default(),
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
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value = res.json().await.unwrap();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not supported for the selected provider"),
        "expected upstream body, got {body}"
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
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        upstream_base_url: format!("http://{upstream_addr}"),
        upstream_api_key: "sk-fake".into(),
        upstream_path: "/v1/responses".into(),
        request_timeout: Duration::from_secs(10),
        reasoning_effort: Some("none".into()),
        reasoning: Default::default(),
        model_aliases: ModelAliases {
            map: BTreeMap::new(),
            default_model: Some("gpt-4o-mini".into()),
        },
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

/// Tools in the outbound Responses request must include
/// `strict: true` and `additionalProperties: false` on the parameters
/// schema. Without these, the airia gateway returns 400
/// `invalid_function_parameters`. This is the headline behavior
/// change of the migration: the proxy bridges Claude Code's
/// lenient tools into the strict Responses shape.
#[tokio::test]
async fn tools_get_strict_and_additional_properties_false() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned.lock().await = Some(
        r#"{
            "id": "resp_ok",
            "object": "response",
            "created_at": 1,
            "status": "completed",
            "model": "gpt-4o",
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "ok", "annotations": []}]
            }],
            "usage": {"input_tokens":1,"output_tokens":1,"total_tokens":2}
        }"#
        .into(),
    );
    let config = make_proxy_config(upstream_addr);
    let proxy_addr = start_proxy(config).await;

    let client = reqwest::Client::new();
    let _ = client
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(
            r#"{
                "model": "gpt-4o",
                "max_tokens": 4,
                "messages": [{"role": "user", "content": "x"}],
                "tools": [{
                    "name": "get_weather",
                    "description": "Get the weather",
                    "input_schema": {
                        "type": "object",
                        "properties": {"location": {"type": "string"}}
                    }
                }]
            }"#,
        ))
        .send()
        .await
        .unwrap();

    let body = upstream.received.lock().await.clone().unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let tools = body["tools"].as_array().expect("tools is array");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["type"], "function");
    assert!(tools[0]["strict"].as_bool().unwrap());
    assert_eq!(
        tools[0]["parameters"]["additionalProperties"],
        serde_json::Value::Bool(false)
    );
    // Properties object is preserved (not overwritten) when already present.
    assert_eq!(
        tools[0]["parameters"]["properties"],
        serde_json::json!({"location": {"type": "string"}})
    );
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
            status: StatusCode::BAD_REQUEST,
            body: Some(
                r#"{"message":"The requested model claude-sonnet-5 is not supported for the selected provider.","type":"invalid_request_error","code":"model_not_supported"}"#.into(),
            ),
        },
        FakeResponse {
            status: StatusCode::OK,
            body: Some(fallback_sse),
        },
    ]);

    let config = Arc::new(Config {
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        upstream_base_url: format!("http://{upstream_addr}"),
        upstream_api_key: "sk-fake".into(),
        upstream_path: "/v1/responses".into(),
        request_timeout: Duration::from_secs(10),
        reasoning_effort: Some("none".into()),
        reasoning: Default::default(),
        model_aliases: ModelAliases {
            map: BTreeMap::new(),
            default_model: Some("gpt-4o-mini".into()),
        },
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
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        upstream_base_url: format!("http://{upstream_addr}"),
        upstream_api_key: "sk-fake".into(),
        upstream_path: "/v1/responses".into(),
        request_timeout: Duration::from_secs(10),
        reasoning_effort: None,
        reasoning: ReasoningConfig {
            default: Some("low".into()),
            models: std::iter::once(("gpt-4o-mini".into(), "none".into())).collect(),
        },
        model_aliases: ModelAliases {
            map: BTreeMap::new(),
            default_model: Some("gpt-4o-mini".into()),
        },
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
