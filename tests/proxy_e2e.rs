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
use openai_to_anthropic_proxy::config::{
    AdvisorConfig, Config, ModelAliases, ReasoningConfig, ServerToolsConfig, ToolResultImages,
    WebFetchConfig, WebSearchConfig,
};
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
    /// Extra response headers sent with `canned_error`.
    canned_error_headers: Arc<Mutex<Vec<(String, String)>>>,
    /// Per-attempt canned responses, served in order. When the vec is
    /// exhausted the handler falls back to the legacy `canned` /
    /// `canned_error` fields. Lets tests drive multi-attempt
    /// scenarios (e.g. proxy's default_model fallback) where the
    /// first send must fail and the second must succeed.
    canned_per_attempt: Arc<Mutex<VecDeque<FakeResponse>>>,
    /// Optional delay before returning a canned response.
    delay: Arc<Mutex<Option<Duration>>>,
    /// Per-attempt delay, consumed in order. Lets a test stall ONLY the
    /// advisor POST (attempt 2) while the executor calls stay fast.
    delay_per_attempt: Arc<Mutex<VecDeque<Duration>>>,
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
    // Record the request BEFORE any delay: a client that times out must
    // still have its POST visible in `received_all`.
    {
        let mut all = s.received_all.lock().await;
        all.push(body.clone());
        *s.request_count.lock().await = all.len();
    }
    // Claim the scripted response BEFORE the delay. A request the client
    // times out on has still consumed its slot; otherwise a later (fast)
    // request would pop the stalled response instead of its own. The
    // MutexGuard is bound to its own `let` so it doesn't live across the
    // `if let` (clippy::significant_drop_in_scrutinee).
    let next = s.canned_per_attempt.lock().await.pop_front();
    let delay = *s.delay.lock().await;
    if let Some(delay) = delay {
        tokio::time::sleep(delay).await;
    }
    let attempt_delay = s.delay_per_attempt.lock().await.pop_front();
    if let Some(delay) = attempt_delay {
        tokio::time::sleep(delay).await;
    }
    *s.received.lock().await = Some(body);
    // Per-attempt responses take priority. Once exhausted, fall
    // through to the legacy single-shot fields.
    if let Some(resp) = next {
        return build_fake_response(resp);
    }
    let err = s.canned_error.lock().await.clone();
    if let Some((status, body)) = err {
        let mut response = (
            status,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            body,
        )
            .into_response();
        for (name, value) in s.canned_error_headers.lock().await.iter() {
            response.headers_mut().insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
        }
        return response;
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
        // A body written as SSE (`sse_run` builds one) is served as a stream.
        Some(body) if body.starts_with("event: ") => (
            resp.status,
            [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
            body,
        )
            .into_response(),
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

/// The error body the production gateway actually sends when it does not
/// know a model. Taken verbatim from `proxy.log.2026-11-12`:
///
/// ```text
/// ← upstream error status=404 Not Found body={"error":{"message":"model \"claude-sonnet-5\" not found. Try glm-5.3-flash:cloud.","type":"not_found_error","param":null,"code":null}}
/// ```
///
/// Note the **nested** envelope, the model name sitting *between* "model"
/// and "not found", and `code: null` — none of which the old detector
/// could see.
const REAL_GATEWAY_MODEL_404: &str = r#"{"error":{"message":"model \"claude-sonnet-5\" not found. Try glm-5.3-flash:cloud.","type":"not_found_error","param":null,"code":null}}"#;

/// The same rejection as the gateway sent it on 2026-08-16 — **single**
/// quotes, and no "Try" hint.
const REAL_GATEWAY_MODEL_404_SINGLE_QUOTED: &str = r#"{"error":{"message":"model 'claude-opus-5' not found","type":"not_found_error","param":null,"code":null}}"#;

/// A real non-model upstream error (2026-09-29). It contains the words
/// "not supported", so a detector that matched those loosely would fire
/// the model fallback on a *tool* rejection.
const REAL_GATEWAY_WEB_SEARCH_400: &str = r#"{"error":{"message":"the web_search tool is not supported (ref: 490f64b3-1e45-4671-aaef-8e7ff3a71f4b)","type":"invalid_request_error","param":null,"code":null}}"#;

/// A proxy config with an inbound→upstream alias map and a
/// `default_model`, for the model-fallback tests.
fn make_config_with_default_model(
    upstream_addr: SocketAddr,
    map: BTreeMap<String, String>,
    default_model: Option<&str>,
) -> Arc<Config> {
    Arc::new(Config {
        model_info: Default::default(),
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        upstream_base_url: format!("http://{upstream_addr}"),
        upstream_api_key: "test-key".into(),
        upstream_path: "/v1/responses".into(),
        request_timeout: Duration::from_secs(10),
        max_request_body_bytes: openai_to_anthropic_proxy::config::DEFAULT_MAX_REQUEST_BODY_BYTES,
        reasoning_effort: Some("none".into()),
        reasoning: Default::default(),
        reasoning_replay: Default::default(),
        model_aliases: ModelAliases {
            map,
            default_model: default_model.map(str::to_owned),
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
        system_message_placement: Default::default(),
        warnings: Vec::new(),
    })
}

async fn start_fake_upstream() -> (SocketAddr, FakeUpstream) {
    let state = FakeUpstream::default();
    let app = Router::new()
        .route("/v1/responses", post(handle_fake))
        .layer(axum::extract::DefaultBodyLimit::max(64 * 1024 * 1024))
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
        model_info: Default::default(),
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        upstream_base_url: format!("http://{addr}"),
        upstream_api_key: "sk-fake".into(),
        upstream_path: "/v1/responses".into(),
        request_timeout: Duration::from_secs(10),
        max_request_body_bytes: openai_to_anthropic_proxy::config::DEFAULT_MAX_REQUEST_BODY_BYTES,
        reasoning_effort: Some("none".into()),
        reasoning: Default::default(),
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
        system_message_placement: Default::default(),
        warnings: Vec::new(),
    })
}

async fn start_proxy(config: Arc<Config>) -> SocketAddr {
    use openai_to_anthropic_proxy::{MappingsStore, RuntimeMappings};
    let store = Arc::new(MappingsStore::seeded(Arc::new(
        RuntimeMappings::from_config(&config),
    )));
    start_proxy_with_store(config, store).await
}

/// Like `start_proxy`, but lets the caller inject a pre-built
/// `MappingsStore` so tests can drive runtime-edits of the alias
/// table (the contract the TUI exposes).
async fn start_proxy_with_store(
    config: Arc<Config>,
    store: Arc<openai_to_anthropic_proxy::MappingsStore>,
) -> SocketAddr {
    let client = reqwest::Client::builder()
        .timeout(config.request_timeout)
        .build()
        .unwrap();
    let app = openai_to_anthropic_proxy::proxy::router(
        config,
        store,
        client,
        openai_to_anthropic_proxy::OutputSink::plain(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

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
        model_info: Default::default(),
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        upstream_base_url: format!("http://{upstream_addr}"),
        upstream_api_key: "sk-fake".into(),
        upstream_path: "/v1/responses".into(),
        request_timeout: Duration::from_secs(10),
        max_request_body_bytes: openai_to_anthropic_proxy::config::DEFAULT_MAX_REQUEST_BODY_BYTES,
        reasoning_effort: None,
        reasoning: ReasoningConfig {
            default: Some("medium".into()),
            models: std::iter::once(("gpt-5.6-luna".into(), "none".into())).collect(),
            ..ReasoningConfig::default()
        },
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
        system_message_placement: Default::default(),
        warnings: Vec::new(),
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

/// The proxy asks the upstream for a reasoning summary whenever reasoning is
/// on, because an OpenAI-style upstream returns reasoning text only when asked.
/// It asks for none when reasoning is `none`, or for a model switched `off`.
#[tokio::test]
async fn reasoning_summary_is_requested_unless_off_or_reasoning_is_none() {
    use openai_to_anthropic_proxy::config::ReasoningSummary;

    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned.lock().await = Some(
        r#"{
            "id": "resp_ok", "object": "response", "created_at": 1,
            "status": "completed", "model": "any",
            "output": [{"type": "message", "role": "assistant",
                "content": [{"type": "output_text", "text": "ok", "annotations": []}]}],
            "usage": {"input_tokens":1,"output_tokens":1,"total_tokens":2}
        }"#
        .into(),
    );

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
            default: Some("medium".into()),
            models: std::iter::once(("no-reasoning".into(), "none".into())).collect(),
            summary_models: std::iter::once(("picky-upstream".into(), ReasoningSummary::Off))
                .collect(),
            ..ReasoningConfig::default()
        },
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
        system_message_placement: Default::default(),
        warnings: Vec::new(),
    });
    let proxy_addr = start_proxy(config).await;
    let client = reqwest::Client::new();

    let send = |model: &'static str| {
        let client = client.clone();
        let upstream = upstream.clone();
        async move {
            let body = format!(
                r#"{{"model":"{model}","max_tokens":4,"messages":[{{"role":"user","content":"a"}}]}}"#
            );
            let _ = client
                .post(format!("http://{proxy_addr}/v1/messages"))
                .header("content-type", "application/json")
                .body(ReqBody::from(body))
                .send()
                .await
                .unwrap();
            let raw = upstream.received.lock().await.clone().unwrap();
            serde_json::from_slice::<serde_json::Value>(&raw).unwrap()
        }
    };

    // Reasoning on (default effort "medium") → a summary is requested.
    let on = send("gpt-5.4-mini").await;
    assert_eq!(on["reasoning"]["effort"], "medium");
    assert_eq!(on["reasoning"]["summary"], "auto");

    // Reasoning "none" → no summary field.
    let none = send("no-reasoning").await;
    assert_eq!(none["reasoning"]["effort"], "none");
    assert!(none["reasoning"].get("summary").is_none());

    // A model switched off → effort still sent, no summary field.
    let off = send("picky-upstream").await;
    assert_eq!(off["reasoning"]["effort"], "medium");
    assert!(off["reasoning"].get("summary").is_none());
}

/// A Claude Code request that carries `output_config.effort` (or
/// `thinking.type = "disabled"`) is translated through the configured
/// effort/thinking-disabled maps before it reaches the upstream.
///
/// This is the wire-level proof for the feature:
///   - `output_config.effort = "high"` → `reasoning.effort = "medium"`
///   - `thinking.type = "disabled"` → `reasoning.effort = "none"`
#[tokio::test]
async fn effort_map_and_thinking_disabled_flow_to_upstream() {
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

    // Configure an effort_map (high→medium) and a thinking_disabled
    // mapping (disabled→none), both under the `default` key.
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
            default: Some("high".into()), // fixed fallback when no signal
            models: Default::default(),
            effort_map: openai_to_anthropic_proxy::config::EffortMap {
                default: BTreeMap::from([("high".into(), Some("medium".into()))]),
                models: Default::default(),
            },
            thinking_disabled: openai_to_anthropic_proxy::config::EffortMap {
                default: BTreeMap::from([("disabled".into(), Some("none".into()))]),
                models: Default::default(),
            },
            ..ReasoningConfig::default()
        },
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
        system_message_placement: Default::default(),
        warnings: Vec::new(),
    });
    let proxy_addr = start_proxy(config).await;
    let client = reqwest::Client::new();

    // Request with output_config.effort = "high" → mapped to "medium".
    let _ = client
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(
            r#"{"model":"gpt-5.4-mini","max_tokens":4,"output_config":{"effort":"high"},"messages":[{"role":"user","content":"a"}]}"#,
        ))
        .send()
        .await
        .unwrap();
    let first = upstream.received.lock().await.clone().unwrap();
    let first: serde_json::Value = serde_json::from_slice(&first).unwrap();
    assert_eq!(first["reasoning"]["effort"], "medium");

    // Request with thinking disabled → mapped to "none", and it must
    // NOT forward any Anthropic `thinking` object.
    let _ = client
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(
            r#"{"model":"gpt-5.4-mini","max_tokens":4,"thinking":{"type":"disabled"},"messages":[{"role":"user","content":"b"}]}"#,
        ))
        .send()
        .await
        .unwrap();
    let second = upstream.received.lock().await.clone().unwrap();
    let second: serde_json::Value = serde_json::from_slice(&second).unwrap();
    assert_eq!(second["reasoning"]["effort"], "none");
    assert!(
        second.get("thinking").is_none(),
        "Anthropic thinking object must not be forwarded upstream"
    );
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
            map: std::iter::once(("claude-sonnet-5".into(), "gpt-5.4-mini".into())).collect(),
            default_model: None,
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
        system_message_placement: Default::default(),
        warnings: Vec::new(),
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
    // Properties object is preserved (not overwritten) when already
    // present. `location` is NOT in the original schema's `required`,
    // so the strict encoding marks it nullable (`["string","null"]`)
    // while still listing it in `required` — the model signals
    // "omitted" with `null` and the proxy strips the sentinel on the
    // response side (src/repair.rs).
    assert_eq!(
        tools[0]["parameters"]["properties"],
        serde_json::json!({"location": {"type": ["string", "null"]}})
    );
    assert_eq!(
        tools[0]["parameters"]["required"],
        serde_json::json!(["location"])
    );
}

/// A client's echoed `thinking` block must reach the upstream. The proxy
/// used to drop it outright (no `Thinking` variant in the inbound enum),
/// which destroyed the model's memory of its own prior reasoning on every
/// turn — 10,467 drops on 2026-11-12 alone.
#[tokio::test]
async fn thinking_block_is_replayed_to_the_upstream() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned.lock().await = Some(
        r#"{
            "id": "resp_1",
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
                "messages": [
                    {"role": "user", "content": "x"},
                    {"role": "assistant", "content": [
                        {"type": "thinking", "thinking": "the model reasoned about this", "signature": "SIG"},
                        {"type": "text", "text": "the answer"}
                    ]}
                ]
            }"#,
        ))
        .send()
        .await
        .unwrap();

    let body = upstream.received.lock().await.clone().unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let input = body["input"].as_array().expect("input is array");
    let assistant = input
        .iter()
        .find(|i| i["role"] == "assistant")
        .expect("assistant message in the upstream body");
    let text = assistant["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("the model reasoned about this"),
        "the reasoning did not reach the upstream: {text:?}"
    );
    assert!(text.contains("the answer"));
    // Text mode must not introduce a new input-item type — only
    // `message` / `function_call` / `function_call_output` are ever sent,
    // because those are the only shapes proven to work against an
    // upstream we know nothing about.
    assert!(
        input.iter().all(|i| matches!(
            i["type"].as_str(),
            Some("message" | "function_call" | "function_call_output")
        )),
        "unexpected input item types: {input:?}"
    );
}

/// A wrong-typed field inside a thinking block must NOT fail the whole
/// request. Before the block was recognised it fell into `Unknown` and was
/// merely dropped; recognising it must not turn a tolerated block into a
/// 400 for the entire conversation.
#[tokio::test]
async fn a_wrong_typed_thinking_field_does_not_fail_the_request() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned.lock().await = Some(
        r#"{
            "id": "resp_1",
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
    let res = client
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(
            r#"{
                "model": "gpt-4o",
                "max_tokens": 4,
                "messages": [
                    {"role": "user", "content": "x"},
                    {"role": "assistant", "content": [
                        {"type": "thinking", "thinking": 123, "signature": "SIG"},
                        {"type": "text", "text": "the answer"}
                    ]}
                ]
            }"#,
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        StatusCode::OK,
        "a wrong-typed thinking field must not 400 the whole request"
    );
    // The upstream must have been called.
    assert!(
        upstream.received.lock().await.is_some(),
        "the upstream was never called"
    );
}

/// `reasoning_replay: "off"` restores the old behaviour: the thinking
/// block is dropped, and the upstream sees only the visible answer.
#[tokio::test]
async fn reasoning_replay_off_drops_the_thinking_block() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned.lock().await = Some(
        r#"{
            "id": "resp_1",
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
    let mut config = (*make_proxy_config(upstream_addr)).clone();
    config.reasoning_replay.default = openai_to_anthropic_proxy::config::ReasoningReplayMode::Off;
    let proxy_addr = start_proxy(Arc::new(config)).await;

    let client = reqwest::Client::new();
    let _ = client
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(
            r#"{
                "model": "gpt-4o",
                "max_tokens": 4,
                "messages": [
                    {"role": "user", "content": "x"},
                    {"role": "assistant", "content": [
                        {"type": "thinking", "thinking": "the model reasoned about this", "signature": "SIG"},
                        {"type": "text", "text": "the answer"}
                    ]}
                ]
            }"#,
        ))
        .send()
        .await
        .unwrap();

    let body = upstream.received.lock().await.clone().unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let input = body["input"].as_array().expect("input is array");
    let assistant = input
        .iter()
        .find(|i| i["role"] == "assistant")
        .expect("assistant message in the upstream body");
    let text = assistant["content"][0]["text"].as_str().unwrap();
    assert_eq!(text, "the answer");
    assert!(!text.contains("the model reasoned about this"));
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

/// disconnected from request handling: TUI edits to model aliases
/// had no effect on actual requests because the handler read from
/// the static `Config::model_aliases`. This test mutates the
/// `MappingsStore` directly — the same call the TUI makes when the
/// operator hits `e` — and asserts the next request is routed to
/// the new upstream name.
#[tokio::test]
async fn runtime_alias_edit_takes_effect_on_next_request() {
    use openai_to_anthropic_proxy::MappingsStore;
    use std::collections::BTreeMap;

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

    // Config has NO aliases for `claude-sonnet-5`. The runtime store
    // is empty too — a freshly-started proxy.
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
        system_message_placement: Default::default(),
        warnings: Vec::new(),
    });
    let store = Arc::new(MappingsStore::seeded(Arc::new(
        openai_to_anthropic_proxy::RuntimeMappings::default(),
    )));
    let proxy_addr = start_proxy_with_store(config, store.clone()).await;

    let client = reqwest::Client::new();

    // 1. Pre-edit: the upstream sees the inbound name verbatim because
    // no alias exists yet.
    let _ = client
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(
            r#"{"model":"claude-sonnet-5","max_tokens":4,"messages":[{"role":"user","content":"a"}]}"#,
        ))
        .send()
        .await
        .unwrap();
    let first = upstream.received.lock().await.clone().unwrap();
    let first: serde_json::Value = serde_json::from_slice(&first).unwrap();
    assert_eq!(first["model"], "claude-sonnet-5");

    // 2. Simulate a TUI edit: `e` on the `claude-sonnet-5` row, type
    // `gpt-5.4-mini`, hit Enter. In code: `store.set_live(...)` with
    // the new map. This is the same call the TUI's edit dialog uses.
    store.set_live(openai_to_anthropic_proxy::RuntimeMappings {
        map: BTreeMap::from([("claude-sonnet-5".into(), "gpt-5.4-mini".into())]),
        default_model: None,
    });

    // 3. Post-edit: the same client request must now reach the
    // upstream as `gpt-5.4-mini`. Before the fix, the handler read
    // from `state.config` (still empty), so this assertion would
    // fail with `claude-sonnet-5`.
    let _ = client
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(
            r#"{"model":"claude-sonnet-5","max_tokens":4,"messages":[{"role":"user","content":"b"}]}"#,
        ))
        .send()
        .await
        .unwrap();
    let second = upstream.received.lock().await.clone().unwrap();
    let second: serde_json::Value = serde_json::from_slice(&second).unwrap();
    assert_eq!(
        second["model"], "gpt-5.4-mini",
        "runtime alias edit must rewrite the inbound model on the next request"
    );
}

/// Companion to the above: clearing an alias at runtime (delete it
/// in the TUI) must restore pass-through behavior. Without the fix,
/// the static-config value (if any) would leak through; here, the
/// config is also empty, so we assert the proxy passes the inbound
/// name through verbatim after the runtime store goes back to empty.
#[tokio::test]
async fn runtime_alias_deletion_restores_pass_through() {
    use openai_to_anthropic_proxy::MappingsStore;
    use std::collections::BTreeMap;

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
        model_info: Default::default(),
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        upstream_base_url: format!("http://{upstream_addr}"),
        upstream_api_key: "sk-fake".into(),
        upstream_path: "/v1/responses".into(),
        request_timeout: Duration::from_secs(10),
        max_request_body_bytes: openai_to_anthropic_proxy::config::DEFAULT_MAX_REQUEST_BODY_BYTES,
        reasoning_effort: Some("none".into()),
        reasoning: Default::default(),
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
        system_message_placement: Default::default(),
        warnings: Vec::new(),
    });
    let store = Arc::new(MappingsStore::seeded(Arc::new(
        openai_to_anthropic_proxy::RuntimeMappings {
            map: BTreeMap::from([("claude-sonnet-5".into(), "gpt-5.4-mini".into())]),
            default_model: None,
        },
    )));
    let proxy_addr = start_proxy_with_store(config, store.clone()).await;

    let client = reqwest::Client::new();

    // Alias is present → upstream sees the resolved name.
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
    assert_eq!(body["model"], "gpt-5.4-mini");

    // Operator hits `d` on the row in the TUI → store.set_live with
    // empty map. Next request must pass through.
    store.set_live(openai_to_anthropic_proxy::RuntimeMappings::default());

    let _ = client
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(
            r#"{"model":"claude-sonnet-5","max_tokens":4,"messages":[{"role":"user","content":"b"}]}"#,
        ))
        .send()
        .await
        .unwrap();
    let body = upstream.received.lock().await.clone().unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["model"], "claude-sonnet-5");
}

/// A request that is in-flight when the operator edits the alias
/// table must finish with the mappings it started with, not the
/// new ones. This pins the snapshot contract documented on
/// `MappingsStore::load_live`. Without the per-handler snapshot,
/// a long-running request could see a torn response (e.g. a stream
/// whose `model` field in `message_start` no longer matches the
/// eventual upstream).
#[tokio::test]
async fn in_flight_request_keeps_its_original_mappings_snapshot() {
    use openai_to_anthropic_proxy::MappingsStore;
    use std::collections::BTreeMap;

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
        model_info: Default::default(),
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        upstream_base_url: format!("http://{upstream_addr}"),
        upstream_api_key: "sk-fake".into(),
        upstream_path: "/v1/responses".into(),
        request_timeout: Duration::from_secs(10),
        max_request_body_bytes: openai_to_anthropic_proxy::config::DEFAULT_MAX_REQUEST_BODY_BYTES,
        reasoning_effort: Some("none".into()),
        reasoning: Default::default(),
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
        system_message_placement: Default::default(),
        warnings: Vec::new(),
    });
    // Seed the runtime store with an alias so the first request
    // resolves `claude-sonnet-5` → `gpt-5.4-mini`.
    let store = Arc::new(MappingsStore::seeded(Arc::new(
        openai_to_anthropic_proxy::RuntimeMappings {
            map: BTreeMap::from([("claude-sonnet-5".into(), "gpt-5.4-mini".into())]),
            default_model: None,
        },
    )));
    let proxy_addr = start_proxy_with_store(config, store.clone()).await;

    let client = reqwest::Client::new();

    // First request: alias is active, upstream sees the resolved
    // name. We capture the body *before* mutating the store, so the
    // ordering is: send → wait → mutate → check received.
    let _ = client
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(
            r#"{"model":"claude-sonnet-5","max_tokens":4,"messages":[{"role":"user","content":"a"}]}"#,
        ))
        .send()
        .await
        .unwrap();

    // Mutate AFTER the first request has been received by the
    // upstream. The mutation must not retroactively rewrite the
    // first body — but more importantly, a new request must pick up
    // the new mapping.
    store.set_live(openai_to_anthropic_proxy::RuntimeMappings {
        map: BTreeMap::from([("claude-sonnet-5".into(), "gpt-5.6-luna".into())]),
        default_model: None,
    });

    let _ = client
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(
            r#"{"model":"claude-sonnet-5","max_tokens":4,"messages":[{"role":"user","content":"b"}]}"#,
        ))
        .send()
        .await
        .unwrap();

    let sent_bodies = upstream.received_all.lock().await.clone();
    assert_eq!(sent_bodies.len(), 2);
    let first: serde_json::Value = serde_json::from_slice(&sent_bodies[0]).unwrap();
    let second: serde_json::Value = serde_json::from_slice(&sent_bodies[1]).unwrap();
    // First request resolved against the original alias (gpt-5.4-mini).
    assert_eq!(first["model"], "gpt-5.4-mini");
    // Second request picked up the edited alias (gpt-5.6-luna).
    assert_eq!(second["model"], "gpt-5.6-luna");
}

/// The live regression: the upstream model obeys the *mutated* strict
/// schema and emits a fabricated value for an optional parameter
/// (`resumeFromRunId`-style, pattern-constrained) plus null sentinels.
/// The proxy must ship the client an input that validates against the
/// *original* schema: fabricated optional dropped, nulls stripped.
#[tokio::test]
async fn non_streaming_tool_input_is_repaired_to_original_schema() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned.lock().await = Some(
        r#"{
            "id": "resp_wf",
            "object": "response",
            "created_at": 1,
            "status": "completed",
            "model": "gpt-4o",
            "output": [{
                "type": "function_call",
                "id": "fc_1",
                "status": "completed",
                "call_id": "call_wf",
                "name": "Workflow",
                "arguments": "{\"script\":\"export const meta = {}\",\"resumeFromRunId\":\"not-a-real-run-id\",\"args\":null}"
            }],
            "usage": {"input_tokens": 5, "output_tokens": 3, "total_tokens": 8}
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
                "messages": [{"role": "user", "content": "run it"}],
                "tools": [{
                    "name": "Workflow",
                    "description": "Run a workflow",
                    "input_schema": {
                        "type": "object",
                        "properties": {
                            "script": {"type": "string"},
                            "resumeFromRunId": {"type": "string", "pattern": "^wf_[a-z0-9-]{6,}$"},
                            "args": {"description": "any value"}
                        },
                        "required": ["script"]
                    }
                }]
            }"#,
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["stop_reason"], "tool_use");
    let input = &body["content"][0]["input"];
    // The fabricated optional value is gone; the null sentinel is gone;
    // the required value survives.
    assert_eq!(
        input,
        &serde_json::json!({"script": "export const meta = {}"})
    );
}

/// Streaming path: argument deltas are buffered and the repaired blob
/// is emitted as a single input_json_delta before content_block_stop.
#[tokio::test]
async fn streaming_tool_input_is_repaired_before_shipping() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    let sse = [
        r#"event: response.created
data: {"type":"response.created","response":{"id":"resp_1","object":"response","created_at":0,"status":"in_progress","model":"gpt-4o","output":[]}}"#,
        r#"event: response.output_item.added
data: {"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_1","status":"in_progress","call_id":"call_wf","name":"Workflow","arguments":""}}"#,
        r#"event: response.function_call_arguments.delta
data: {"type":"response.function_call_arguments.delta","item_id":"fc_1","output_index":0,"delta":"{\"script\":\"s\",\"resumeFromRunId\":\"bogus\""}"#,
        r#"event: response.function_call_arguments.delta
data: {"type":"response.function_call_arguments.delta","item_id":"fc_1","output_index":0,"delta":",\"args\":null}"}"#,
        r#"event: response.output_item.done
data: {"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc_1","status":"completed","call_id":"call_wf","name":"Workflow","arguments":"{\"script\":\"s\",\"resumeFromRunId\":\"bogus\",\"args\":null}"}}"#,
        r#"event: response.completed
data: {"type":"response.completed","response":{"id":"resp_1","object":"response","created_at":0,"status":"completed","model":"gpt-4o","output":[{"type":"function_call","id":"fc_1","status":"completed","call_id":"call_wf","name":"Workflow","arguments":"{\"script\":\"s\",\"resumeFromRunId\":\"bogus\",\"args\":null}"}],"usage":{"input_tokens":4,"output_tokens":2,"total_tokens":6}}}"#,
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
                "messages": [{"role": "user", "content": "run it"}],
                "tools": [{
                    "name": "Workflow",
                    "description": "Run a workflow",
                    "input_schema": {
                        "type": "object",
                        "properties": {
                            "script": {"type": "string"},
                            "resumeFromRunId": {"type": "string", "pattern": "^wf_[a-z0-9-]{6,}$"},
                            "args": {"description": "any value"}
                        },
                        "required": ["script"]
                    }
                }]
            }"#,
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);
    let text = res.text().await.unwrap();

    // Exactly one input_json_delta, carrying the repaired blob.
    let input_deltas: Vec<serde_json::Value> = text
        .split("\n\n")
        .filter(|s| s.contains("\"input_json_delta\""))
        .filter_map(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("data: "))
                .and_then(|d| serde_json::from_str(d).ok())
        })
        .collect();
    assert_eq!(input_deltas.len(), 1);
    let partial = input_deltas[0]["delta"]["partial_json"].as_str().unwrap();
    let parsed: serde_json::Value = serde_json::from_str(partial).unwrap();
    assert_eq!(parsed, serde_json::json!({"script": "s"}));

    // And the event ordering is well-formed: start, one delta, stop.
    let kinds: Vec<&str> = text
        .split("\n\n")
        .filter(|s| !s.is_empty())
        .filter_map(|e| e.lines().find_map(|l| l.strip_prefix("event: ")))
        .collect();
    assert_eq!(
        kinds,
        vec![
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop",
        ]
    );
}

/// Hidden validation retry: when the repaired tool input still violates
/// the original schema (a genuinely required parameter missing), the
/// proxy re-queries the upstream once with a corrective tool result
/// instead of shipping a call the client would reject.
#[tokio::test]
async fn invalid_tool_input_triggers_corrective_retry() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    // Attempt 1: model omits the required `script` parameter entirely.
    // Attempt 2: model complies.
    let mut per_attempt = std::collections::VecDeque::new();
    per_attempt.push_back(FakeResponse {
        status: StatusCode::OK,
        body: Some(
            r#"{
                "id": "resp_bad",
                "object": "response",
                "created_at": 1,
                "status": "completed",
                "model": "gpt-4o",
                "output": [{
                    "type": "function_call",
                    "id": "fc_1",
                    "status": "completed",
                    "call_id": "call_1",
                    "name": "Workflow",
                    "arguments": "{\"resumeFromRunId\":\"bogus\"}"
                }],
                "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
            }"#
            .into(),
        ),
    });
    per_attempt.push_back(FakeResponse {
        status: StatusCode::OK,
        body: Some(
            r#"{
                "id": "resp_good",
                "object": "response",
                "created_at": 1,
                "status": "completed",
                "model": "gpt-4o",
                "output": [{
                    "type": "function_call",
                    "id": "fc_2",
                    "status": "completed",
                    "call_id": "call_2",
                    "name": "Workflow",
                    "arguments": "{\"script\":\"export const meta = {}\"}"
                }],
                "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
            }"#
            .into(),
        ),
    });
    *upstream.canned_per_attempt.lock().await = per_attempt;

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
                "messages": [{"role": "user", "content": "run it"}],
                "tools": [{
                    "name": "Workflow",
                    "description": "Run a workflow",
                    "input_schema": {
                        "type": "object",
                        "properties": {
                            "script": {"type": "string"},
                            "resumeFromRunId": {"type": "string", "pattern": "^wf_[a-z0-9-]{6,}$"}
                        },
                        "required": ["script"]
                    }
                }]
            }"#,
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = res.json().await.unwrap();
    // The client sees the SECOND (valid) attempt's call.
    assert_eq!(body["content"][0]["id"], "call_2");
    assert_eq!(
        body["content"][0]["input"],
        serde_json::json!({"script": "export const meta = {}"})
    );

    // The upstream was queried twice; the second request carries the
    // original bad call plus a corrective tool result.
    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(sent.len(), 2);
    let second: serde_json::Value = serde_json::from_slice(&sent[1]).unwrap();
    let items = second["input"].as_array().unwrap();
    let has_corrective = items.iter().any(|i| {
        i["type"] == "function_call_output"
            && i["output"]
                .as_str()
                .is_some_and(|o| o.contains("[TOOL_ERROR]"))
    });
    assert!(
        has_corrective,
        "second request must carry a corrective tool result"
    );
}

/// An upstream that rejects one optional parameter (here `temperature`)
/// must be retried once without it, and the fact must be remembered for
/// the rest of the process so later requests never send it again.
///
/// Asserts on the exact bodies the fake upstream received, so it proves
/// both halves of the feature: the one-shot retry, and the latch that
/// keeps the doomed parameter off the wire afterwards.
#[tokio::test]
async fn unsupported_parameter_retries_without_it_and_remembers() {
    let (upstream_addr, upstream) = start_fake_upstream().await;

    let ok_body = || {
        r#"{
            "id": "resp_ok",
            "object": "response",
            "created_at": 1,
            "status": "completed",
            "model": "gpt-6-astra",
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "ok", "annotations": []}]
            }],
            "usage": {"input_tokens":1,"output_tokens":1,"total_tokens":2}
        }"#
        .to_string()
    };

    // First upstream call rejects `temperature`; the second succeeds.
    // Anything past that (the third call, which exercises the latch)
    // falls through to `canned`.
    *upstream.canned_per_attempt.lock().await = VecDeque::from(vec![
        FakeResponse {
            status: StatusCode::BAD_REQUEST,
            body: Some(
                r#"{"message":"Unsupported parameter: temperature is not supported with this model.","type":"invalid_request_error","code":"unsupported_parameter","param":"temperature"}"#.into(),
            ),
        },
        FakeResponse {
            status: StatusCode::OK,
            body: Some(ok_body()),
        },
    ]);
    *upstream.canned.lock().await = Some(ok_body());

    let config = make_proxy_config(upstream_addr);
    let proxy_addr = start_proxy(config).await;
    let client = reqwest::Client::new();

    let request = r#"{"model":"gpt-6-astra","max_tokens":4,"temperature":0.7,"top_p":0.9,"messages":[{"role":"user","content":"a"}]}"#;

    // First client request: sent optimistically, rejected, retried.
    let res = client
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(request))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(sent.len(), 2, "expected exactly one parameter-strip retry");
    let first: serde_json::Value = serde_json::from_slice(&sent[0]).unwrap();
    let second: serde_json::Value = serde_json::from_slice(&sent[1]).unwrap();
    assert!(
        first.get("temperature").is_some(),
        "the optimistic first attempt must carry the parameter"
    );
    assert!(
        second.get("temperature").is_none(),
        "the retry must drop it"
    );
    assert!(
        second.get("top_p").is_some(),
        "only the named parameter is dropped; top_p must survive"
    );

    // Second client request, same model: the latch means the 400 is not
    // provoked again -- `temperature` is never put on the wire.
    let res = client
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(request))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(
        sent.len(),
        3,
        "a learned fact must avoid a second failed attempt"
    );
    let third: serde_json::Value = serde_json::from_slice(&sent[2]).unwrap();
    assert!(
        third.get("temperature").is_none(),
        "the learned fact must be applied before the first send"
    );
    assert!(
        third.get("top_p").is_some(),
        "unrelated parameters stay intact"
    );
}

/// A 400 that names a parameter but is a *value* problem, not a support
/// problem, must pass straight through: no retry, one upstream call.
/// This is the guard that stops the proxy silently stripping a
/// parameter the model actually honours.
#[tokio::test]
async fn invalid_parameter_value_is_not_treated_as_unsupported() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned_error.lock().await = Some((
        StatusCode::BAD_REQUEST,
        r#"{"message":"Invalid value for 'temperature': temperature must be between 0 and 2.","type":"invalid_request_error","code":"invalid_request_error","param":"temperature"}"#.to_string(),
    ));

    let config = make_proxy_config(upstream_addr);
    let proxy_addr = start_proxy(config).await;
    let client = reqwest::Client::new();

    let res = client
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(
            r#"{"model":"gpt-5.4-mini","max_tokens":4,"temperature":9.5,"messages":[{"role":"user","content":"a"}]}"#,
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        upstream.received_all.lock().await.len(),
        1,
        "a value error must not trigger a parameter-strip retry"
    );
}

/// The real-world always-reasoning shape: the upstream rejects
/// `temperature` and then `top_p` on consecutive attempts. The proxy
/// must strip each in turn and still deliver a successful response --
/// a single shared retry budget would fail this request.
#[tokio::test]
async fn rejects_both_sampling_params_one_at_a_time() {
    let (upstream_addr, upstream) = start_fake_upstream().await;

    let ok_body = || {
        r#"{
            "id": "resp_ok",
            "object": "response",
            "created_at": 1,
            "status": "completed",
            "model": "gpt-6-astra",
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "ok", "annotations": []}]
            }],
            "usage": {"input_tokens":1,"output_tokens":1,"total_tokens":2}
        }"#
        .to_string()
    };

    *upstream.canned_per_attempt.lock().await = VecDeque::from(vec![
        FakeResponse {
            status: StatusCode::BAD_REQUEST,
            body: Some(
                r#"{"message":"Unsupported parameter: temperature","code":"unsupported_parameter","param":"temperature"}"#.into(),
            ),
        },
        FakeResponse {
            status: StatusCode::BAD_REQUEST,
            body: Some(
                r#"{"message":"Unsupported parameter: top_p","code":"unsupported_parameter","param":"top_p"}"#.into(),
            ),
        },
        FakeResponse {
            status: StatusCode::OK,
            body: Some(ok_body()),
        },
    ]);

    let config = make_proxy_config(upstream_addr);
    let proxy_addr = start_proxy(config).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(
            r#"{"model":"gpt-6-astra","max_tokens":4,"temperature":0.7,"top_p":0.9,"messages":[{"role":"user","content":"a"}]}"#,
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(sent.len(), 3, "expected one retry per rejected parameter");
    let bodies: Vec<serde_json::Value> = sent
        .iter()
        .map(|b| serde_json::from_slice(b).unwrap())
        .collect();
    assert!(bodies[0].get("temperature").is_some() && bodies[0].get("top_p").is_some());
    assert!(bodies[1].get("temperature").is_none() && bodies[1].get("top_p").is_some());
    assert!(bodies[2].get("temperature").is_none() && bodies[2].get("top_p").is_none());
}

// ─── Phase-1 characterization tests ────────────────────────────────────────
//
// These pin the exact retry order, response attribution and streaming Drop
// behaviour of the request pipeline *before* it is reshaped into the reusable
// `Attempt` unit. All of them use the fake upstream above; none call a real API.

/// A shared non-streaming success body with authoritative usage.
fn completed_response(text: &str, input_tokens: u64, output_tokens: u64) -> String {
    format!(
        r#"{{
            "id": "resp_ok",
            "object": "response",
            "created_at": 1,
            "status": "completed",
            "model": "gpt-4o-mini",
            "output": [{{
                "type": "message",
                "role": "assistant",
                "content": [{{"type": "output_text", "text": "{text}", "annotations": []}}]
            }}],
            "usage": {{"input_tokens":{input_tokens},"output_tokens":{output_tokens},"total_tokens":{}}}
        }}"#,
        input_tokens + output_tokens
    )
}

/// Like `start_proxy`, but shares the caller's `SessionStatsStore` and TUI
/// `OutputSink` so a test can assert on attribution and emitted log lines.
/// Uses `router_with_stores` with a fresh capability store to keep every
/// test's learned-parameter latch independent.
async fn start_proxy_with_captured_stores(
    config: Arc<Config>,
    stats: Arc<openai_to_anthropic_proxy::SessionStatsStore>,
    output: openai_to_anthropic_proxy::OutputSink,
) -> SocketAddr {
    use openai_to_anthropic_proxy::{CapabilityStore, MappingsStore, RuntimeMappings};
    let store = Arc::new(MappingsStore::seeded(Arc::new(
        RuntimeMappings::from_config(&config),
    )));
    let client = reqwest::Client::builder()
        .timeout(config.request_timeout)
        .build()
        .unwrap();
    let app = openai_to_anthropic_proxy::proxy::router_with_stores(
        config,
        store,
        client,
        output,
        stats,
        Arc::new(CapabilityStore::new()),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

/// Retry ORDER matters: an unsupported-parameter 400 strips the named
/// parameter, a following model rejection re-translates the *whole* request
/// for `default_model` (recomputing reasoning/caching/replay), and only then
/// does the succeeded request reach the client. This asserts the exact JSON
/// bodies the fake upstream received, in order — a change to any of the
/// three policies, or to the retry order, shows up here.
#[tokio::test]
async fn retry_order_matrix_records_exact_upstream_bodies() {
    use openai_to_anthropic_proxy::config::ReasoningReplayMode;

    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned_per_attempt.lock().await = VecDeque::from(vec![
        FakeResponse {
            status: StatusCode::BAD_REQUEST,
            body: Some(
                r#"{"message":"Unsupported parameter: temperature","code":"unsupported_parameter","param":"temperature"}"#.into(),
            ),
        },
        FakeResponse {
            status: StatusCode::BAD_REQUEST,
            body: Some(
                r#"{"message":"model not supported","type":"invalid_request_error","code":"model_not_supported"}"#.into(),
            ),
        },
        FakeResponse {
            status: StatusCode::OK,
            body: Some(completed_response("matrix ok", 7, 3)),
        },
    ]);

    let config = Arc::new(Config {
        model_info: Default::default(),
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        upstream_base_url: format!("http://{upstream_addr}"),
        upstream_api_key: "test-key".into(),
        upstream_path: "/v1/responses".into(),
        request_timeout: Duration::from_secs(10),
        max_request_body_bytes: openai_to_anthropic_proxy::config::DEFAULT_MAX_REQUEST_BODY_BYTES,
        reasoning_effort: None,
        reasoning: ReasoningConfig {
            default: Some("low".into()),
            models: BTreeMap::from([("gpt-4o-mini".into(), "none".into())]),
            ..ReasoningConfig::default()
        },
        reasoning_replay: openai_to_anthropic_proxy::config::ReasoningReplayConfig {
            default: ReasoningReplayMode::Text,
            models: BTreeMap::from([("gpt-4o-mini".into(), ReasoningReplayMode::Off)]),
            max_chars: 0,
        },
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
        system_message_placement: Default::default(),
        warnings: Vec::new(),
    });
    let proxy_addr = start_proxy(config).await;

    // Carries a thinking block, a cache_control hint and both sampling
    // parameters so every recomputed policy has an observable field.
    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(
            r#"{
                "model":"claude-sonnet-5",
                "max_tokens":16,
                "temperature":0.7,
                "top_p":0.9,
                "messages":[
                    {"role":"user","content":[{"type":"text","text":"cached","cache_control":{"type":"ephemeral"}}]},
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
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["content"][0]["text"], "matrix ok");

    let sent = upstream.received_all.lock().await.clone();
    let bodies: Vec<serde_json::Value> = sent
        .iter()
        .map(|b| serde_json::from_slice(b).unwrap())
        .collect();
    assert_eq!(bodies.len(), 3, "unsupported-param → fallback → success");

    // Attempt 1: primary model, both sampling params, primary policies.
    let first = &bodies[0];
    assert_eq!(first["model"], "claude-sonnet-5");
    assert_eq!(first["temperature"], 0.7);
    assert_eq!(first["top_p"], 0.9);
    assert_eq!(first["reasoning"]["effort"], "low");
    assert_eq!(first["prompt_cache_key"], "cache-key");
    assert!(first["input"][0]["content"][0]["prompt_cache_breakpoint"].is_object());
    assert!(
        first["input"][1]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("the model reasoned about this")
    );

    // Attempt 2: only `temperature` was stripped for the parameter retry.
    let second = &bodies[1];
    assert_eq!(second["model"], "claude-sonnet-5");
    assert!(second.get("temperature").is_none());
    assert_eq!(second["top_p"], 0.9);
    assert_eq!(second["reasoning"]["effort"], "low");
    assert_eq!(second["prompt_cache_key"], "cache-key");

    // Attempt 3: a full re-translation for the fallback model. The rebuilt
    // request carries both sampling params again (the fallback model has no
    // learned facts), and its own reasoning/caching/replay policies.
    let third = &bodies[2];
    assert_eq!(third["model"], "gpt-4o-mini");
    assert_eq!(third["temperature"], 0.7);
    assert_eq!(third["top_p"], 0.9);
    assert_eq!(third["reasoning"]["effort"], "none");
    assert!(third.get("prompt_cache_key").is_none());
    assert!(
        third["input"][0]["content"][0]
            .get("prompt_cache_breakpoint")
            .is_none()
    );
    assert_eq!(third["input"][1]["content"][0]["text"], "the answer");
}

/// After a model fallback, the hidden validation/corrective retry still runs
/// on the *fallback* request. If the corrective retry's upstream call fails,
/// the first successful (fallback) response must be kept — not mapped into
/// an error, and not re-translated for a third time.
#[tokio::test]
async fn corrective_retry_after_fallback_keeps_first_success_on_retry_error() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    let bad_call = r#"{
        "id": "resp_bad",
        "object": "response",
        "created_at": 1,
        "status": "completed",
        "model": "gpt-4o-mini",
        "output": [{
            "type": "function_call",
            "id": "fc_1",
            "status": "completed",
            "call_id": "call_1",
            "name": "Workflow",
            "arguments": "{\"resumeFromRunId\":\"bogus\"}"
        }],
        "usage": {"input_tokens":1,"output_tokens":1,"total_tokens":2}
    }"#;
    *upstream.canned_per_attempt.lock().await = VecDeque::from(vec![
        FakeResponse {
            status: StatusCode::BAD_REQUEST,
            body: Some(
                r#"{"message":"model not supported","type":"invalid_request_error","code":"model_not_supported"}"#.into(),
            ),
        },
        FakeResponse {
            status: StatusCode::OK,
            body: Some(bad_call.into()),
        },
        FakeResponse {
            status: StatusCode::BAD_REQUEST,
            body: Some(r#"{"message":"corrective retry failed"}"#.into()),
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
            r#"{
                "model": "claude-sonnet-5",
                "max_tokens": 64,
                "messages": [{"role": "user", "content": "run it"}],
                "tools": [{
                    "name": "Workflow",
                    "description": "Run a workflow",
                    "input_schema": {
                        "type": "object",
                        "properties": {"script": {"type": "string"}},
                        "required": ["script"]
                    }
                }]
            }"#,
        ))
        .send()
        .await
        .unwrap();

    // The first success wins: the client sees the invalid-but-repaired call
    // (not an error), even though the corrective retry's upstream 400 came
    // later.
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["content"][0]["id"], "call_1");

    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(sent.len(), 3, "fallback + one bounded corrective retry");
    let second: serde_json::Value = serde_json::from_slice(&sent[1]).unwrap();
    let third: serde_json::Value = serde_json::from_slice(&sent[2]).unwrap();
    assert_eq!(second["model"], "gpt-4o-mini");
    assert_eq!(third["model"], "gpt-4o-mini");
    let corrective = third["input"].as_array().unwrap().iter().any(|i| {
        i["type"] == "function_call_output"
            && i["output"]
                .as_str()
                .is_some_and(|o| o.contains("[TOOL_ERROR]"))
    });
    assert!(corrective, "the retry must carry the corrective input item");
}

/// A fallback completion is attributed exactly once, to the *actual* model
/// that produced it, and the response log line is emitted once.
#[tokio::test]
async fn fallback_usage_attribution_is_recorded_once() {
    use openai_to_anthropic_proxy::tui::LogKind;
    use openai_to_anthropic_proxy::{SessionStatsStore, TokenTotals};

    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned_per_attempt.lock().await = VecDeque::from(vec![
        FakeResponse {
            status: StatusCode::NOT_FOUND,
            body: Some(REAL_GATEWAY_MODEL_404.into()),
        },
        FakeResponse {
            status: StatusCode::OK,
            body: Some(completed_response("attributed", 11, 5)),
        },
    ]);

    let stats = Arc::new(SessionStatsStore::new());
    let (output, log_rx) = openai_to_anthropic_proxy::OutputSink::tui();
    let config =
        make_config_with_default_model(upstream_addr, BTreeMap::new(), Some("gpt-4o-mini"));
    let proxy_addr = start_proxy_with_captured_stores(config, stats.clone(), output).await;

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

    let snapshot = stats.snapshot_sections();
    assert!(snapshot.inbound.is_empty(), "no inbound-model attribution");
    assert_eq!(snapshot.actual.len(), 1, "one actual-model bucket");
    assert_eq!(
        snapshot.actual.get("gpt-4o-mini"),
        Some(&TokenTotals {
            requests: 1,
            input_tokens: 11,
            output_tokens: 5,
            ..TokenTotals::default()
        })
    );

    let lines: Vec<_> = std::iter::from_fn(|| log_rx.try_recv().ok()).collect();
    let responses: Vec<_> = lines
        .iter()
        .filter(|l| l.kind == LogKind::Response)
        .collect();
    assert_eq!(responses.len(), 1, "exactly one response log line");
    assert!(responses[0].text.contains("11 in"));
    assert!(responses[0].text.contains("5 out"));
}

/// A stream that reaches its terminal event records session stats exactly
/// once — even if the client disconnects immediately after reading the
/// terminal bytes. This is the `TranslatorStream::Drop` contract.
#[tokio::test]
async fn streaming_terminal_event_records_stats_once() {
    use openai_to_anthropic_proxy::SessionStatsStore;

    let (upstream_addr, upstream) = start_fake_upstream().await;
    let sse = [
        r#"event: response.created
data: {"type":"response.created","response":{"id":"resp_1","object":"response","created_at":0,"status":"in_progress","model":"gpt-4o","output":[]}}"#,
        r#"event: response.output_item.added
data: {"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_x","status":"in_progress","role":"assistant","content":[]}}"#,
        r#"event: response.output_text.delta
data: {"type":"response.output_text.delta","item_id":"msg_x","output_index":0,"content_index":0,"delta":"Hi"}"#,
        r#"event: response.completed
data: {"type":"response.completed","response":{"id":"resp_1","object":"response","created_at":0,"status":"completed","model":"gpt-4o","output":[],"usage":{"input_tokens":4,"output_tokens":2,"total_tokens":6}}}"#,
    ]
    .join("\n\n")
        + "\n\n";
    *upstream.canned_stream.lock().await = Some(sse);

    let stats = Arc::new(SessionStatsStore::new());
    let proxy_addr = start_proxy_with_captured_stores(
        make_proxy_config(upstream_addr),
        stats.clone(),
        openai_to_anthropic_proxy::OutputSink::tui().0,
    )
    .await;

    let mut res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(
            r#"{"model":"gpt-4o","max_tokens":16,"stream":true,"messages":[{"role":"user","content":"a"}]}"#,
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    // Client disconnect: read only the first chunk, then drop the response.
    // The proxy's Drop hook must still have captured the terminal usage.
    let _chunk = res.chunk().await.unwrap();
    drop(res);

    // The upstream SSE body is finite; polling until Drop has run is enough
    // to make the assertion deterministic without sleeping on wall time.
    for _ in 0..100 {
        let totals = stats.snapshot_sections();
        if !totals.inbound.is_empty() {
            break;
        }
        tokio::task::yield_now().await;
    }
    let totals = stats.snapshot_sections();
    assert!(totals.actual.is_empty(), "no fallback was used");
    assert_eq!(totals.inbound.len(), 1);
    let usage = &totals.inbound["gpt-4o"];
    assert_eq!(usage.requests, 1, "terminal event recorded exactly once");
    assert_eq!(usage.input_tokens, 4);
    assert_eq!(usage.output_tokens, 2);
}

// ─── Phase-3 advisor integration tests ─────────────────────────────────────
//
// All of these use the existing in-process `FakeUpstream`. No real API is
// called, and the only advisor model name anywhere in this file is the cheap
// default `beam`. `enabled: false` (the shipped default) is verified to be a
// byte-for-byte no-op before any of the enabled tests run.

const ADVISOR_DECLARATION: &str = r#"{"type":"advisor_20260301","name":"advisor","model":"beam"}"#;

/// Claude Code's three-field declaration plus an advisor-enabled config.
/// `max_tokens: 64` and `timeout_secs: 2` keep every test cheap and bounded.
fn advisor_config(addr: SocketAddr) -> Arc<Config> {
    let mut config = (*make_proxy_config(addr)).clone();
    config.advisor = AdvisorConfig {
        enabled: true,
        model: "beam".into(),
        effort: None,
        max_uses: Some(2),
        max_tokens: Some(64),
        reasoning_max_tokens: Some(100),
        timeout_secs: Some(2),
    };
    Arc::new(config)
}

fn advisor_request(stream: bool) -> String {
    advisor_request_with_decl(stream, ADVISOR_DECLARATION)
}

fn advisor_request_with_decl(stream: bool, declaration: &str) -> String {
    format!(
        r#"{{
            "model": "gpt-4o-mini",
            "max_tokens": 64,
            "stream": {stream},
            "system": "Git status: clean\n\n# Advisor Tool\n\nCall advisor BEFORE substantive work.\n\n<system-reminder>skills</system-reminder>",
            "tools": [{declaration}],
            "messages": [{{"role": "user", "content": "Do the task"}}]
        }}"#
    )
}

/// A completed Responses body with one `output_text` message.
fn advisor_executor_message(id: &str, text: &str) -> FakeResponse {
    FakeResponse {
        status: StatusCode::OK,
        body: Some(completed_response_with_id(id, text)),
    }
}

fn completed_response_with_id(id: &str, text: &str) -> String {
    format!(
        r#"{{
            "id": "{id}",
            "object": "response",
            "created_at": 1,
            "status": "completed",
            "model": "gpt-4o-mini",
            "output": [{{
                "type": "message",
                "role": "assistant",
                "content": [{{"type": "output_text", "text": "{text}", "annotations": []}}]
            }}],
            "usage": {{"input_tokens":7,"output_tokens":3,"total_tokens":10}}
        }}"#
    )
}

/// Executor asks for advice: one `function_call{name:"advisor"}`.
fn advisor_call_response(id: &str, call_id: &str) -> FakeResponse {
    FakeResponse {
        status: StatusCode::OK,
        body: Some(format!(
            r#"{{
                "id": "{id}",
                "object": "response",
                "created_at": 1,
                "status": "completed",
                "model": "gpt-4o-mini",
                "output": [{{
                    "type": "function_call",
                    "call_id": "{call_id}",
                    "name": "advisor",
                    "arguments": "{{}}"
                }}],
                "usage": {{"input_tokens":9,"output_tokens":2,"total_tokens":11}}
            }}"#
        )),
    }
}

/// Advisor's own answer, with a distinct model name and usage so the
/// `advisor_message` iteration is easy to assert on.
fn advisor_answer(text: &str) -> FakeResponse {
    FakeResponse {
        status: StatusCode::OK,
        body: Some(format!(
            r#"{{
                "id": "resp_advice",
                "object": "response",
                "created_at": 1,
                "status": "completed",
                "model": "beam",
                "output": [{{
                    "type": "message",
                    "role": "assistant",
                    "content": [{{"type": "output_text", "text": "{text}", "annotations": []}}]
                }}],
                "usage": {{"input_tokens":21,"output_tokens":13,"total_tokens":34}}
            }}"#
        )),
    }
}

fn script(upstream: &FakeUpstream, responses: Vec<FakeResponse>) {
    *upstream.canned_per_attempt.try_lock().unwrap() = VecDeque::from(responses);
}

/// 1. The MOST important invariant: with `enabled: false` the proxy makes
/// exactly one upstream POST and answers exactly as it did before phase 3,
/// even when the inbound request declares the advisor tool. A second case
/// (enabled config but NO declaration) proves detection-triggering: an
/// `enabled: true` deployment still leaves every non-advisor request alone.
#[tokio::test]
async fn advisor_disabled_is_byte_for_byte_identical() {
    const BASH_DECL: &str =
        r#"{"name":"Bash","description":"run","input_schema":{"type":"object","properties":{}}}"#;
    for (label, enabled, declaration) in [
        ("disabled", false, ADVISOR_DECLARATION),
        ("enabled_no_declaration", true, BASH_DECL),
    ] {
        let (upstream_addr, upstream) = start_fake_upstream().await;
        *upstream.canned.lock().await =
            Some(completed_response_with_id("resp_same", "same answer"));
        let mut config = (*make_proxy_config(upstream_addr)).clone();
        config.advisor.enabled = enabled;
        let proxy_addr = start_proxy(Arc::new(config)).await;

        let res = reqwest::Client::new()
            .post(format!("http://{proxy_addr}/v1/messages"))
            .header("content-type", "application/json")
            .body(ReqBody::from(advisor_request_with_decl(false, declaration)))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{label}");
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
            "{label}: the client response must be byte-identical to the pre-advisor path"
        );

        let sent = upstream.received_all.lock().await.clone();
        assert_eq!(sent.len(), 1, "{label}: no extra POSTs");
        let received: serde_json::Value = serde_json::from_slice(&sent[0]).unwrap();
        assert_eq!(received["stream"], false);
        // The ordinary translator may legitimately forward the client's
        // declared tools; what must NEVER appear is a synthesized advisor
        // function tool (that only happens on the active advisor path).
        let upstream_tools = received["tools"].as_array().cloned().unwrap_or_default();
        assert!(
            !upstream_tools.iter().any(|t| t["name"] == "advisor"),
            "{label}: no synthesized advisor tool"
        );
        // B9: with the advisor pipeline INACTIVE (both cases here — the
        // shipped default `enabled:false`, or `enabled:true` with no real
        // declaration) the orphan `# Advisor Tool` section must NOT reach
        // the executor. Before the fix, translate.rs's name-based check
        // preserved it for the disabled + declaration case, telling the
        // model to call a tool that was dropped on translation.
        assert!(
            !received["instructions"]
                .as_str()
                .unwrap()
                .contains("Call advisor BEFORE substantive work"),
            "{label}: inactive advisor must strip the # Advisor Tool instructions"
        );
    }
}

/// An ordinary streaming request with `enabled: false` is also unchanged:
/// the upstream is still asked to stream (no forced buffering) and the
/// client still gets the real translated SSE, not the advisor's synthetic
/// sequence.
#[tokio::test]
async fn advisor_disabled_leaves_streaming_unchanged() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    let sse = [
        r#"event: response.created
data: {"type":"response.created","response":{"id":"resp_1","object":"response","created_at":0,"status":"in_progress","model":"gpt-4o","output":[]}}"#,
        r#"event: response.output_text.delta
data: {"type":"response.output_text.delta","item_id":"msg_x","output_index":0,"content_index":0,"delta":"Hi"}"#,
        r#"event: response.completed
data: {"type":"response.completed","response":{"id":"resp_1","object":"response","created_at":0,"status":"completed","model":"gpt-4o","output":[],"usage":{"input_tokens":4,"output_tokens":2,"total_tokens":6}}}"#,
    ]
    .join("\n\n")
        + "\n\n";
    *upstream.canned_stream.lock().await = Some(sse);
    let config = make_proxy_config(upstream_addr);
    assert!(!config.advisor.enabled);
    let proxy_addr = start_proxy(config).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(true)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers()
            .get(axum::http::header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap(),
        "text/event-stream"
    );
    let text = res.text().await.unwrap();
    let kinds: Vec<&str> = text
        .split("\n\n")
        .filter(|s| !s.trim().is_empty())
        .filter_map(|e| e.lines().find_map(|l| l.strip_prefix("event: ")))
        .collect();
    assert_eq!(kinds.iter().filter(|k| **k == "message_start").count(), 1);
    assert_eq!(kinds.iter().filter(|k| **k == "message_stop").count(), 1);

    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(sent.len(), 1, "no extra POST");
    let v: serde_json::Value = serde_json::from_slice(&sent[0]).unwrap();
    assert_eq!(v["stream"], true, "upstream streaming was NOT forced off");
    assert!(
        !v["tools"]
            .as_array()
            .unwrap_or(&Vec::new())
            .iter()
            .any(|t| t["name"] == "advisor"),
        "no synthesized advisor tool"
    );
}

/// The advisor request carries a `prompt_cache_key` only when the operator
/// turned prompt caching on for the advisor's model, and the key is the same
/// for every consultation of one conversation.
async fn advisor_cache_keys(models: &[&str], cache_key: Option<&str>) -> Vec<serde_json::Value> {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            advisor_call_response("resp_c1", "call_t1"),
            advisor_answer("first advice"),
            advisor_call_response("resp_c2", "call_t2"),
            advisor_answer("second advice"),
            advisor_executor_message("resp_final", "done"),
        ],
    );
    let mut config = (*advisor_config(upstream_addr)).clone();
    config.prompt_caching.models = models.iter().map(|m| (*m).to_owned()).collect();
    config.prompt_caching.cache_key = cache_key.map(str::to_owned);
    let proxy_addr = start_proxy(Arc::new(config)).await;
    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    upstream
        .received_all
        .lock()
        .await
        .iter()
        .map(|b| serde_json::from_slice::<serde_json::Value>(b).unwrap())
        .filter(|b| b["model"] == "beam")
        .map(|b| b["prompt_cache_key"].clone())
        .collect()
}

#[tokio::test]
async fn advisor_sends_no_cache_key_unless_caching_is_on_for_its_model() {
    let keys = advisor_cache_keys(&[], None).await;
    assert_eq!(keys.len(), 2, "two consultations");
    assert!(keys.iter().all(serde_json::Value::is_null), "{keys:?}");
    // Caching on for another model only: still none.
    let keys = advisor_cache_keys(&["gpt-4o-mini"], None).await;
    assert!(keys.iter().all(serde_json::Value::is_null), "{keys:?}");
}

#[tokio::test]
async fn advisor_uses_one_stable_cache_key_per_conversation() {
    let keys = advisor_cache_keys(&["beam"], None).await;
    assert_eq!(keys.len(), 2);
    let first = keys[0].as_str().expect("a derived key");
    assert!(first.starts_with("advisor-"), "{first}");
    assert_eq!(keys[0], keys[1], "both consultations share one key");
}

#[tokio::test]
async fn advisor_uses_the_configured_cache_key_as_given() {
    let keys = advisor_cache_keys(&["beam"], Some("my-key")).await;
    assert!(keys.iter().all(|k| k == "my-key"), "{keys:?}");
}

/// The `reasoning` object of each advisor request, for an advisor with the
/// given effort and the proxy's `reasoning.summary` setting.
async fn advisor_reasoning(
    effort: Option<&str>,
    summary: openai_to_anthropic_proxy::config::ReasoningSummary,
) -> Vec<serde_json::Value> {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            advisor_call_response("resp_c1", "call_t1"),
            advisor_answer("advice"),
            advisor_executor_message("resp_final", "done"),
        ],
    );
    let mut config = (*advisor_config(upstream_addr)).clone();
    config.advisor.effort = effort.map(str::to_owned);
    config.reasoning.summary = summary;
    let proxy_addr = start_proxy(Arc::new(config)).await;
    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    upstream
        .received_all
        .lock()
        .await
        .iter()
        .map(|b| serde_json::from_slice::<serde_json::Value>(b).unwrap())
        .filter(|b| b["model"] == "beam")
        .map(|b| b["reasoning"].clone())
        .collect()
}

#[tokio::test]
async fn advisor_follows_the_reasoning_summary_setting() {
    use openai_to_anthropic_proxy::config::ReasoningSummary;

    // Reasoning on → the advisor asks for a summary, like the main request.
    let on = advisor_reasoning(Some("low"), ReasoningSummary::Auto).await;
    assert_eq!(on.len(), 1);
    assert_eq!(on[0]["effort"], "low");
    assert_eq!(on[0]["summary"], "auto");

    // Switched off → effort still sent, no summary.
    let off = advisor_reasoning(Some("low"), ReasoningSummary::Off).await;
    assert_eq!(off[0]["effort"], "low");
    assert!(off[0].get("summary").is_none(), "{}", off[0]);

    // Effort "none" → no summary even when the setting is on.
    let none = advisor_reasoning(Some("none"), ReasoningSummary::Auto).await;
    assert!(none[0].get("summary").is_none(), "{}", none[0]);

    // No advisor effort → no `reasoning` object at all, so nothing to add to.
    let unset = advisor_reasoning(None, ReasoningSummary::Auto).await;
    assert!(unset[0].is_null(), "{}", unset[0]);
}

/// 2. Happy path: 3 POSTs (executor call → advisor text → final executor
/// text). The third body must carry the canonical `function_call_output`
/// advice, and the client sees one contiguous, id-matched block pair plus
/// `usage.iterations[]`.
#[tokio::test]
async fn advisor_happy_path() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            advisor_call_response("resp_call", "call_adv_1"),
            advisor_answer("Use a channel-based coordination pattern."),
            advisor_executor_message("resp_final", "Here is the implementation."),
        ],
    );
    let proxy_addr = start_proxy(advisor_config(upstream_addr)).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = res.json().await.unwrap();

    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(sent.len(), 3, "executor, advisor, executor");
    let bodies: Vec<serde_json::Value> = sent
        .iter()
        .map(|b| serde_json::from_slice(b).unwrap())
        .collect();

    // The advisor's own POST is separate, tool-free and explicitly modelled.
    assert_eq!(bodies[1]["model"], "beam");
    assert_eq!(bodies[1]["stream"], false);
    assert!(bodies[1]["tools"].is_null());
    // answer budget 64 + reasoning budget 100
    assert_eq!(bodies[1]["max_output_tokens"], 164);
    assert!(
        bodies[1]["input"][0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("transcript")
    );

    // The 3rd (final executor) request carries BOTH canonical input items.
    let items = bodies[2]["input"].as_array().unwrap();
    let call = items
        .iter()
        .find(|i| i["type"] == "function_call")
        .expect("function_call echo");
    assert_eq!(call["name"], "advisor");
    assert_eq!(call["call_id"], "call_adv_1");
    let output = items
        .iter()
        .find(|i| i["type"] == "function_call_output")
        .expect("function_call_output advice");
    assert_eq!(output["call_id"], "call_adv_1");
    assert_eq!(
        output["output"], "Use a channel-based coordination pattern.",
        "the advice must reach the executor's transcript"
    );
    let call_pos = items
        .iter()
        .position(|i| i["type"] == "function_call")
        .unwrap();
    let out_pos = items
        .iter()
        .position(|i| i["type"] == "function_call_output")
        .unwrap();
    assert!(call_pos < out_pos, "no orphan function_call");

    // One assistant message, contiguous pair, matching ids, end_turn.
    let content = body["content"].as_array().unwrap();
    assert_eq!(content.len(), 3, "pair + final executor text");
    assert_eq!(content[0]["type"], "server_tool_use");
    assert_eq!(content[0]["name"], "advisor");
    assert_eq!(content[1]["type"], "advisor_tool_result");
    assert_eq!(content[0]["id"], content[1]["tool_use_id"]);
    assert!(content[0]["id"].as_str().unwrap().starts_with("srvtoolu_"));
    assert_eq!(content[1]["content"]["type"], "advisor_result");
    assert_eq!(
        content[1]["content"]["text"],
        "Use a channel-based coordination pattern."
    );
    assert_eq!(content[2]["text"], "Here is the implementation.");
    assert_eq!(body["stop_reason"], "end_turn");
    assert!(
        !content
            .iter()
            .any(|b| b["type"] == "tool_use" && b["name"] == "advisor"),
        "no client tool_use named advisor"
    );

    // iterations[]: message, advisor_message, message — in order, with the
    // advisor's own tokens NOT folded into the top-level totals.
    let iterations = body["usage"]["iterations"].as_array().unwrap();
    assert_eq!(iterations.len(), 3);
    assert_eq!(iterations[0]["type"], "message");
    assert_eq!(iterations[1]["type"], "advisor_message");
    assert_eq!(iterations[1]["model"], "beam");
    assert_eq!(iterations[1]["input_tokens"], 21);
    assert_eq!(iterations[1]["output_tokens"], 13);
    assert_eq!(iterations[2]["type"], "message");
    // Top-level usage is the sum of the EXECUTOR iterations (as in
    // Anthropic's own example: 412 + 1348 = 1760), and never includes the
    // advisor's tokens.
    let executor_sum = |field: &str| {
        iterations
            .iter()
            .filter(|i| i["type"] == "message")
            .map(|i| i[field].as_u64().unwrap())
            .sum::<u64>()
    };
    assert_eq!(
        body["usage"]["input_tokens"],
        executor_sum("input_tokens"),
        "every executor run, not only the last"
    );
    assert_eq!(
        body["usage"]["output_tokens"],
        executor_sum("output_tokens")
    );
    assert_eq!(body["usage"]["input_tokens"], 16);
}

/// The executor's own words and reasoning from a round reach its re-run: the
/// re-run's input continues with what it said (reasoning folded in, under
/// the default `text` replay mode), then the call, then the call's output.
#[tokio::test]
async fn the_rerun_input_replays_what_the_executor_said_in_the_round() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    let first = FakeResponse {
        status: StatusCode::OK,
        body: Some(
            r#"{
                "id": "resp_call",
                "object": "response",
                "created_at": 1,
                "status": "completed",
                "model": "gpt-4o-mini",
                "output": [
                    {"type": "reasoning", "id": "rs_1",
                     "summary": [{"type": "summary_text", "text": "A second opinion would help."}],
                     "encrypted_content": "ENC"},
                    {"type": "message", "id": "msg_1", "role": "assistant",
                     "content": [{"type": "output_text", "text": "Let me check my plan.", "annotations": []}]},
                    {"type": "function_call", "call_id": "call_adv_1", "name": "advisor", "arguments": "{}"}
                ],
                "usage": {"input_tokens":9,"output_tokens":2,"total_tokens":11}
            }"#
            .to_owned(),
        ),
    };
    script(
        &upstream,
        vec![
            first,
            advisor_answer("Use a channel-based coordination pattern."),
            advisor_executor_message("resp_final", "Here is the implementation."),
        ],
    );
    let proxy_addr = start_proxy(advisor_config(upstream_addr)).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(sent.len(), 3, "executor, advisor, executor");
    let first_body: serde_json::Value = serde_json::from_slice(&sent[0]).unwrap();
    let rerun: serde_json::Value = serde_json::from_slice(&sent[2]).unwrap();
    let before = first_body["input"].as_array().unwrap().len();
    let items = rerun["input"].as_array().unwrap();
    let added = &items[before..];

    assert_eq!(added.len(), 3, "message, call, output: {added:#?}");
    assert_eq!(added[0]["type"], "message");
    assert_eq!(added[0]["role"], "assistant");
    assert_eq!(
        added[0]["content"][0]["text"],
        "<thinking>\nA second opinion would help.\n</thinking>\n\nLet me check my plan."
    );
    assert_eq!(added[1]["type"], "function_call");
    assert_eq!(added[1]["call_id"], "call_adv_1");
    assert_eq!(added[2]["type"], "function_call_output");
    assert_eq!(added[2]["call_id"], "call_adv_1");
    assert_eq!(
        items
            .iter()
            .filter(|i| i["type"] == "function_call")
            .count(),
        1,
        "the call is replayed exactly once"
    );
}

/// The advisor sees the round it was called from: the executor's words
/// that led to the consultation and the triggering call itself.
#[tokio::test]
async fn the_advisor_transcript_includes_the_round_that_called_it() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    let first = FakeResponse {
        status: StatusCode::OK,
        body: Some(
            r#"{
                "id": "resp_call",
                "object": "response",
                "created_at": 1,
                "status": "completed",
                "model": "gpt-4o-mini",
                "output": [
                    {"type": "message", "id": "msg_1", "role": "assistant",
                     "content": [{"type": "output_text", "text": "My plan: use a mutex per shard.", "annotations": []}]},
                    {"type": "function_call", "call_id": "call_adv_1", "name": "advisor", "arguments": "{}"}
                ],
                "usage": {"input_tokens":9,"output_tokens":2,"total_tokens":11}
            }"#
            .to_owned(),
        ),
    };
    script(
        &upstream,
        vec![
            first,
            advisor_answer("Use a channel-based coordination pattern."),
            advisor_executor_message("resp_final", "Here is the implementation."),
        ],
    );
    let proxy_addr = start_proxy(advisor_config(upstream_addr)).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(sent.len(), 3, "executor, advisor, executor");
    let advisor_body: serde_json::Value = serde_json::from_slice(&sent[1]).unwrap();
    assert_eq!(advisor_body["model"], "beam");
    let transcript = advisor_body["input"][0]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(
        transcript.contains("My plan: use a mutex per shard."),
        "the executor's words from the round: {transcript}"
    );
    assert!(
        transcript.contains("call_adv_1"),
        "the triggering call: {transcript}"
    );
    assert!(
        !transcript.contains("Use a channel-based"),
        "no advice before it is given"
    );
}

/// A call dropped because an earlier call in the same response exhausted its
/// tool is already in the executor's input, so it is answered with a "not
/// run" output instead of being left dangling.
#[tokio::test]
async fn a_call_dropped_after_exhaustion_mid_response_is_still_answered() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    let calls: Vec<String> = (1..=4)
        .map(|n| {
            format!(
                r#"{{"type": "function_call", "call_id": "call_adv_{n}", "name": "advisor", "arguments": "{{}}"}}"#
            )
        })
        .collect();
    let first = FakeResponse {
        status: StatusCode::OK,
        body: Some(format!(
            r#"{{
                "id": "resp_call",
                "object": "response",
                "created_at": 1,
                "status": "completed",
                "model": "gpt-4o-mini",
                "output": [{}],
                "usage": {{"input_tokens":9,"output_tokens":2,"total_tokens":11}}
            }}"#,
            calls.join(",")
        )),
    };
    // `max_uses: 2`: calls 1 and 2 consult, call 3 is refused (exhausting
    // the tool), call 4 is dropped.
    script(
        &upstream,
        vec![
            first,
            advisor_answer("Advice one."),
            advisor_answer("Advice two."),
            advisor_executor_message("resp_final", "Done."),
        ],
    );
    let proxy_addr = start_proxy(advisor_config(upstream_addr)).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(
        body["content"].as_array().unwrap().last().unwrap()["text"],
        "Done."
    );

    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(sent.len(), 4, "executor, advisor x2, executor");
    let rerun: serde_json::Value = serde_json::from_slice(&sent[3]).unwrap();
    let items = rerun["input"].as_array().unwrap();
    for n in 1..=4 {
        let id = format!("call_adv_{n}");
        let calls = items
            .iter()
            .filter(|i| i["type"] == "function_call" && i["call_id"] == id)
            .count();
        let outputs: Vec<&serde_json::Value> = items
            .iter()
            .filter(|i| i["type"] == "function_call_output" && i["call_id"] == id)
            .collect();
        assert_eq!(calls, 1, "{id} replayed once");
        assert_eq!(outputs.len(), 1, "{id} answered exactly once");
        if n == 4 {
            assert!(
                outputs[0]["output"]
                    .as_str()
                    .unwrap()
                    .starts_with("This call was not run"),
                "{:?}",
                outputs[0]
            );
        }
    }
}

/// 3. Fail closed. A 404 from the advisor (a model rejection shape!) must
/// NOT trigger `default_model`, must NOT fail the client, and must yield
/// `error_code: "unavailable"` with the executor's final answer intact.
#[tokio::test]
async fn advisor_failure_degrades_to_unavailable() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            advisor_call_response("resp_call", "call_adv_fail"),
            FakeResponse {
                status: StatusCode::NOT_FOUND,
                body: Some(REAL_GATEWAY_MODEL_404.into()),
            },
            advisor_executor_message("resp_final", "Answer without advice."),
        ],
    );
    // A default_model IS configured: if the advisor error ever went through
    // Attempt::send_success, this would produce a 4th POST.
    let mut config = (*advisor_config(upstream_addr)).clone();
    config.model_aliases = ModelAliases {
        map: BTreeMap::new(),
        default_model: Some("gpt-4o-mini".into()),
    };
    let proxy_addr = start_proxy(Arc::new(config)).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK, "fail closed, never an error");
    let body: serde_json::Value = res.json().await.unwrap();

    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(sent.len(), 3, "no fallback POST, no 4th request");
    let bodies: Vec<serde_json::Value> = sent
        .iter()
        .map(|b| serde_json::from_slice(b).unwrap())
        .collect();
    // The advisor 404 is a MODEL-REJECTION shape. If it had gone through the
    // executor's fallback path there would be an extra POST; the exact
    // 3-request sequence (and the unchanged executor model on the last
    // request) is the proof that no fallback POST ever happened.
    assert_eq!(
        bodies[1]["model"], "beam",
        "the 404 was the advisor's own POST"
    );
    assert_eq!(
        bodies[2]["model"], "gpt-4o-mini",
        "unchanged executor model"
    );
    assert!(
        !bodies[2]["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["type"] == "function_call" && i["name"] != "advisor"),
        "no re-translated fallback request"
    );
    // The executor still receives a (non-advice) function_call_output so the
    // upstream never sees an orphan function_call.
    let items = bodies[2]["input"].as_array().unwrap();
    assert_eq!(
        items
            .iter()
            .filter(|i| i["type"] == "function_call_output")
            .count(),
        1
    );
    let failure_output = items
        .iter()
        .find(|i| i["type"] == "function_call_output")
        .unwrap()["output"]
        .as_str()
        .unwrap();
    assert!(
        failure_output.contains("could not be consulted")
            && failure_output.contains("request to it was refused"),
        "the executor is told why, and not that a review happened: {failure_output}"
    );

    let content = body["content"].as_array().unwrap();
    assert_eq!(content[0]["type"], "server_tool_use");
    assert_eq!(content[1]["type"], "advisor_tool_result");
    assert_eq!(content[0]["id"], content[1]["tool_use_id"]);
    assert_eq!(content[1]["content"]["type"], "advisor_tool_result_error");
    assert_eq!(content[1]["content"]["error_code"], "unavailable");
    assert!(
        content
            .iter()
            .any(|b| b["text"] == "Answer without advice."),
        "the executor's final answer still arrives"
    );
    assert_eq!(body["stop_reason"], "end_turn");
    let iterations = body["usage"]["iterations"].as_array().unwrap();
    assert_eq!(
        iterations.len(),
        2,
        "two executor messages, no advisor billing"
    );
    assert!(iterations.iter().all(|i| i["type"] == "message"));
}

/// 3c. The advisor's per-request timeout really is enforced: the fake
/// upstream stalls ONLY the advisor POST (attempt 2) while the executor
/// calls stay fast, so a 1-second `timeout_secs` must cut it off without
/// failing the client or touching `default_model`.
#[tokio::test]
async fn advisor_timeout_degrades_gracefully() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            advisor_call_response("resp_call", "call_timeout"),
            advisor_answer("never arrives"),
            advisor_answer("never arrives"),
            advisor_executor_message("resp_final", "never arrives"),
        ],
    );
    // Attempt 1 = executor (fast); attempts 2 and 3 = the advisor and its
    // one retry (both stalled); attempt 4 = final executor (fast).
    *upstream.delay_per_attempt.lock().await = VecDeque::from([
        Duration::ZERO,
        Duration::from_secs(3),
        Duration::from_secs(3),
        Duration::ZERO,
    ]);
    let mut config = (*advisor_config(upstream_addr)).clone();
    config.advisor.timeout_secs = Some(1);
    let proxy_addr = start_proxy(Arc::new(config)).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK, "fail closed on a timeout");
    let body: serde_json::Value = res.json().await.unwrap();
    let result = body["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["type"] == "advisor_tool_result")
        .unwrap();
    assert_eq!(result["content"]["error_code"], "execution_time_exceeded");
    assert!(
        body["content"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["text"] == "never arrives"),
        "the executor's NEXT canned answer arrived; the timeout did not fail the turn"
    );
    // The fake upstream records each stalled advisor POST before the client
    // times out: the advisor and its ONE retry (2nd and 3rd), then the final
    // executor call (4th). No 5th request: a timeout never triggers a
    // default_model fallback, and the advisor is retried only once.
    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(sent.len(), 4, "one retry, no fallback POST after it");
    let bodies: Vec<serde_json::Value> = sent
        .iter()
        .map(|b| serde_json::from_slice(b).unwrap())
        .collect();
    assert_eq!(bodies[0]["model"], "gpt-4o-mini");
    assert_eq!(bodies[1]["model"], "beam", "the timed-out advisor POST");
    assert_eq!(bodies[2]["model"], "beam", "its retry");
    assert_eq!(bodies[3]["model"], "gpt-4o-mini");
}

/// 3b. Repeated advisor failures stay fail-closed, and at most ONE
/// operator-visible warning is emitted for the whole turn (the loop
/// consults twice, both times unsuccessfully).
/// A tracing writer that collects everything into a shared buffer.
#[derive(Clone, Default)]
struct LogCapture(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for LogCapture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogCapture {
    type Writer = Self;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Every failed consultation reaches the tracing log (the operator's record
/// of how often the advisor fails), including the failed attempt that was
/// retried, while the TUI still gets a single line (see below).
#[tokio::test]
async fn advisor_logs_every_failure_even_though_the_tui_shows_one() {
    // A global subscriber: tests run on separate threads, and a thread-local
    // one is defeated by tracing's callsite cache once another thread has hit
    // the same callsite with no subscriber. Other tests' warnings land in the
    // buffer too, so this test picks out its own lines by a unique marker in
    // the upstream error text.
    static CAPTURE: std::sync::OnceLock<LogCapture> = std::sync::OnceLock::new();
    let capture = CAPTURE
        .get_or_init(|| {
            let capture = LogCapture::default();
            tracing::subscriber::set_global_default(
                tracing_subscriber::fmt()
                    .with_writer(capture.clone())
                    .with_ansi(false)
                    .with_max_level(tracing::Level::WARN)
                    .finish(),
            )
            .expect("no other test installs a global subscriber");
            capture
        })
        .clone();
    const MARK: &str = "overloaded-7f3a91";

    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            advisor_call_response("resp_c1", "call_t1"),
            // A 503 is retried once, and the retry fails too.
            FakeResponse {
                status: StatusCode::SERVICE_UNAVAILABLE,
                body: Some(format!(r#"{{"error":{{"message":"{MARK}"}}}}"#)),
            },
            FakeResponse {
                status: StatusCode::SERVICE_UNAVAILABLE,
                body: Some(format!(r#"{{"error":{{"message":"{MARK}"}}}}"#)),
            },
            advisor_call_response("resp_c2", "call_t2"),
            FakeResponse {
                status: StatusCode::NOT_FOUND,
                body: Some(REAL_GATEWAY_MODEL_404.replace("claude-sonnet-5", MARK)),
            },
            advisor_executor_message("resp_final", "Answer without advice."),
        ],
    );
    let mut config = (*advisor_config(upstream_addr)).clone();
    config.advisor.max_uses = Some(2);
    let proxy_addr = start_proxy(Arc::new(config)).await;
    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let log = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    let failures: Vec<&str> = log
        .lines()
        .filter(|l| l.contains(MARK))
        .filter(|l| l.contains("advisor consultation failed; continuing without advice"))
        .collect();
    assert_eq!(
        failures.len(),
        2,
        "one log line per failed consultation:\n{log}"
    );
    assert!(
        failures[0].contains("failure=\"overloaded\""),
        "{}",
        failures[0]
    );
    assert!(
        failures[0].contains("failures_this_turn=1"),
        "{}",
        failures[0]
    );
    assert!(failures[0].contains("prompt_chars="), "{}", failures[0]);
    assert!(
        failures[1].contains("failure=\"unavailable\""),
        "{}",
        failures[1]
    );
    assert!(
        failures[1].contains("failures_this_turn=2"),
        "{}",
        failures[1]
    );
    assert_eq!(
        log.lines()
            .filter(|l| l.contains(MARK))
            .filter(|l| l.contains("attempt failed; retrying once"))
            .count(),
        1,
        "the failed attempt that was retried is logged:\n{log}"
    );
}

#[tokio::test]
async fn advisor_repeated_failure_emits_one_warning() {
    use openai_to_anthropic_proxy::SessionStatsStore;
    use openai_to_anthropic_proxy::tui::LogKind;

    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            advisor_call_response("resp_c1", "call_t1"),
            // A 503 is retried once, and the retry fails too.
            FakeResponse {
                status: StatusCode::SERVICE_UNAVAILABLE,
                body: Some(r#"{"error":{"message":"overloaded"}}"#.into()),
            },
            FakeResponse {
                status: StatusCode::SERVICE_UNAVAILABLE,
                body: Some(r#"{"error":{"message":"overloaded"}}"#.into()),
            },
            advisor_call_response("resp_c2", "call_t2"),
            // A 404 is permanent, so it is not retried.
            FakeResponse {
                status: StatusCode::NOT_FOUND,
                body: Some(REAL_GATEWAY_MODEL_404.into()),
            },
            advisor_executor_message("resp_final", "Answer without advice."),
        ],
    );
    let mut config = (*advisor_config(upstream_addr)).clone();
    config.advisor.max_uses = Some(2);
    let stats = Arc::new(SessionStatsStore::new());
    let (output, log_rx) = openai_to_anthropic_proxy::OutputSink::tui();
    let proxy_addr = start_proxy_with_captured_stores(Arc::new(config), stats, output).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = res.json().await.unwrap();
    let errors: Vec<&serde_json::Value> = body["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|b| b["type"] == "advisor_tool_result")
        .collect();
    assert_eq!(errors.len(), 2);
    // The 503 (retried, still failing) is "overloaded"; the 404 is a refused
    // request, which has no more specific client code than "unavailable".
    let codes: Vec<&serde_json::Value> =
        errors.iter().map(|e| &e["content"]["error_code"]).collect();
    assert_eq!(codes, ["overloaded", "unavailable"]);
    assert!(
        body["content"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["text"] == "Answer without advice."),
        "the executor's answer survives two advisor failures"
    );
    assert_eq!(
        body["usage"]["iterations"].as_array().unwrap().len(),
        3,
        "three executor samplings, no advisor billing"
    );
    let lines: Vec<_> = std::iter::from_fn(|| log_rx.try_recv().ok()).collect();
    let warnings = lines
        .iter()
        .filter(|l| l.kind == LogKind::Warning && l.text.contains("advisor"))
        .count();
    assert_eq!(
        warnings, 1,
        "at most one advisor warning for the whole turn"
    );
}

/// 4. `max_uses` caps the loop. The executor keeps asking for advice; the
/// proxy must stop after `max_uses` consultations, inject a
/// `function_call_output` for the pending (3rd) call and re-run the executor
/// ONE final time for a real answer. That extra re-run is TERMINAL: if it
/// asks for advice again, no more POSTs are made and the orphan tool_use is
/// stripped. Six POSTs is the hard bound for this script.
#[tokio::test]
async fn advisor_max_uses_caps_the_loop() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            advisor_call_response("resp_c1", "call_c1"),
            advisor_answer("Advice one."),
            advisor_call_response("resp_c2", "call_c2"),
            advisor_answer("Advice two."),
            // The cap-triggering 3rd advisor call (no consultation).
            advisor_call_response("resp_c3", "call_c3"),
            // The ONE terminal re-run: still asks for advice. Render it.
            advisor_executor_message("resp_final", "Final answer after the cap."),
        ],
    );
    let mut config = (*advisor_config(upstream_addr)).clone();
    config.advisor.max_uses = Some(2);
    let proxy_addr = start_proxy(Arc::new(config)).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = res.json().await.unwrap();

    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(
        sent.len(),
        6,
        "2 executor + 2 advisor + capped 3rd executor + ONE terminal re-run; no 7th POST"
    );
    let bodies: Vec<serde_json::Value> = sent
        .iter()
        .map(|b| serde_json::from_slice(b).unwrap())
        .collect();
    // The terminal re-run carries the FunctionCallOutput for the cap call.
    assert!(
        bodies[5]["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["type"] == "function_call_output"
                && i["call_id"] == "call_c3"
                && i["output"].as_str().unwrap().contains("limit reached")),
        "the cap-triggering call got a FunctionCallOutput"
    );
    // No orphan advisor function_call in ANY upstream request.
    for (i, b) in bodies.iter().enumerate() {
        assert!(
            !b["input"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["type"] == "function_call"
                    && item["name"] == "advisor"
                    && !b["input"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|o| o["type"] == "function_call_output"
                            && o["call_id"] == item["call_id"])),
            "attempt {i}: every advisor function_call has a matching output"
        );
    }
    let content = body["content"].as_array().unwrap();
    let results: Vec<&serde_json::Value> = content
        .iter()
        .filter(|b| b["type"] == "advisor_tool_result")
        .collect();
    assert_eq!(results.len(), 3, "2 successful pairs + the cap result");
    assert_eq!(results[0]["content"]["type"], "advisor_result");
    assert_eq!(results[1]["content"]["type"], "advisor_result");
    assert_eq!(results[2]["content"]["type"], "advisor_tool_result_error");
    assert_eq!(results[2]["content"]["error_code"], "max_uses_exceeded");
    // Every pair is contiguous with a matching id (the first 6 blocks).
    for pair in content.chunks(2).take(3) {
        assert_eq!(pair[0]["type"], "server_tool_use");
        assert_eq!(pair[1]["type"], "advisor_tool_result");
        assert_eq!(pair[0]["id"], pair[1]["tool_use_id"]);
    }
    assert!(
        content
            .iter()
            .any(|b| b["text"] == "Final answer after the cap."),
        "the terminal re-run's real answer must be rendered"
    );
    assert_eq!(body["stop_reason"], "end_turn");
}

/// `max_uses = 0` means "no limit": the consultation loop is bounded ONLY
/// by the executor's behaviour. This test drives the executor to ask 12
/// times (past the runaway threshold of 10) and proves that EVERY ask is
/// answered — no `max_uses_exceeded`, no early stop. The advisor imposes no
/// ceiling of its own; it only warns (once at startup, and once per turn past
/// 10 consultations). The SHARED round limit (`server_tools.max_rounds`, 10 by
/// default) is lifted too, or it would end the turn first.
#[tokio::test]
async fn advisor_unlimited_max_uses_never_hits_a_cap() {
    const CONSULTATIONS: usize = 12;
    let (upstream_addr, upstream) = start_fake_upstream().await;
    let mut scripted: Vec<FakeResponse> = Vec::new();
    for i in 0..CONSULTATIONS {
        scripted.push(advisor_call_response(
            &format!("resp_c{i}"),
            &format!("call_c{i}"),
        ));
        scripted.push(advisor_answer(&format!("Advice number {i}.")));
    }
    scripted.push(advisor_executor_message(
        "resp_final",
        "Done after many calls.",
    ));
    script(&upstream, scripted);

    let mut config = (*advisor_config(upstream_addr)).clone();
    config.advisor.max_uses = None;
    // Unlimited consultations also need the shared round limit lifted: by
    // default a turn runs at most 10 rounds, like Anthropic's own loop.
    config.server_tools.max_rounds = None;
    let proxy_addr = start_proxy(Arc::new(config)).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = res.json().await.unwrap();

    let content = body["content"].as_array().unwrap();
    let results: Vec<&serde_json::Value> = content
        .iter()
        .filter(|b| b["type"] == "advisor_tool_result")
        .collect();
    assert_eq!(
        results.len(),
        CONSULTATIONS,
        "every consultation was answered — the loop has no ceiling"
    );
    for (i, r) in results.iter().enumerate() {
        assert_eq!(r["content"]["type"], "advisor_result");
        assert_eq!(r["content"]["text"], format!("Advice number {i}."));
    }
    assert!(
        !content
            .iter()
            .any(|b| b["content"]["error_code"] == "max_uses_exceeded"),
        "an unlimited max_uses must never produce a cap error"
    );
    assert!(
        content
            .iter()
            .any(|b| b["text"] == "Done after many calls.")
    );

    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(
        sent.len(),
        CONSULTATIONS * 2 + 1,
        "12 executor calls + 12 advisor calls + the final executor message"
    );
}

/// DEFECT 3: a resolved `Some(0)` must mean exactly what a CONFIGURED `0`
/// means — "no limit". Before the fix, `Some(0)` meant "zero
/// consultations" (the cap fires on the first ask) and "a zero-token
/// answer", so the same number had two meanings depending on how the
/// config was built.
#[tokio::test]
async fn a_zero_limit_in_a_built_config_means_no_limit() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            advisor_call_response("resp_call1", "call_zero1"),
            advisor_answer("Advice one."),
            advisor_call_response("resp_call2", "call_zero2"),
            advisor_answer("Advice two."),
            advisor_executor_message("resp_final", "Done after two consultations."),
        ],
    );
    let mut config = (*advisor_config(upstream_addr)).clone();
    config.advisor.max_uses = Some(0);
    config.advisor.max_tokens = Some(0);
    let proxy_addr = start_proxy(Arc::new(config)).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = res.json().await.unwrap();
    let content = body["content"].as_array().unwrap();
    let results = content
        .iter()
        .filter(|b| b["type"] == "advisor_tool_result")
        .count();
    assert_eq!(
        results, 2,
        "both consultations were answered — `Some(0)` is not a zero-consultation cap"
    );
    assert!(
        !content
            .iter()
            .any(|b| b["content"]["error_code"] == "max_uses_exceeded"),
        "an unlimited max_uses must never produce a cap error"
    );

    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(
        sent.len(),
        5,
        "2 executor calls + 2 advisor calls + the final executor message"
    );
    let bodies: Vec<serde_json::Value> = sent
        .iter()
        .map(|b| serde_json::from_slice(b).unwrap())
        .collect();
    // `max_tokens: Some(0)` = "no limit" => the proxy omits the cap and the
    // upstream's own default applies.
    assert!(
        bodies[1].get("max_output_tokens").is_none(),
        "no cap on the advisor's own request: {}",
        bodies[1]
    );
    assert!(
        bodies[3].get("max_output_tokens").is_none(),
        "no cap on the advisor's own request: {}",
        bodies[3]
    );
}

/// DEFECT 3: `timeout_secs: Some(0)` in a built config means "no limit",
/// exactly like a configured `0` — no per-request timeout override, so the
/// GLOBAL `request_timeout` is the only bound. Proof: the advisor POST is
/// stalled for 2s — longer than the 1s a per-request timeout would allow —
/// and it still succeeds.
#[tokio::test]
async fn a_zero_timeout_in_a_built_config_means_no_limit() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            advisor_call_response("resp_call", "call_zero_timeout"),
            advisor_answer("Slow but useful advice."),
            advisor_executor_message("resp_final", "Here is the implementation."),
        ],
    );
    // Attempt 1 = executor (fast); attempt 2 = advisor (stalled 2s, which
    // is LONGER than the 1s per-request timeout the bounded case uses);
    // attempt 3 = final executor (fast).
    *upstream.delay_per_attempt.lock().await =
        VecDeque::from([Duration::ZERO, Duration::from_secs(2), Duration::ZERO]);
    let mut config = (*advisor_config(upstream_addr)).clone();
    config.advisor.timeout_secs = Some(0);
    // The global bound still applies — it is simply the ONLY bound.
    config.request_timeout = Duration::from_secs(10);
    let proxy_addr = start_proxy(Arc::new(config)).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK, "fail closed on a timeout");
    let body: serde_json::Value = res.json().await.unwrap();
    let result = body["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["type"] == "advisor_tool_result")
        .unwrap();
    assert_eq!(
        result["content"]["type"], "advisor_result",
        "the stalled advisor call still completed: no per-request timeout was applied"
    );
    assert_eq!(result["content"]["text"], "Slow but useful advice.");
}

/// `max_tokens = 0` means "no limit": the proxy must OMIT
/// `max_output_tokens` from the advisor's OWN upstream request, so the
/// UPSTREAM's default applies (it may well be smaller than 2048). The
/// executor's own requests are untouched — they never carried the field.
#[tokio::test]
async fn advisor_unlimited_max_tokens_omits_the_cap() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            advisor_call_response("resp_call", "call_adv_unlim"),
            advisor_answer("Use a channel-based coordination pattern."),
            advisor_executor_message("resp_final", "Here is the implementation."),
        ],
    );
    let mut config = (*advisor_config(upstream_addr)).clone();
    config.advisor.max_tokens = None;
    let proxy_addr = start_proxy(Arc::new(config)).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = res.json().await.unwrap();
    assert!(
        body["content"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["type"] == "advisor_tool_result"),
        "the consultation still happened"
    );

    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(sent.len(), 3, "executor, advisor, executor");
    let bodies: Vec<serde_json::Value> = sent
        .iter()
        .map(|b| serde_json::from_slice(b).unwrap())
        .collect();
    assert_eq!(bodies[1]["model"], "beam", "the advisor's own POST");
    assert!(
        bodies[1].get("max_output_tokens").is_none(),
        "no limit => no cap on the advisor's own request: {}",
        bodies[1]
    );
    // The bounded case (the default config) DOES send the cap — proven
    // here so the omission above cannot be an accident of the wiring.
    let (bounded_addr, bounded) = start_fake_upstream().await;
    script(
        &bounded,
        vec![
            advisor_call_response("resp_call", "call_adv_bounded"),
            advisor_answer("Advice."),
            advisor_executor_message("resp_final", "Done."),
        ],
    );
    let bounded_addr = start_proxy(advisor_config(bounded_addr)).await;
    let res = reqwest::Client::new()
        .post(format!("http://{bounded_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let sent = bounded.received_all.lock().await.clone();
    let bodies: Vec<serde_json::Value> = sent
        .iter()
        .map(|b| serde_json::from_slice(b).unwrap())
        .collect();
    assert_eq!(
        bodies[1]["max_output_tokens"], 164,
        "the bounded case: answer 64 + reasoning 100"
    );
}

/// `timeout_secs = 0` means "no limit": no per-request timeout override is
/// sent, so the GLOBAL `request_timeout` (the reqwest client's own timeout)
/// is the only bound. Proof: the advisor POST is stalled for 2s — longer
/// than the 1s a per-request timeout would allow — and it still succeeds.
/// (The bounded counterpart is `advisor_timeout_degrades_gracefully`: the
/// same stall with `timeout_secs: 1` degrades to `unavailable`.)
#[tokio::test]
async fn advisor_unlimited_timeout_sends_no_per_request_timeout() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            advisor_call_response("resp_call", "call_adv_unlim"),
            advisor_answer("Slow but useful advice."),
            advisor_executor_message("resp_final", "Here is the implementation."),
        ],
    );
    // Attempt 1 = executor (fast); attempt 2 = advisor (stalled 2s, which
    // is LONGER than the 1s per-request timeout the bounded case uses);
    // attempt 3 = final executor (fast).
    *upstream.delay_per_attempt.lock().await =
        VecDeque::from([Duration::ZERO, Duration::from_secs(2), Duration::ZERO]);
    let mut config = (*advisor_config(upstream_addr)).clone();
    config.advisor.timeout_secs = None;
    // The global bound still applies — it is simply the ONLY bound.
    config.request_timeout = Duration::from_secs(10);
    let proxy_addr = start_proxy(Arc::new(config)).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK, "fail closed on a timeout");
    let body: serde_json::Value = res.json().await.unwrap();
    let result = body["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["type"] == "advisor_tool_result")
        .unwrap();
    assert_eq!(
        result["content"]["type"], "advisor_result",
        "the stalled advisor call still completed: no per-request timeout was applied"
    );
    assert_eq!(result["content"]["text"], "Slow but useful advice.");
}

/// A `default_model` fallback RE-TRANSLATES the executor request through
/// `build_outbound`, so the advisor `prepare` hook must re-apply there too:
/// the fallback body still carries exactly one zero-argument advisor tool
/// and still forces `stream:false`. Without this, an advisor turn that
/// triggers the fallback would silently lose the tool.
#[tokio::test]
async fn advisor_fallback_retranslation_keeps_the_tool() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            FakeResponse {
                status: StatusCode::NOT_FOUND,
                body: Some(REAL_GATEWAY_MODEL_404.into()),
            },
            advisor_call_response("resp_call", "call_fb"),
            advisor_answer("Advice after fallback."),
            advisor_executor_message("resp_final", "Done after fallback."),
        ],
    );
    let mut config = (*advisor_config(upstream_addr)).clone();
    config.model_aliases = ModelAliases {
        map: BTreeMap::new(),
        default_model: Some("gpt-4o-mini".into()),
    };
    let proxy_addr = start_proxy(Arc::new(config)).await;
    // The request must name a DIFFERENT model than `default_model` for the
    // fallback to fire; `REAL_GATEWAY_MODEL_404` names claude-sonnet-5.
    let request = advisor_request(false).replace(
        "\"model\": \"gpt-4o-mini\"",
        "\"model\": \"claude-sonnet-5\"",
    );

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(request))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["stop_reason"], "end_turn");
    assert_eq!(body["content"][0]["type"], "server_tool_use");
    assert_eq!(
        body["content"][1]["content"]["text"],
        "Advice after fallback."
    );

    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(sent.len(), 4, "404 + fallback + advisor + final executor");
    let bodies: Vec<serde_json::Value> = sent
        .iter()
        .map(|b| serde_json::from_slice(b).unwrap())
        .collect();
    assert_eq!(
        bodies[0]["model"], "claude-sonnet-5",
        "rejected first model"
    );
    assert_eq!(bodies[1]["model"], "gpt-4o-mini", "fallback model");
    assert_eq!(bodies[2]["model"], "beam", "the advisor's own POST");
    assert!(bodies[2]["tools"].is_null(), "advisor POST has no tools");
    assert_eq!(bodies[3]["model"], "gpt-4o-mini");
    for i in [0usize, 1, 3] {
        let v = &bodies[i];
        let tools = v["tools"].as_array().unwrap();
        let advisor_tools: Vec<_> = tools.iter().filter(|t| t["name"] == "advisor").collect();
        assert_eq!(
            advisor_tools.len(),
            1,
            "attempt {i}: exactly one synthesized advisor tool"
        );
        assert_eq!(
            advisor_tools[0]["parameters"]["properties"],
            serde_json::json!({})
        );
        assert_eq!(v["stream"], false, "attempt {i}: still buffered");
    }
    // The 4th (final executor) POST carries the advice injection.
    assert!(
        bodies[3]["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["type"] == "function_call_output"
                && i["output"] == "Advice after fallback.")
    );
}

/// 5. `stream:true` still yields a proper Anthropic SSE sequence even though
/// every upstream request was forced to `stream:false`.
#[tokio::test]
async fn advisor_stream_true_emits_sse() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            advisor_call_response("resp_call", "call_sse"),
            advisor_answer("Keep it simple."),
            advisor_executor_message("resp_final", "Done."),
        ],
    );
    let proxy_addr = start_proxy(advisor_config(upstream_addr)).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(true)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers()
            .get(axum::http::header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap(),
        "text/event-stream",
        "hard-coded; the upstream answered application/json"
    );
    assert_eq!(
        res.headers()
            .get(axum::http::header::CACHE_CONTROL)
            .unwrap()
            .to_str()
            .unwrap(),
        "no-cache"
    );
    let text = res.text().await.unwrap();
    assert!(!text.contains("[DONE]"));

    let events: Vec<&str> = text
        .split("\n\n")
        .filter(|s| !s.trim().is_empty())
        .collect();
    let kinds: Vec<&str> = events
        .iter()
        .filter_map(|e| e.lines().find_map(|l| l.strip_prefix("event: ")))
        .collect();
    assert_eq!(kinds.iter().filter(|k| **k == "message_start").count(), 1);
    assert_eq!(kinds.iter().filter(|k| **k == "message_stop").count(), 1);
    assert_eq!(kinds.iter().filter(|k| **k == "message_delta").count(), 1);
    assert_eq!(kinds.first(), Some(&"message_start"));
    assert_eq!(kinds.last(), Some(&"message_stop"));

    // The advisor Raw blocks arrive whole (start + stop, no delta), and the
    // final message_delta carries stop_reason + usage.iterations[].
    let mut deltas = 0usize;
    let mut raw_starts = Vec::new();
    for e in &events {
        let Some(data) = e.lines().find_map(|l| l.strip_prefix("data: ")) else {
            continue;
        };
        let v: serde_json::Value = serde_json::from_str(data).unwrap();
        match v["type"].as_str().unwrap() {
            "content_block_start" => {
                let t = v["content_block"]["type"].as_str().unwrap();
                if t == "server_tool_use" || t == "advisor_tool_result" {
                    raw_starts.push(t.to_owned());
                }
            }
            "content_block_delta" => deltas += 1,
            "message_delta" => {
                assert_eq!(v["delta"]["stop_reason"], "end_turn");
                let iterations = v["usage"]["iterations"].as_array().unwrap();
                assert!(
                    iterations
                        .iter()
                        .any(|i| i["type"] == "advisor_message" && i["model"] == "beam")
                );
            }
            _ => {}
        }
    }
    assert_eq!(
        raw_starts,
        vec!["server_tool_use", "advisor_tool_result"],
        "the pair arrives in one message_start … message_stop"
    );
    assert_eq!(deltas, 1, "only the final executor text gets a delta");

    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(sent.len(), 3);
    // The executor runs are streamed upstream (the client streams); the
    // advisor's own consultation is not.
    let streams: Vec<bool> = sent
        .iter()
        .map(|body| {
            serde_json::from_slice::<serde_json::Value>(body).unwrap()["stream"]
                .as_bool()
                .unwrap()
        })
        .collect();
    assert_eq!(streams, vec![true, false, true]);
}

/// 6. Claude Code echoes both advisor blocks verbatim on the next request.
/// `ContentBlockParam` now parses them explicitly; the proxy must not 400,
/// must not emit an orphan `function_call` upstream, and must replay the
/// prior advice (see `advisor_echo_prior_advice_reaches_upstream`).
#[tokio::test]
async fn advisor_inbound_echo_does_not_400() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned.lock().await = Some(completed_response_with_id("resp_echo", "ok"));

    let request = format!(
        r#"{{
            "model": "gpt-4o-mini",
            "max_tokens": 64,
            "tools": [{ADVISOR_DECLARATION}],
            "messages": [
                {{"role":"user","content":"Do the task"}},
                {{"role":"assistant","content":[
                    {{"type":"text","text":"Let me consult the advisor."}},
                    {{"type":"server_tool_use","id":"srvtoolu_echo","name":"advisor","input":{{}}}},
                    {{"type":"advisor_tool_result","tool_use_id":"srvtoolu_echo",
                      "content":{{"type":"advisor_result","text":"Prior advice."}}}}
                ]}}
            ]
        }}"#
    );
    for enabled in [false, true] {
        let mut config = (*make_proxy_config(upstream_addr)).clone();
        config.advisor.enabled = enabled;
        let proxy_addr = start_proxy(Arc::new(config)).await;
        let res = reqwest::Client::new()
            .post(format!("http://{proxy_addr}/v1/messages"))
            .header("content-type", "application/json")
            .body(ReqBody::from(request.clone()))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "enabled={enabled}");
        let body: serde_json::Value = res.json().await.unwrap();
        assert_eq!(body["content"][0]["text"], "ok", "enabled={enabled}");
    }

    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(sent.len(), 2, "one POST per request");
    for body in &sent {
        let v: serde_json::Value = serde_json::from_slice(body).unwrap();
        let items = v["input"].as_array().expect("input is an Items array");
        assert!(
            items.iter().all(|i| i["type"] != "function_call"),
            "the echoed advisor blocks must not leak in as orphan function_call items"
        );
        assert!(
            items
                .iter()
                .any(|i| i["type"] == "message" && i["role"] == "assistant"),
            "the echoed assistant message is still well-formed"
        );
        assert!(
            items.iter().all(|i| i["type"] != "function_call_output"),
            "no synthetic tool result for an echoed block"
        );
    }
}

/// THE fidelity gap: prior advice must survive into the upstream transcript.
/// Claude Code echoes `server_tool_use` + `advisor_tool_result` back on the
/// next turn. With the advisor DISABLED no new consultation fires, so the
/// upstream sees exactly ONE POST whose assistant `output_text` carries the
/// prior advice — and no orphan `function_call`. A control request without
/// the blocks must stay clean.
#[tokio::test]
async fn advisor_echo_prior_advice_reaches_upstream() {
    const PRIOR: &str = "Use a channel-based coordination pattern.";
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned.lock().await = Some(completed_response_with_id("resp_echo2", "ok"));
    // Advisor DISABLED (make_proxy_config's default). No declaration either,
    // so the ordinary non-advisor pipeline is used for both requests.
    let config = make_proxy_config(upstream_addr);
    assert!(!config.advisor.enabled);
    let proxy_addr = start_proxy(config).await;

    let with_echo = r#"{
        "model": "gpt-4o-mini",
        "max_tokens": 64,
        "messages": [
            {"role":"user","content":"Do the task"},
            {"role":"assistant","content":[
                {"type":"text","text":"Let me consult the advisor."},
                {"type":"server_tool_use","id":"srvtoolu_echo2",
                 "name":"advisor","input":{}},
                {"type":"advisor_tool_result","tool_use_id":"srvtoolu_echo2",
                 "content":{"type":"advisor_result",
                            "text":"Use a channel-based coordination pattern."}}
            ]},
            {"role":"user","content":"Continue"}
        ]
    }"#;
    let without_echo = r#"{
        "model": "gpt-4o-mini",
        "max_tokens": 64,
        "messages": [
            {"role":"user","content":"Do the task"},
            {"role":"assistant","content":[
                {"type":"text","text":"Let me consult the advisor."}
            ]},
            {"role":"user","content":"Continue"}
        ]
    }"#;

    for (label, body) in [("with echo", with_echo), ("control", without_echo)] {
        let res = reqwest::Client::new()
            .post(format!("http://{proxy_addr}/v1/messages"))
            .header("content-type", "application/json")
            .body(ReqBody::from(body))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{label}");
        let answer: serde_json::Value = res.json().await.unwrap();
        assert_eq!(answer["type"], "message", "{label}");
        assert_eq!(answer["content"][0]["type"], "text", "{label}");
        assert_eq!(answer["content"][0]["text"], "ok", "{label}");
        assert_eq!(answer["stop_reason"], "end_turn", "{label}");
    }

    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(
        sent.len(),
        2,
        "one upstream POST per request; no consultation"
    );
    let v: serde_json::Value = serde_json::from_slice(&sent[0]).unwrap();
    let items = v["input"].as_array().expect("input is an Items array");
    let assistant_parts: Vec<&serde_json::Value> = items
        .iter()
        .filter(|i| i["type"] == "message" && i["role"] == "assistant")
        .flat_map(|i| i["content"].as_array().unwrap())
        .collect();
    assert_eq!(assistant_parts.len(), 1, "one assistant content part");
    // Responses requires `output_text` on assistant messages (`input_text`
    // is rejected upstream), so the advice lives in that part's `text`.
    assert_eq!(assistant_parts[0]["type"], "output_text");
    let assistant_text = assistant_parts[0]["text"].as_str().unwrap();
    assert_eq!(
        assistant_text,
        format!("Let me consult the advisor.\n[Advisor advice from a previous turn: {PRIOR}]\n"),
        "prior advice missing or not clearly delimited: {assistant_text:?}"
    );
    assert!(
        items.iter().all(|i| i["type"] != "function_call"),
        "no orphan function_call from the echoed server_tool_use"
    );
    assert!(
        items.iter().all(|i| i["type"] != "function_call_output"),
        "no synthetic tool result for the echoed advisor block"
    );

    let control: serde_json::Value = serde_json::from_slice(&sent[1]).unwrap();
    let control_text = serde_json::to_string(&control["input"]).unwrap();
    assert!(
        !control_text.contains(PRIOR),
        "control request must carry no advice text"
    );
    assert!(!control_text.contains("Advisor advice"));
}

/// 7. The zero-argument function tool reaches the upstream with an empty
/// schema, and the `# Advisor Tool` system section is NOT stripped.
#[tokio::test]
async fn advisor_tool_declaration_reaches_upstream() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned.lock().await = Some(completed_response_with_id("resp_tool", "ok"));
    let proxy_addr = start_proxy(advisor_config(upstream_addr)).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(sent.len(), 1, "no consultation was requested");
    let v: serde_json::Value = serde_json::from_slice(&sent[0]).unwrap();
    let tools = v["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 1);
    assert_eq!(
        tools[0],
        serde_json::json!({
            "type": "function",
            "name": "advisor",
            "description": "Consult a reviewer model before substantive work. Takes no arguments.",
            "strict": true,
            "parameters": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }
        })
    );
    assert_eq!(
        tools[0]["parameters"]["properties"]
            .as_object()
            .unwrap()
            .len(),
        0
    );
    assert_eq!(v["stream"], false, "advisor path buffers the executor");
    let instructions = v["instructions"].as_str().unwrap();
    assert!(instructions.contains("# Advisor Tool"), "section preserved");
    assert!(instructions.contains("Call advisor BEFORE substantive work."));
    assert!(instructions.contains("<system-reminder>skills</system-reminder>"));
}

// ─── Regression tests for the confirmed advisor bugs (B1–B12) ──────────────
//
// All use the in-process fake upstream. No real API calls, no expensive
// model names: the only advisor model is `beam`.

/// An executor sampling that produces BOTH visible text and a sibling client
/// tool_use alongside the advisor call. Used by the B1/B6 fidelity tests.
fn advisor_call_with_text_and_bash(id: &str, call_id: &str, text: &str) -> FakeResponse {
    FakeResponse {
        status: StatusCode::OK,
        body: Some(format!(
            r#"{{
                "id": "{id}", "object": "response", "created_at": 1,
                "status": "completed", "model": "gpt-4o-mini",
                "output": [
                    {{"type": "message", "role": "assistant",
                      "content": [{{"type": "output_text", "text": "{text}", "annotations": []}}]}},
                    {{"type": "function_call", "call_id": "{call_id}",
                      "name": "advisor", "arguments": "{{}}"}},
                    {{"type": "function_call", "call_id": "call_bash_side",
                      "name": "Bash", "arguments": "{{\"command\":\"ls\"}}"}}
                ],
                "usage": {{"input_tokens":9,"output_tokens":4,"total_tokens":13}}
            }}"#
        )),
    }
}

/// An executor response with BOTH visible text and an advisor call (B3).
fn advisor_call_with_text(id: &str, call_id: &str, text: &str) -> FakeResponse {
    FakeResponse {
        status: StatusCode::OK,
        body: Some(format!(
            r#"{{
                "id": "{id}", "object": "response", "created_at": 1,
                "status": "completed", "model": "gpt-4o-mini",
                "output": [
                    {{"type": "message", "role": "assistant",
                      "content": [{{"type": "output_text", "text": "{text}", "annotations": []}}]}},
                    {{"type": "function_call", "call_id": "{call_id}",
                      "name": "advisor", "arguments": "{{}}"}}
                ],
                "usage": {{"input_tokens":9,"output_tokens":4,"total_tokens":13}}
            }}"#
        )),
    }
}

/// An advisor response with a non-`completed` status but a plausible-looking
/// `output_text`. B2: this must NEVER be accepted as advice.
fn advisor_answer_with_status(status: &str, text: &str) -> FakeResponse {
    FakeResponse {
        status: StatusCode::OK,
        body: Some(format!(
            r#"{{
                "id": "resp_advice_bad", "object": "response", "created_at": 1,
                "status": "{status}", "model": "beam",
                "output": [{{
                    "type": "message", "role": "assistant",
                    "content": [{{"type": "output_text", "text": "{text}", "annotations": []}}]
                }}],
                "usage": {{"input_tokens":21,"output_tokens":13,"total_tokens":34}}
            }}"#
        )),
    }
}

/// An advisor call with NO `usage` field (B10).
fn advisor_call_without_usage(id: &str, call_id: &str) -> FakeResponse {
    FakeResponse {
        status: StatusCode::OK,
        body: Some(format!(
            r#"{{
                "id": "{id}", "object": "response", "created_at": 1,
                "status": "completed", "model": "gpt-4o-mini",
                "output": [{{
                    "type": "function_call", "call_id": "{call_id}",
                    "name": "advisor", "arguments": "{{}}"
                }}]
            }}"#
        )),
    }
}

/// A completed response with NO `usage` field (B10).
fn response_without_usage(id: &str, text: &str) -> FakeResponse {
    FakeResponse {
        status: StatusCode::OK,
        body: Some(format!(
            r#"{{
                "id": "{id}", "object": "response", "created_at": 1,
                "status": "completed", "model": "gpt-4o-mini",
                "output": [{{
                    "type": "message", "role": "assistant",
                    "content": [{{"type": "output_text", "text": "{text}", "annotations": []}}]
                }}]
            }}"#
        )),
    }
}

/// A terminal `incomplete` / `max_output_tokens` executor response (B7).
fn executor_max_tokens_response(id: &str) -> FakeResponse {
    FakeResponse {
        status: StatusCode::OK,
        body: Some(format!(
            r#"{{
                "id": "{id}", "object": "response", "created_at": 1,
                "status": "incomplete", "model": "gpt-4o-mini",
                "incomplete_details": {{"reason": "max_output_tokens"}},
                "output": [{{
                    "type": "message", "role": "assistant",
                    "content": [{{"type": "output_text", "text": "Truncated.", "annotations": []}}]
                }}],
                "usage": {{"input_tokens":7,"output_tokens":64,"total_tokens":71}}
            }}"#
        )),
    }
}

/// B1(a): an executor RE-RUN error must NOT fail the user's turn. The last
/// good response is rendered with an `unavailable` pair, and the executor's
/// own answer survives.
#[tokio::test]
async fn b1_rerun_500_degrades_to_last_good_with_unavailable() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            advisor_call_with_text_and_bash("resp_first", "call_b1", "Let me consult the advisor."),
            advisor_answer("Use a channel."),
            FakeResponse {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                body: Some(r#"{"error":{"message":"executor exploded"}}"#.into()),
            },
        ],
    );
    let proxy_addr = start_proxy(advisor_config(upstream_addr)).await;

    let started = std::time::Instant::now();
    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        StatusCode::OK,
        "never propagate a re-run error"
    );
    let body: serde_json::Value = res.json().await.unwrap();
    assert!(started.elapsed() < Duration::from_secs(5));

    let content = body["content"].as_array().unwrap();
    assert!(
        content
            .iter()
            .any(|b| b["text"] == "Let me consult the advisor."),
        "the last good executor answer survives"
    );
    let unavailable: Vec<_> = content
        .iter()
        .filter(|b| b["type"] == "advisor_tool_result")
        .collect();
    assert!(
        unavailable
            .iter()
            .any(|b| b["content"]["error_code"] == "unavailable"),
        "a fail-closed unavailable block is emitted for the failed re-run"
    );
    // The advisor's successful pair is still contiguous and id-matched.
    let mut pair_positions = Vec::new();
    for (i, b) in content.iter().enumerate() {
        if b["type"] == "server_tool_use" {
            let id = &b["id"];
            assert_eq!(
                content[i + 1]["type"],
                "advisor_tool_result",
                "server_tool_use must be immediately followed by its result"
            );
            assert_eq!(content[i + 1]["tool_use_id"], *id);
            pair_positions.push(i);
        }
    }
    assert_eq!(pair_positions.len(), 2, "consultation pair + degraded pair");
    assert_eq!(body["stop_reason"], "tool_use", "Bash is still pending");
}

/// B1(b): a re-run 400 naming ALL THREE beta-latching tokens must not leak
/// any of them into the client body. §18.4: such a 400 makes Claude Code
/// disable the advisor for the whole process.
#[tokio::test]
async fn b1_rerun_400_with_advisor_tokens_is_scrubbed() {
    const BETA_400: &str = r#"{"error":{"type":"invalid_request_error","message":"Input tag 'advisor_20260301' found; advisor-tool-2026-03-01 beta required for advisor_20260301"}}"#;
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            advisor_call_response("resp_first", "call_b1b"),
            advisor_answer("Use a channel."),
            FakeResponse {
                status: StatusCode::BAD_REQUEST,
                body: Some(BETA_400.into()),
            },
        ],
    );
    let proxy_addr = start_proxy(advisor_config(upstream_addr)).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK, "re-run error is fail-closed");
    let raw = res.text().await.unwrap();
    for token in ["advisor_20260301", "advisor-tool-2026-03-01", "Input tag"] {
        assert!(
            !raw.contains(token),
            "{token} must NEVER reach the client from the advisor path"
        );
    }
    let body: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert!(
        body["content"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["content"]["error_code"] == "unavailable"),
        "the turn degrades to an unavailable block instead"
    );
}

/// B1 (first-attempt variant): an advisor-active turn whose FIRST executor
/// POST returns a beta-naming 400 still propagates the error, but the body
/// must be scrubbed so Claude Code cannot latch the feature off.
#[tokio::test]
async fn b1_first_executor_400_is_scrubbed_before_reaching_client() {
    const BETA_400: &str = r#"{"error":{"type":"invalid_request_error","message":"Input tag 'advisor_20260301' found; advisor-tool-2026-03-01 beta required"}}"#;
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![FakeResponse {
            status: StatusCode::BAD_REQUEST,
            body: Some(BETA_400.into()),
        }],
    );
    let proxy_addr = start_proxy(advisor_config(upstream_addr)).await;
    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let raw = res.text().await.unwrap();
    for token in ["advisor_20260301", "advisor-tool-2026-03-01", "Input tag"] {
        assert!(!raw.contains(token), "{token} leaked on the first error");
    }
}

/// B2: a 200 with `status:"failed"`/`"incomplete"` and an `output_text`
/// must degrade to `unavailable`, and its text must NOT be injected into the
/// executor prompt or billed as an `advisor_message`.
#[tokio::test]
async fn b2_failed_advisor_status_is_not_accepted_as_advice() {
    for status in ["failed", "incomplete"] {
        let (upstream_addr, upstream) = start_fake_upstream().await;
        script(
            &upstream,
            vec![
                advisor_call_response("resp_call", "call_b2"),
                // The unusable response is retried once; the retry is bad too.
                advisor_answer_with_status(status, "FAKE ADVICE FROM A FAILED RESPONSE"),
                advisor_answer_with_status(status, "FAKE ADVICE FROM A FAILED RESPONSE"),
                advisor_executor_message("resp_final", "Answer without advice."),
            ],
        );
        let proxy_addr = start_proxy(advisor_config(upstream_addr)).await;
        let res = reqwest::Client::new()
            .post(format!("http://{proxy_addr}/v1/messages"))
            .header("content-type", "application/json")
            .body(ReqBody::from(advisor_request(false)))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "status={status}");
        let body: serde_json::Value = res.json().await.unwrap();
        let content = body["content"].as_array().unwrap();
        let result = content
            .iter()
            .find(|b| b["type"] == "advisor_tool_result")
            .unwrap();
        assert_eq!(result["content"]["error_code"], "unavailable", "{status}");
        assert!(result["content"]["text"].is_null(), "{status}");
        assert!(
            content
                .iter()
                .any(|b| b["text"] == "Answer without advice."),
            "{status}: the executor still answers"
        );
        let iterations = body["usage"]["iterations"].as_array().unwrap();
        assert!(
            iterations.iter().all(|i| i["type"] == "message"),
            "{status}: a failed review must not be billed"
        );

        // The failed text was NOT injected into the executor's transcript.
        let sent = upstream.received_all.lock().await.clone();
        assert_eq!(
            sent.len(),
            4,
            "{status}: the advisor and ONE retry, no more"
        );
        let final_body: serde_json::Value = serde_json::from_slice(&sent[3]).unwrap();
        let injected = final_body["input"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["type"] == "function_call_output")
            .unwrap()["output"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(
            !injected.contains("FAKE ADVICE"),
            "{status}: failed advice leaked into the executor prompt"
        );
        assert!(
            injected.contains("returned no advice"),
            "{status}: {injected}"
        );
    }
}

/// B3: at `max_uses`, the pending advisor call gets a FunctionCallOutput,
/// the executor is re-run ONCE for a real answer, and no orphan advisor
/// `function_call` reaches the upstream. The total POST count is bounded.
#[tokio::test]
async fn b3_cap_injects_output_reruns_once_and_bounds_the_loop() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            // max_uses = 1: the first advisor call IS consulted…
            advisor_call_response("resp_first", "call_first"),
            advisor_answer("Advice one."),
            // …the second one hits the cap and gets a FunctionCallOutput…
            advisor_call_response("resp_cap", "call_cap"),
            // …and the ONE terminal re-run STILL asks for advice, but also
            // gives a real text answer. No further consultation and no
            // further POST is allowed.
            advisor_call_with_text(
                "resp_rerun_asks_again",
                "call_cap_again",
                "Terminal answer despite another advisor request.",
            ),
            advisor_executor_message("resp_never", "should not be reached"),
        ],
    );
    let mut config = (*advisor_config(upstream_addr)).clone();
    config.advisor.max_uses = Some(1);
    let proxy_addr = start_proxy(Arc::new(config)).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = res.json().await.unwrap();

    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(
        sent.len(),
        4,
        "executor + advisor + cap executor + ONE terminal re-run; no 5th POST"
    );
    let bodies: Vec<serde_json::Value> = sent
        .iter()
        .map(|b| serde_json::from_slice(b).unwrap())
        .collect();
    // The cap-triggering call is answered with a FunctionCallOutput on the
    // terminal re-run (and on no other request).
    let caps: Vec<&serde_json::Value> = bodies[3]["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["type"] == "function_call_output" && i["call_id"] == "call_cap")
        .collect();
    assert_eq!(caps.len(), 1, "exactly one injected cap output");
    assert!(
        caps[0]["output"]
            .as_str()
            .unwrap()
            .contains("limit reached")
    );
    for (i, b) in bodies.iter().enumerate() {
        for item in b["input"].as_array().unwrap() {
            if item["type"] == "function_call" && item["name"] == "advisor" {
                assert!(
                    b["input"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|o| o["type"] == "function_call_output"
                            && o["call_id"] == item["call_id"]),
                    "attempt {i}: orphan advisor function_call"
                );
            }
        }
    }
    let content = body["content"].as_array().unwrap();
    assert!(
        !content
            .iter()
            .any(|b| b["type"] == "tool_use" && b["name"] == "advisor"),
        "no orphan client tool_use named advisor"
    );
    assert!(
        content
            .iter()
            .any(|b| b["content"]["error_code"] == "max_uses_exceeded"),
        "the cap is reported to the client"
    );
    assert!(
        content.iter().all(|b| b["type"] != "pause_turn"),
        "never pause_turn"
    );
    assert!(
        content.iter().any(|b| b["type"] == "text"
            && b["text"] == "Terminal answer despite another advisor request."),
        "a text block from the terminal re-run is rendered"
    );
    assert_eq!(body["stop_reason"], "end_turn");
}

/// B3 (real-answer variant): the terminal re-run produces a plain text
/// answer. It must be rendered (not discarded) and no orphan call lingers.
#[tokio::test]
async fn b3_cap_terminal_rerun_renders_a_real_answer() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            advisor_call_response("resp_first", "call_first2"),
            advisor_answer("Advice one."),
            // Cap-triggering call: inject FunctionCallOutput, re-run ONCE.
            advisor_call_response("resp_cap", "call_cap2"),
            advisor_executor_message("resp_answer", "A real answer after the cap."),
        ],
    );
    let mut config = (*advisor_config(upstream_addr)).clone();
    config.advisor.max_uses = Some(1);
    let proxy_addr = start_proxy(Arc::new(config)).await;
    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = res.json().await.unwrap();
    let content = body["content"].as_array().unwrap();
    assert!(
        content
            .iter()
            .any(|b| b["text"] == "A real answer after the cap."),
        "the terminal re-run's answer must be rendered"
    );
    let results: Vec<&serde_json::Value> = content
        .iter()
        .filter(|b| b["type"] == "advisor_tool_result")
        .collect();
    assert_eq!(results.len(), 2, "one successful pair + one cap result");
    assert_eq!(results[0]["content"]["type"], "advisor_result");
    assert_eq!(results[1]["content"]["error_code"], "max_uses_exceeded");
    assert_eq!(
        upstream.received_all.lock().await.len(),
        4,
        "no POST beyond the bound"
    );
}

/// HIGH regression: the terminal cap re-run asked for `advisor` again. Its
/// content must be rendered EXACTLY ONCE: no second `intermediate_blocks`
/// splice, no THIRD `server_tool_use` / `max_uses_exceeded` pair, and no POST
/// after the bound. With `max_uses = 1` the exact sequence is the consulted
/// pair, the (already emitted) cap pair, then the terminal text. The advisor-
/// only `function_call` is stripped from the final response, and the previous
/// cap call still has NO_MORE_ADVICE in the upstream transcript.
#[tokio::test]
async fn cap_terminal_rerun_with_another_advisor_call_renders_once() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            // max_uses = 1: first call is consulted, second hits the cap.
            advisor_call_response("resp_first", "call_first"),
            advisor_answer("Advice one."),
            advisor_call_response("resp_cap", "call_cap"),
            // Terminal re-run: real text AND another advisor call. This
            // response must render once, from final_resp only.
            advisor_call_with_text("resp_terminal", "call_terminal_again", "Terminal answer."),
            advisor_executor_message("resp_never", "must not be reached"),
        ],
    );
    let mut config = (*advisor_config(upstream_addr)).clone();
    config.advisor.max_uses = Some(1);
    let proxy_addr = start_proxy(Arc::new(config)).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = res.json().await.unwrap();
    let content = body["content"].as_array().unwrap();

    let types: Vec<&str> = content
        .iter()
        .map(|b| b["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        types,
        vec![
            "server_tool_use",
            "advisor_tool_result",
            "server_tool_use",
            "advisor_tool_result",
            "text",
        ],
        "exact block sequence: consulted pair + cap pair + ONE terminal text"
    );
    assert_eq!(content[4]["text"], "Terminal answer.");
    assert_eq!(
        content
            .iter()
            .filter(|b| b["type"] == "text" && b["text"] == "Terminal answer.")
            .count(),
        1,
        "terminal text appears exactly once"
    );
    assert_eq!(content[1]["content"]["type"], "advisor_result");
    assert_eq!(content[1]["content"]["text"], "Advice one.");
    assert_eq!(content[1]["tool_use_id"], content[0]["id"]);
    assert_eq!(content[3]["content"]["error_code"], "max_uses_exceeded");
    assert_eq!(content[3]["tool_use_id"], content[2]["id"]);
    assert_ne!(content[0]["id"], content[2]["id"]);
    assert!(
        !content[0]["id"].as_str().unwrap().is_empty()
            && !content[2]["id"].as_str().unwrap().is_empty(),
        "consultation ids are non-empty"
    );
    assert!(
        !content
            .iter()
            .any(|b| b["type"] == "tool_use" && b["name"] == "advisor"),
        "no orphan tool_use named advisor"
    );
    assert_eq!(
        content
            .iter()
            .filter(|b| b["content"]["error_code"] == "max_uses_exceeded")
            .count(),
        1,
        "exactly ONE cap result, never a third pair"
    );
    assert_eq!(body["stop_reason"], "end_turn");
    assert_eq!(
        body["usage"]["iterations"].as_array().unwrap().len(),
        4,
        "executor, advisor, cap executor, terminal executor"
    );

    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(sent.len(), 4, "no POST beyond the bound");
    let bodies: Vec<serde_json::Value> = sent
        .iter()
        .map(|b| serde_json::from_slice(b).unwrap())
        .collect();
    // NO_MORE_ADVICE answers the cap call on the terminal re-run.
    let cap_outputs: Vec<&serde_json::Value> = bodies[3]["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["type"] == "function_call_output" && i["call_id"] == "call_cap")
        .collect();
    assert_eq!(cap_outputs.len(), 1);
    assert!(
        cap_outputs[0]["output"]
            .as_str()
            .unwrap()
            .contains("limit reached")
    );
    // No orphan advisor function_call in ANY upstream input.
    for (i, b) in bodies.iter().enumerate() {
        for item in b["input"].as_array().unwrap() {
            if item["type"] == "function_call" && item["name"] == "advisor" {
                assert!(
                    b["input"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|o| o["type"] == "function_call_output"
                            && o["call_id"] == item["call_id"]),
                    "attempt {i}: orphan advisor function_call"
                );
            }
        }
    }
}

/// The consultation cap: the executor asks a SECOND time, the cap is
/// reached, and the turn still ends with exactly ONE cap pair and ONE
/// text block.
///
/// `max_uses: Some(1)` — NOT `Some(0)`: `0` means "no limit" everywhere in
/// the codebase (the same meaning as `None`), so the cap is forced by the
/// executor asking twice, not by a zero bound.
#[tokio::test]
async fn cap_terminal_rerun_emits_exactly_one_cap_pair_and_text() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            // 1. the executor asks for advice
            advisor_call_response("resp_call1", "call_first"),
            // 2. the advisor answers
            advisor_answer("Advice one."),
            // 3. the executor asks AGAIN — this hits the cap
            advisor_call_response("resp_call2", "call_cap"),
            // 4. the ONE terminal re-run finally answers
            advisor_executor_message("resp_terminal", "Terminal answer."),
            advisor_executor_message("resp_never", "must not be reached"),
        ],
    );
    let mut config = (*advisor_config(upstream_addr)).clone();
    config.advisor.max_uses = Some(1);
    let proxy_addr = start_proxy(Arc::new(config)).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = res.json().await.unwrap();
    let content = body["content"].as_array().unwrap();
    let types: Vec<&str> = content
        .iter()
        .map(|b| b["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        types,
        vec![
            "server_tool_use",
            "advisor_tool_result",
            "server_tool_use",
            "advisor_tool_result",
            "text",
        ],
        "exact block sequence: consulted pair + cap pair + ONE terminal text"
    );
    // EXACTLY ONE cap pair: one `max_uses_exceeded` result, closing the
    // `server_tool_use` that opened it.
    let caps: Vec<&serde_json::Value> = content
        .iter()
        .filter(|b| b["content"]["error_code"] == "max_uses_exceeded")
        .collect();
    assert_eq!(caps.len(), 1, "exactly one cap result");
    assert_eq!(caps[0]["tool_use_id"], content[2]["id"]);
    // EXACTLY ONE text block: the terminal answer.
    let texts: Vec<&serde_json::Value> = content.iter().filter(|b| b["type"] == "text").collect();
    assert_eq!(texts.len(), 1, "exactly one text block");
    assert_eq!(texts[0]["text"], "Terminal answer.");
    assert_eq!(body["stop_reason"], "end_turn");
    assert_eq!(
        upstream.received_all.lock().await.len(),
        4,
        "executor + advisor + cap executor + ONE terminal re-run; no 5th POST"
    );
}

/// B4: `timeout_secs > request_timeout` must NOT extend the operator's
/// timeout. A large advisor timeout is clamped to `request_timeout`, so a
/// stalled advisor is cut off quickly and the turn degrades.
#[tokio::test]
async fn b4_advisor_timeout_is_clamped_to_request_timeout() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            advisor_call_response("resp_call", "call_b4"),
            advisor_answer("never arrives"),
            advisor_answer("never arrives"),
            advisor_executor_message("resp_final", "Continued without advice."),
        ],
    );
    // The advisor stalls on both its attempt and its one retry.
    *upstream.delay_per_attempt.lock().await = VecDeque::from([
        Duration::ZERO,
        Duration::from_secs(3),
        Duration::from_secs(3),
        Duration::ZERO,
    ]);
    let mut config = (*advisor_config(upstream_addr)).clone();
    config.request_timeout = Duration::from_millis(300);
    config.advisor.timeout_secs = Some(5);
    let proxy_addr = start_proxy(Arc::new(config)).await;

    let started = std::time::Instant::now();
    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    let elapsed = started.elapsed();
    assert_eq!(res.status(), StatusCode::OK, "degrades, never errors");
    assert!(
        elapsed < Duration::from_millis(1500),
        "the 5s advisor timeout must be clamped to 300ms; turn took {elapsed:?}"
    );
    let body: serde_json::Value = res.json().await.unwrap();
    let result = body["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["type"] == "advisor_tool_result")
        .unwrap();
    assert_eq!(result["content"]["error_code"], "execution_time_exceeded");
    assert!(
        body["content"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["text"] == "Continued without advice."),
        "the executor re-run still answers"
    );
}

/// B5: a `default_model` fallback on the FINAL executor re-run must preserve
/// the injected advice (`function_call_output`) in the accumulator.
#[tokio::test]
async fn b5_mid_session_fallback_preserves_injected_advice() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            advisor_call_response("resp_call", "call_b5"),
            advisor_answer("Advice that must survive the fallback."),
            // The FINAL executor call first returns a model-rejection 404.
            FakeResponse {
                status: StatusCode::NOT_FOUND,
                body: Some(REAL_GATEWAY_MODEL_404.into()),
            },
            // Then succeeds via `default_model`.
            advisor_executor_message("resp_final", "Done after fallback."),
        ],
    );
    let mut config = (*advisor_config(upstream_addr)).clone();
    config.model_aliases = ModelAliases {
        map: BTreeMap::new(),
        default_model: Some("gpt-4o-mini".into()),
    };
    let proxy_addr = start_proxy(Arc::new(config)).await;
    let request = advisor_request(false).replace(
        "\"model\": \"gpt-4o-mini\"",
        "\"model\": \"claude-sonnet-5\"",
    );

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(request))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["stop_reason"], "end_turn");
    assert!(
        body["content"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["text"] == "Done after fallback.")
    );

    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(sent.len(), 4, "call, advisor, rejected final, fallback");
    let bodies: Vec<serde_json::Value> = sent
        .iter()
        .map(|b| serde_json::from_slice(b).unwrap())
        .collect();
    assert_eq!(bodies[2]["model"], "claude-sonnet-5");
    assert_eq!(bodies[3]["model"], "gpt-4o-mini", "fallback model");
    let items = bodies[3]["input"].as_array().unwrap();
    assert_eq!(
        items.len(),
        3,
        "original translation + injected function_call/output pair"
    );
    assert!(
        items.iter().any(|i| i["type"] == "function_call_output"
            && i["output"] == "Advice that must survive the fallback."),
        "the injected advice survives the re-translation"
    );
    let call = items.iter().find(|i| i["type"] == "function_call").unwrap();
    assert_eq!(call["call_id"], "call_b5");
    let output = items
        .iter()
        .find(|i| i["type"] == "function_call_output")
        .unwrap();
    assert_eq!(output["call_id"], "call_b5", "no orphan function_call");
}

/// B6: intermediate executor text and a sibling client `tool_use` must
/// survive to the final Anthropic message, with `stop_reason == tool_use`.
#[tokio::test]
async fn b6_intermediate_content_and_sibling_tool_use_survive() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            advisor_call_with_text_and_bash(
                "resp_first",
                "call_b6",
                "Let me consult the advisor first.",
            ),
            advisor_answer("Use a channel."),
            advisor_executor_message("resp_final", "Here is the implementation."),
        ],
    );
    let proxy_addr = start_proxy(advisor_config(upstream_addr)).await;
    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = res.json().await.unwrap();
    let content = body["content"].as_array().unwrap();

    assert!(
        content
            .iter()
            .any(|b| b["text"] == "Let me consult the advisor first."),
        "intermediate executor text survives"
    );
    assert!(
        content
            .iter()
            .any(|b| b["type"] == "tool_use" && b["name"] == "Bash"),
        "sibling client tool_use survives"
    );
    assert!(
        content
            .iter()
            .any(|b| b["text"] == "Here is the implementation."),
        "the final executor text survives"
    );
    // The pair remains contiguous and id-matched.
    let use_pos = content
        .iter()
        .position(|b| b["type"] == "server_tool_use")
        .unwrap();
    assert_eq!(content[use_pos + 1]["type"], "advisor_tool_result");
    assert_eq!(content[use_pos + 1]["tool_use_id"], content[use_pos]["id"]);
    assert_eq!(content[use_pos + 1]["content"]["text"], "Use a channel.");
    // Order: intermediate text/tool_use precede the consultation pair.
    assert!(
        content
            .iter()
            .position(|b| b["text"] == "Let me consult the advisor first.")
            .unwrap()
            < use_pos
    );
    assert_eq!(
        body["stop_reason"], "tool_use",
        "a client tool is still pending"
    );
}

/// B7: a terminal `incomplete` / `max_output_tokens` reason must survive
/// the response assembly (not be clobbered to `end_turn`); a normal `completed`
/// response still yields `end_turn`.
#[tokio::test]
async fn b7_stop_reason_is_preserved_for_incomplete_and_completed() {
    // (a) incomplete / max_output_tokens → max_tokens
    {
        let (upstream_addr, upstream) = start_fake_upstream().await;
        script(
            &upstream,
            vec![
                advisor_call_response("resp_call", "call_b7"),
                advisor_answer("Be concise."),
                executor_max_tokens_response("resp_incomplete"),
            ],
        );
        let proxy_addr = start_proxy(advisor_config(upstream_addr)).await;
        let res = reqwest::Client::new()
            .post(format!("http://{proxy_addr}/v1/messages"))
            .header("content-type", "application/json")
            .body(ReqBody::from(advisor_request(false)))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body: serde_json::Value = res.json().await.unwrap();
        assert_eq!(body["stop_reason"], "max_tokens");
        assert!(
            body["content"]
                .as_array()
                .unwrap()
                .iter()
                .any(|b| b["text"] == "Truncated.")
        );
    }
    // (b) a normal completed turn → end_turn
    {
        let (upstream_addr, upstream) = start_fake_upstream().await;
        script(
            &upstream,
            vec![
                advisor_call_response("resp_call", "call_b7b"),
                advisor_answer("Be concise."),
                advisor_executor_message("resp_done", "Done."),
            ],
        );
        let proxy_addr = start_proxy(advisor_config(upstream_addr)).await;
        let res = reqwest::Client::new()
            .post(format!("http://{proxy_addr}/v1/messages"))
            .header("content-type", "application/json")
            .body(ReqBody::from(advisor_request(false)))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body: serde_json::Value = res.json().await.unwrap();
        assert_eq!(body["stop_reason"], "end_turn");
    }
}

/// B8: an ordinary function tool named `advisor` (WITH an input_schema) must
/// NOT activate the advisor pipeline — one POST, normal streaming preserved.
#[tokio::test]
async fn b8_ordinary_function_tool_named_advisor_does_not_activate() {
    const ORDINARY_ADVISOR_TOOL: &str = r#"{"name":"advisor","description":"an ordinary client tool","input_schema":{"type":"object","properties":{}}}"#;
    let (upstream_addr, upstream) = start_fake_upstream().await;
    let sse = [
        r#"event: response.created
data: {"type":"response.created","response":{"id":"resp_1","object":"response","created_at":0,"status":"in_progress","model":"gpt-4o","output":[]}}"#,
        r#"event: response.output_text.delta
data: {"type":"response.output_text.delta","item_id":"msg_x","output_index":0,"content_index":0,"delta":"Hi"}"#,
        r#"event: response.completed
data: {"type":"response.completed","response":{"id":"resp_1","object":"response","created_at":0,"status":"completed","model":"gpt-4o","output":[],"usage":{"input_tokens":4,"output_tokens":2,"total_tokens":6}}}"#,
    ]
    .join("\n\n")
        + "\n\n";
    *upstream.canned_stream.lock().await = Some(sse);
    // Advisor config ENABLED: the discriminator must be the declaration
    // shape, not the config flag.
    let proxy_addr = start_proxy(advisor_config(upstream_addr)).await;
    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request_with_decl(
            true,
            ORDINARY_ADVISOR_TOOL,
        )))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers()
            .get(axum::http::header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap(),
        "text/event-stream",
        "the REAL translated SSE stream, not the synthetic advisor sequence"
    );
    let text = res.text().await.unwrap();
    let kinds: Vec<&str> = text
        .split("\n\n")
        .filter(|s| !s.trim().is_empty())
        .filter_map(|e| e.lines().find_map(|l| l.strip_prefix("event: ")))
        .collect();
    assert_eq!(kinds.iter().filter(|k| **k == "message_start").count(), 1);
    assert_eq!(kinds.iter().filter(|k| **k == "message_stop").count(), 1);

    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(sent.len(), 1, "NO extra advisor POST");
    let v: serde_json::Value = serde_json::from_slice(&sent[0]).unwrap();
    assert_eq!(v["stream"], true, "normal streaming is preserved");
    let tools = v["tools"].as_array().unwrap();
    assert_eq!(
        tools.iter().filter(|t| t["name"] == "advisor").count(),
        1,
        "exactly one (the client's own) advisor function tool; no duplicate"
    );
    assert_eq!(tools[0]["parameters"]["type"], "object");
    // B9: inactive advisor → the orphan section is removed.
    assert!(
        !v["instructions"]
            .as_str()
            .unwrap()
            .contains("Call advisor BEFORE substantive work")
    );
}

/// B9: with `enabled:false` (the shipped default) and an inbound advisor
/// declaration, the orphan `# Advisor Tool` instructions must NOT reach the
/// executor; with `enabled:true` they MUST be kept.
#[tokio::test]
async fn b9_disabled_and_declared_strips_orphan_advisor_instructions() {
    for (label, enabled, expect_instructions) in
        [("disabled", false, false), ("enabled", true, true)]
    {
        let (upstream_addr, upstream) = start_fake_upstream().await;
        *upstream.canned.lock().await = Some(completed_response_with_id("resp_b9", "ok"));
        let mut config = (*advisor_config(upstream_addr)).clone();
        config.advisor.enabled = enabled;
        let proxy_addr = start_proxy(Arc::new(config)).await;
        let res = reqwest::Client::new()
            .post(format!("http://{proxy_addr}/v1/messages"))
            .header("content-type", "application/json")
            .body(ReqBody::from(advisor_request(false)))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{label}");
        let sent = upstream.received_all.lock().await.clone();
        let v: serde_json::Value = serde_json::from_slice(&sent[0]).unwrap();
        let instructions = v["instructions"].as_str().unwrap();
        assert_eq!(
            instructions.contains("Call advisor BEFORE substantive work"),
            expect_instructions,
            "{label}: advisor instructions policy"
        );
        if expect_instructions {
            assert!(instructions.contains("# Advisor Tool"), "{label}");
        } else {
            assert!(!instructions.contains("# Advisor Tool"), "{label}");
            // Unrelated system sections must survive the strip.
            assert!(instructions.contains("<system-reminder>skills</system-reminder>"));
            assert!(instructions.contains("Git status: clean"));
        }
    }
}

/// B10: a zero-usage advisor-active turn must emit NO `iterations` field.
#[tokio::test]
async fn b10_zero_usage_turn_omits_iterations() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            advisor_call_without_usage("resp_call", "call_b10"),
            FakeResponse {
                status: StatusCode::OK,
                body: Some(
                    r#"{
                        "id": "resp_advice_nousage", "object": "response", "created_at": 1,
                        "status": "completed", "model": "beam",
                        "output": [{"type": "message", "role": "assistant",
                          "content": [{"type": "output_text", "text": "Be concise.", "annotations": []}]}]
                    }"#
                    .into(),
                ),
            },
            response_without_usage("resp_final", "Done."),
        ],
    );
    let proxy_addr = start_proxy(advisor_config(upstream_addr)).await;
    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let raw = res.text().await.unwrap();
    assert!(
        !raw.contains("\"iterations\""),
        "empty iterations must be None, not an empty array: {raw}"
    );
    let body: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert!(body["usage"]["iterations"].is_null());
    assert!(
        body["content"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["text"] == "Done.")
    );
}

/// B12: the `advisor_message` iteration carries the explicit
/// `cache_creation: null` field Claude Code sees on the wire (§3.5).
#[tokio::test]
async fn b12_advisor_iteration_has_cache_creation_null() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            advisor_call_response("resp_call", "call_b12"),
            advisor_answer("Be concise."),
            advisor_executor_message("resp_final", "Done."),
        ],
    );
    let proxy_addr = start_proxy(advisor_config(upstream_addr)).await;
    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let raw = res.text().await.unwrap();
    assert!(raw.contains("\"cache_creation\":null"), "{raw}");
    let body: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let iterations = body["usage"]["iterations"].as_array().unwrap();
    let advisor = iterations
        .iter()
        .find(|i| i["type"] == "advisor_message")
        .unwrap();
    assert!(advisor["cache_creation"].is_null());
    assert_eq!(advisor["cache_creation_input_tokens"], 0);
}

/// HARD GATE: `enabled:false` with NO advisor declaration is byte-for-byte
/// identical to the pre-advisor (`17f8329`) behaviour for ordinary
/// non-streaming requests. The B9 post-translation strip is a no-op here
/// (translate.rs already removed the section when no advisor tool exists).
#[tokio::test]
async fn b9_disabled_without_declaration_is_still_byte_identical() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned.lock().await = Some(completed_response_with_id("resp_gate", "same answer"));
    let config = make_proxy_config(upstream_addr);
    assert!(!config.advisor.enabled);
    let proxy_addr = start_proxy(config).await;

    let request = advisor_request_with_decl(
        false,
        r#"{"name":"Bash","description":"run","input_schema":{"type":"object","properties":{}}}"#,
    );
    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(request))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.json::<serde_json::Value>().await.unwrap(),
        serde_json::json!({
            "id": "msg_resp_gate",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "same answer"}],
            "model": "gpt-4o-mini",
            "stop_reason": "end_turn",
            "stop_sequence": null,
            "usage": {"input_tokens": 7, "output_tokens": 3}
        })
    );
    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(sent.len(), 1, "no extra POST");
    let v: serde_json::Value = serde_json::from_slice(&sent[0]).unwrap();
    assert_eq!(v["stream"], false);
    assert!(v["tools"].as_array().unwrap().len() == 1);
    assert_eq!(v["tools"][0]["name"], "Bash");
    assert!(
        !v["instructions"]
            .as_str()
            .unwrap()
            .contains("Call advisor BEFORE substantive work"),
        "the pre-existing strip already removed the section"
    );
    assert!(
        v["instructions"]
            .as_str()
            .unwrap()
            .contains("<system-reminder>skills</system-reminder>")
    );
}

// ─── Two-section instruction-strip regressions (byte identity + B9) ─────────
//
// `17f8329` stripped only the FIRST line-start `# Advisor Tool` section.
// A post-translation strip must NEVER run on the ordinary no-declaration
// path: that is what deleted a SECOND unrelated section in `ddac5de`.

/// Two `# Advisor Tool` sections, separated by `<system-reminder>`.
const TWO_ADVISOR_SECTIONS: &str = "Git status: clean\n\n\
# Advisor Tool\n\nCall advisor ONE.\n\n\
<system-reminder>skills</system-reminder>\n\n\
# Advisor Tool\n\nCall advisor TWO.\n\n\
<system-reminder>agents</system-reminder>";

fn two_section_request(stream: bool, declaration: &str) -> String {
    let declaration: serde_json::Value = serde_json::from_str(declaration).unwrap();
    serde_json::json!({
        "model": "gpt-4o-mini",
        "max_tokens": 64,
        "stream": stream,
        "system": TWO_ADVISOR_SECTIONS,
        "tools": [declaration],
        "messages": [{"role": "user", "content": "Do the task"}],
    })
    .to_string()
}

/// `enabled:false` + NO declaration: the translator strips section ONE once
/// and the proxy does NOT strip again. Section TWO survives, exactly as it
/// did in `17f8329` (byte identity), for both non-streaming and streaming.
#[tokio::test]
async fn two_sections_disabled_no_declaration_preserves_second_section() {
    const BASH_DECL: &str =
        r#"{"name":"Bash","description":"run","input_schema":{"type":"object","properties":{}}}"#;
    for stream in [false, true] {
        let (upstream_addr, upstream) = start_fake_upstream().await;
        if stream {
            let created = serde_json::json!({
                "type": "response.created",
                "response": {"id": "resp_1", "object": "response",
                    "created_at": 0, "status": "in_progress",
                    "model": "gpt-4o", "output": []}
            })
            .to_string();
            let completed = serde_json::json!({
                "type": "response.completed",
                "response": {"id": "resp_1", "object": "response",
                    "created_at": 0, "status": "completed",
                    "model": "gpt-4o", "output": [],
                    "usage": {"input_tokens": 4, "output_tokens": 2, "total_tokens": 6}}
            })
            .to_string();
            *upstream.canned_stream.lock().await = Some(format!(
                "event: response.created\ndata: {created}\n\n\
                 event: response.completed\ndata: {completed}\n\n"
            ));
        } else {
            *upstream.canned.lock().await =
                Some(completed_response_with_id("resp_two_sec", "same answer"));
        }
        let config = make_proxy_config(upstream_addr);
        assert!(!config.advisor.enabled);
        let proxy_addr = start_proxy(config).await;
        let res = reqwest::Client::new()
            .post(format!("http://{proxy_addr}/v1/messages"))
            .header("content-type", "application/json")
            .body(ReqBody::from(two_section_request(stream, BASH_DECL)))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "stream={stream}");
        if stream {
            assert_eq!(
                res.headers()
                    .get(axum::http::header::CONTENT_TYPE)
                    .unwrap()
                    .to_str()
                    .unwrap(),
                "text/event-stream"
            );
            let _ = res.text().await.unwrap();
        } else {
            assert_eq!(
                res.json::<serde_json::Value>().await.unwrap(),
                serde_json::json!({
                    "id": "msg_resp_two_sec",
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "text", "text": "same answer"}],
                    "model": "gpt-4o-mini",
                    "stop_reason": "end_turn",
                    "stop_sequence": null,
                    "usage": {"input_tokens": 7, "output_tokens": 3}
                })
            );
        }
        let sent = upstream.received_all.lock().await.clone();
        assert_eq!(sent.len(), 1, "stream={stream}: no extra POST");
        let v: serde_json::Value = serde_json::from_slice(&sent[0]).unwrap();
        let instructions = v["instructions"].as_str().unwrap();
        assert!(
            !instructions.contains("Call advisor ONE"),
            "stream={stream}: section ONE was stripped once"
        );
        assert!(
            instructions.contains("# Advisor Tool\n\nCall advisor TWO."),
            "stream={stream}: section TWO survives, byte-identical to 17f8329: {instructions:?}"
        );
        assert!(instructions.starts_with("Git status: clean\n\n<system-reminder>skills"));
        assert!(instructions.contains("<system-reminder>agents</system-reminder>"));
        assert_eq!(v["stream"], stream);
        assert_eq!(v["tools"][0]["name"], "Bash");
    }
}

/// `enabled:false` + schema-less declaration (B9): the translator skips its
/// strip, so `build_outbound` applies ONE strip. Section ONE is removed; the
/// second section is defined to survive (matching the single-strip contract).
#[tokio::test]
async fn two_sections_disabled_schema_less_decl_strips_only_first() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned.lock().await = Some(completed_response_with_id("resp_b9_two", "ok"));
    let config = make_proxy_config(upstream_addr);
    assert!(!config.advisor.enabled);
    let proxy_addr = start_proxy(config).await;
    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(two_section_request(
            false,
            ADVISOR_DECLARATION,
        )))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(sent.len(), 1, "B9: no extra POST");
    let v: serde_json::Value = serde_json::from_slice(&sent[0]).unwrap();
    let instructions = v["instructions"].as_str().unwrap();
    assert!(
        !instructions.contains("Call advisor ONE"),
        "no orphan tool instructions from section ONE: {instructions:?}"
    );
    assert!(
        instructions.contains("Call advisor TWO"),
        "exactly one strip; section TWO survives"
    );
    assert!(instructions.contains("<system-reminder>skills</system-reminder>"));
    assert!(instructions.contains("<system-reminder>agents</system-reminder>"));
    assert!(
        !v["tools"]
            .as_array()
            .unwrap_or(&Vec::new())
            .iter()
            .any(|t| t["name"] == "advisor"),
        "schema-less declaration is dropped upstream"
    );
}

/// `enabled:true` + real declaration: the advisor pipeline is active, so
/// BOTH sections are preserved for the synthesized tool.
#[tokio::test]
async fn two_sections_enabled_declaration_preserves_both_sections() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned.lock().await = Some(completed_response_with_id("resp_active", "Done."));
    let proxy_addr = start_proxy(advisor_config(upstream_addr)).await;
    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(two_section_request(
            false,
            ADVISOR_DECLARATION,
        )))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(sent.len(), 1, "no consultation without an advisor call");
    let v: serde_json::Value = serde_json::from_slice(&sent[0]).unwrap();
    let instructions = v["instructions"].as_str().unwrap();
    assert_eq!(
        instructions, TWO_ADVISOR_SECTIONS,
        "active advisor keeps both sections byte-for-byte"
    );
    assert_eq!(
        v["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|t| t["name"] == "advisor")
            .count(),
        1,
        "exactly one synthesized advisor tool"
    );
}

/// Anthropic request whose single user message carries a tool_result with
/// an `image` block — the shape Claude Code emits when a tool returns a
/// screenshot (e.g. a browser/computer-use tool).
fn request_with_image_tool_result() -> String {
    r#"{
        "model":"claude-sonnet-5",
        "max_tokens":16,
        "stream":false,
        "messages":[
            {"role":"user","content":[
                {"type":"tool_result","tool_use_id":"toolu_01","content":[
                    {"type":"text","text":"screenshot attached"},
                    {"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgo="}}
                ]}
            ]}
        ]
    }"#
    .to_string()
}

const CANNED_RESPONSE: &str = r#"{
    "id":"resp_img",
    "object":"response",
    "created_at":1,
    "status":"completed",
    "model":"any",
    "output":[{"type":"message","id":"m1","status":"completed","role":"assistant","content":[{"type":"output_text","text":"ok","annotations":[]}]}],
    "usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}
}"#;

async fn setup_with_images(
    mode: Option<ToolResultImages>,
) -> (SocketAddr, SocketAddr, FakeUpstream) {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned.lock().await = Some(CANNED_RESPONSE.to_string());
    let base = make_proxy_config(upstream_addr);
    let config = match mode {
        Some(m) => {
            let mut c = (*base).clone();
            c.tool_result_images = m;
            Arc::new(c)
        }
        None => base,
    };
    let proxy_addr = start_proxy(config).await;
    (proxy_addr, upstream_addr, upstream)
}

#[tokio::test]
async fn tool_result_image_reaches_upstream_as_array_form() {
    let (proxy_addr, _upstream_addr, upstream) = setup_with_images(None).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(request_with_image_tool_result())
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        *upstream.request_count.lock().await,
        1,
        "proxy must not retry"
    );

    let sent = upstream
        .received
        .lock()
        .await
        .clone()
        .expect("upstream was not called");
    let body: serde_json::Value = serde_json::from_slice(&sent).unwrap();
    let items = body["input"].as_array().expect("input is an array");
    let fco = items
        .iter()
        .find(|i| i["type"] == "function_call_output")
        .expect("expected a function_call_output item");
    // Default mode: the output is the array-of-parts form with an
    // input_image part carrying the data URI.
    let output = fco["output"].as_array().expect("output must be an array");
    let parts: Vec<&str> = output.iter().map(|p| p["type"].as_str().unwrap()).collect();
    assert_eq!(parts, ["input_text", "input_image"]);
    assert_eq!(output[0]["text"], "screenshot attached");
    assert_eq!(output[1]["image_url"], "data:image/png;base64,iVBORw0KGgo=");
}

#[tokio::test]
async fn tool_result_image_lift_mode_sends_follow_up_user_message() {
    let (proxy_addr, _upstream_addr, upstream) =
        setup_with_images(Some(ToolResultImages::Lift)).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(request_with_image_tool_result())
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        *upstream.request_count.lock().await,
        1,
        "proxy must not retry"
    );

    let sent = upstream
        .received
        .lock()
        .await
        .clone()
        .expect("upstream was not called");
    let body: serde_json::Value = serde_json::from_slice(&sent).unwrap();
    let items = body["input"].as_array().unwrap();
    let fco_pos = items
        .iter()
        .position(|i| i["type"] == "function_call_output")
        .expect("expected a function_call_output item");
    assert_eq!(items[fco_pos]["output"], "screenshot attached");
    // The image must be forwarded in a user message immediately after the
    // tool output.
    let lifted = &items[fco_pos + 1];
    assert_eq!(lifted["type"], "message");
    assert_eq!(lifted["role"], "user");
    // First part labels which tool call the image came from.
    assert_eq!(lifted["content"][0]["type"], "input_text");
    assert_eq!(lifted["content"][1]["type"], "input_image");
    assert_eq!(
        lifted["content"][1]["image_url"],
        "data:image/png;base64,iVBORw0KGgo="
    );
}

#[tokio::test]
async fn tool_result_image_drop_mode_keeps_text_only() {
    let (proxy_addr, _upstream_addr, upstream) =
        setup_with_images(Some(ToolResultImages::Drop)).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(request_with_image_tool_result())
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        *upstream.request_count.lock().await,
        1,
        "proxy must not retry"
    );

    let sent = upstream
        .received
        .lock()
        .await
        .clone()
        .expect("upstream was not called");
    let body: serde_json::Value = serde_json::from_slice(&sent).unwrap();
    let text = String::from_utf8_lossy(&sent);
    assert!(
        !text.contains("input_image"),
        "drop mode must not emit any image part: {text}"
    );
    let items = body["input"].as_array().unwrap();
    let fco = items
        .iter()
        .find(|i| i["type"] == "function_call_output")
        .unwrap();
    assert_eq!(fco["output"], "screenshot attached");
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

#[tokio::test]
async fn large_tool_result_image_is_accepted() {
    let (proxy_addr, _upstream_addr, upstream) = setup_with_images(None).await;

    // ~6 MB of base64 (~4.5 MB of binary image data).
    let big_b64 = "A".repeat(6 * 1024 * 1024);
    let body = format!(
        r#"{{"model":"claude-sonnet-5","max_tokens":16,"stream":false,"messages":[
            {{"role":"user","content":[
                {{"type":"tool_result","tool_use_id":"toolu_big","content":[
                    {{"type":"text","text":"screenshot attached"}},
                    {{"type":"image","source":{{"type":"base64","media_type":"image/png","data":"{big_b64}"}}}}
                ]}}
            ]}}
        ]}}"#
    );

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let sent = upstream
        .received
        .lock()
        .await
        .clone()
        .expect("upstream was not called");
    let text = String::from_utf8_lossy(&sent);
    assert!(
        text.contains("\"input_image\""),
        "image part must reach the upstream"
    );
}

// ─── Server-side `web_fetch` ───────────────────────────────────────────────
//
// The Anthropic server-side `web_fetch` tool: the client declares it in
// `tools[]` with a versioned `type` and NO `input_schema`, and the SERVER
// does the HTTP fetch. The proxy is that server.
//
// The fetch itself cannot be pointed at a loopback test server — the SSRF
// guard correctly refuses private addresses, because the URL comes from a
// model. These tests therefore cover the PROXY side of the contract (tool
// synthesis, call interception, error degradation, wire shape); the fetch
// layer itself is covered by `src/tools/web_fetch/fetch/live_tests.rs`, which uses
// the test-only permissive address policy.

/// The Anthropic server-side `web_fetch` declaration: a name, a versioned
/// `type`, and NO `input_schema` (that is what makes it a server tool).
const WEB_FETCH_DECLARATION: &str =
    r#"{"type":"web_fetch_20250910","name":"web_fetch","max_uses":10}"#;

/// A request that declares the server-side `web_fetch` tool.
fn web_fetch_request(stream: bool) -> String {
    format!(
        r#"{{
            "model": "gpt-4o-mini",
            "max_tokens": 64,
            "stream": {stream},
            "tools": [{WEB_FETCH_DECLARATION}],
            "messages": [{{"role": "user", "content": "Summarize the page"}}]
        }}"#
    )
}

/// A Responses body whose output is one `function_call` for `web_fetch`.
fn web_fetch_call_response(url: &str) -> String {
    serde_json::json!({
        "id": "resp_fetch",
        "object": "response",
        "created_at": 1,
        "status": "completed",
        "model": "gpt-4o-mini",
        "output": [{
            "type": "function_call",
            "id": "fc_1",
            "call_id": "call_1",
            "name": "web_fetch",
            "arguments": serde_json::json!({ "url": url }).to_string(),
        }],
        "usage": {"input_tokens": 10, "output_tokens": 5, "total_tokens": 15},
    })
    .to_string()
}

/// A proxy config with `web_fetch` enabled.
fn web_fetch_config(upstream: SocketAddr) -> Arc<Config> {
    let mut config = (*make_proxy_config(upstream)).clone();
    config.web_fetch = WebFetchConfig {
        enabled: true,
        timeout_secs: Some(2),
        max_bytes: None,
        max_content_bytes: None,
    };
    Arc::new(config)
}

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

// ─── Proxy-native `web_search` ─────────────────────────────────────────────
//
// With `web_search.enabled`, the client's server-side `web_search` tool is
// answered by the PROXY (DuckDuckGo scraping + page fetching) instead of the
// upstream's built-in search. A mock "DuckDuckGo" serves the results here; the
// result pages themselves point at a loopback address, which the SSRF guard
// refuses, so these tests cover the failed-page path. Page fetching, parsing
// and compaction are covered by the unit tests in `src/tools/web_search/`.

/// The Anthropic server-side `web_search` declaration (no `input_schema`).
const WEB_SEARCH_DECLARATION: &str =
    r#"{"type":"web_search_20250305","name":"web_search","max_uses":8}"#;

fn web_search_request(stream: bool) -> String {
    format!(
        r#"{{
            "model": "gpt-4o-mini",
            "max_tokens": 64,
            "stream": {stream},
            "tools": [{WEB_SEARCH_DECLARATION}],
            "messages": [{{"role": "user", "content": "What is new in Rust?"}}]
        }}"#
    )
}

/// A Responses body whose output is one `proxy_web_search` function call.
fn web_search_call_response(queries: &[&str]) -> String {
    serde_json::json!({
        "id": "resp_search",
        "object": "response",
        "created_at": 1,
        "status": "completed",
        "model": "gpt-4o-mini",
        "output": [{
            "type": "function_call",
            "id": "fc_1",
            "call_id": "call_s1",
            "name": "proxy_web_search",
            "arguments": serde_json::json!({ "queries": queries }).to_string(),
        }],
        "usage": {"input_tokens": 10, "output_tokens": 5, "total_tokens": 15},
    })
    .to_string()
}

/// A mock DuckDuckGo: records every form body and answers with `html`.
async fn start_fake_search_engine(html: &'static str) -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let app = Router::new()
        .route(
            "/html/",
            post(
                move |State(seen): State<Arc<Mutex<Vec<String>>>>, body: Bytes| async move {
                    seen.lock()
                        .await
                        .push(String::from_utf8_lossy(&body).into_owned());
                    Response::builder()
                        .status(StatusCode::OK)
                        .header("content-type", "text/html")
                        .body(Body::from(html))
                        .unwrap()
                },
            ),
        )
        .with_state(seen.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (addr, seen)
}

const FAKE_RESULTS_HTML: &str = r#"<div id="links">
  <div class="result web-result"><h2><a href="//duckduckgo.com/l/?uddg=http%3A%2F%2F127.0.0.1%3A9%2Fone">First hit</a></h2>
    <a class="result__snippet">first snippet</a></div>
  <div class="result web-result"><h2><a href="//duckduckgo.com/l/?uddg=http%3A%2F%2F127.0.0.1%3A9%2Ftwo">Second hit</a></h2>
    <a class="result__snippet">second snippet</a></div>
</div>"#;

const FAKE_CAPTCHA_HTML: &str = r#"<form id="challenge-form"></form>"#;

fn web_search_config(upstream: SocketAddr, engine: SocketAddr) -> Arc<Config> {
    let mut config = (*make_proxy_config(upstream)).clone();
    config.web_search = WebSearchConfig {
        enabled: true,
        search_endpoint: format!("http://{engine}/html/"),
        page_timeout_secs: 2,
        search_timeout_secs: 2,
        ..WebSearchConfig::default()
    };
    Arc::new(config)
}

async fn post_messages(proxy: SocketAddr, body: String) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("http://{proxy}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(body))
        .send()
        .await
        .unwrap()
}

/// Disabled (the default): the upstream's built-in search is used, exactly as
/// before — no function tool, no forced buffering.
#[tokio::test]
async fn web_search_disabled_keeps_the_upstreams_builtin_search() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned.lock().await = Some(completed_response_with_id("resp_same", "same answer"));
    let proxy_addr = start_proxy(make_proxy_config(upstream_addr)).await;

    let res = post_messages(proxy_addr, web_search_request(false)).await;
    assert_eq!(res.status(), StatusCode::OK);

    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(sent.len(), 1);
    let received: serde_json::Value = serde_json::from_slice(&sent[0]).unwrap();
    let tools = received["tools"].as_array().expect("tools");
    assert!(tools.iter().any(|t| t["type"] == "web_search"));
    assert!(!tools.iter().any(|t| t["name"] == "proxy_web_search"));
}

/// Enabled: the built-in search is replaced by the function tool and the
/// request is buffered.
#[tokio::test]
async fn web_search_enabled_replaces_the_builtin_tool_and_buffers() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned.lock().await = Some(completed_response_with_id("resp_done", "done"));
    let (engine, _) = start_fake_search_engine(FAKE_RESULTS_HTML).await;
    let proxy_addr = start_proxy(web_search_config(upstream_addr, engine)).await;

    let res = post_messages(proxy_addr, web_search_request(true)).await;
    assert_eq!(res.status(), StatusCode::OK);

    let sent = upstream.received_all.lock().await.clone();
    let received: serde_json::Value = serde_json::from_slice(&sent[0]).unwrap();
    assert_eq!(received["stream"], true);
    let tools = received["tools"].as_array().expect("tools");
    assert!(!tools.iter().any(|t| t["type"] == "web_search"));
    let tool = tools
        .iter()
        .find(|t| t["name"] == "proxy_web_search")
        .expect("the proxy_web_search function tool must be synthesized");
    assert_eq!(tool["parameters"]["required"][0], "queries");
}

/// The whole loop: two queries are searched, each yields a
/// `server_tool_use`/`web_search_tool_result` pair for the client, the
/// executor is re-run with the formatted results (the unreachable pages are
/// reported, not fatal), and the answer is delivered.
#[tokio::test]
async fn web_search_runs_each_query_and_feeds_the_results_back() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned_per_attempt.lock().await = VecDeque::from([
        FakeResponse {
            status: StatusCode::OK,
            body: Some(web_search_call_response(&["rust news", "rust 2026"])),
        },
        FakeResponse {
            status: StatusCode::OK,
            body: Some(completed_response_with_id("resp_after", "Rust is great.")),
        },
    ]);
    let (engine, searched) = start_fake_search_engine(FAKE_RESULTS_HTML).await;
    let proxy_addr = start_proxy(web_search_config(upstream_addr, engine)).await;

    let res = post_messages(proxy_addr, web_search_request(false)).await;
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = res.json().await.unwrap();
    let content = body["content"].as_array().expect("content");

    let uses: Vec<_> = content
        .iter()
        .filter(|b| b["type"] == "server_tool_use")
        .collect();
    let results: Vec<_> = content
        .iter()
        .filter(|b| b["type"] == "web_search_tool_result")
        .collect();
    assert_eq!(uses.len(), 2, "one pair per query: {content:?}");
    assert_eq!(results.len(), 2);
    assert_eq!(uses[0]["name"], "web_search");
    assert_eq!(uses[0]["input"]["query"], "rust news");
    assert_eq!(uses[1]["input"]["query"], "rust 2026");
    assert_eq!(results[0]["tool_use_id"], uses[0]["id"]);
    assert_eq!(results[0]["content"][0]["title"], "First hit");
    assert_eq!(results[0]["content"][0]["url"], "http://127.0.0.1:9/one");
    assert_eq!(body["usage"]["server_tool_use"]["web_search_requests"], 2);

    assert!(
        !content
            .iter()
            .any(|b| b["type"] == "tool_use" && b["name"] == "proxy_web_search"),
        "the internal call must not surface as a client tool_use"
    );
    assert!(
        content
            .iter()
            .any(|b| b["type"] == "text" && b["text"] == "Rust is great.")
    );

    // The search engine saw both queries, as form posts.
    let forms = searched.lock().await.clone();
    assert_eq!(forms.len(), 2);
    assert!(forms.iter().any(|f| f.contains("q=rust+news")));

    // The re-run executor was handed the results, with the failed pages named.
    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(sent.len(), 2);
    let second = String::from_utf8_lossy(&sent[1]).into_owned();
    assert!(second.contains("function_call_output"));
    assert!(second.contains("web_search_results"));
    assert!(second.contains("failed to fetch page"));
    assert!(second.contains("http://127.0.0.1:9/one"));
}

/// More queries than `max_queries` are not run.
#[tokio::test]
async fn web_search_caps_the_number_of_queries() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned_per_attempt.lock().await = VecDeque::from([
        FakeResponse {
            status: StatusCode::OK,
            body: Some(web_search_call_response(&["a", "b", "c", "d", "e"])),
        },
        FakeResponse {
            status: StatusCode::OK,
            body: Some(completed_response_with_id("resp_after", "ok")),
        },
    ]);
    let (engine, searched) = start_fake_search_engine(FAKE_RESULTS_HTML).await;
    let mut config = (*web_search_config(upstream_addr, engine)).clone();
    config.web_search.max_queries = 2;
    let proxy_addr = start_proxy(Arc::new(config)).await;

    let res = post_messages(proxy_addr, web_search_request(false)).await;
    assert_eq!(res.status(), StatusCode::OK);

    assert_eq!(searched.lock().await.len(), 2);
    let sent = upstream.received_all.lock().await.clone();
    let second = String::from_utf8_lossy(&sent[1]).into_owned();
    assert!(second.contains("3 further queries were NOT run"));
}

/// A CAPTCHA from the search engine degrades to an error result; the turn
/// still completes with HTTP 200.
#[tokio::test]
async fn web_search_captcha_degrades_to_an_error_result() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned_per_attempt.lock().await = VecDeque::from([
        FakeResponse {
            status: StatusCode::OK,
            body: Some(web_search_call_response(&["blocked"])),
        },
        FakeResponse {
            status: StatusCode::OK,
            body: Some(completed_response_with_id(
                "resp_after",
                "I could not search.",
            )),
        },
    ]);
    let (engine, _) = start_fake_search_engine(FAKE_CAPTCHA_HTML).await;
    let proxy_addr = start_proxy(web_search_config(upstream_addr, engine)).await;

    let res = post_messages(proxy_addr, web_search_request(false)).await;
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = res.json().await.unwrap();
    let content = body["content"].as_array().expect("content");

    let result = content
        .iter()
        .find(|b| b["type"] == "web_search_tool_result")
        .expect("a result block");
    assert_eq!(result["content"]["type"], "web_search_tool_result_error");
    assert_eq!(result["content"]["error_code"], "unavailable");
    assert!(content.iter().any(|b| b["text"] == "I could not search."));

    let sent = upstream.received_all.lock().await.clone();
    let second = String::from_utf8_lossy(&sent[1]).into_owned();
    assert!(second.contains("search_error"));
    assert!(second.contains("CAPTCHA"));
    // The model is told how long to wait, and is not told to give up.
    assert!(second.contains("Wait about 10 minutes before searching again"));
}

/// A second blocked search tells the model how long ago the previous blocked
/// one was, so it can see how fast it is retrying.
#[tokio::test]
async fn web_search_repeat_block_reports_time_since_the_previous_one() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned_per_attempt.lock().await = VecDeque::from([
        FakeResponse {
            status: StatusCode::OK,
            body: Some(web_search_call_response(&["first", "second"])),
        },
        FakeResponse {
            status: StatusCode::OK,
            body: Some(completed_response_with_id("resp_after", "done")),
        },
    ]);
    let (engine, _) = start_fake_search_engine(FAKE_CAPTCHA_HTML).await;
    let proxy_addr = start_proxy(web_search_config(upstream_addr, engine)).await;

    let res = post_messages(proxy_addr, web_search_request(false)).await;
    assert_eq!(res.status(), StatusCode::OK);

    let sent = upstream.received_all.lock().await.clone();
    let second = String::from_utf8_lossy(&sent[1]).into_owned();
    assert_eq!(
        second.matches("bot challenge (CAPTCHA)").count(),
        2,
        "both searches are blocked: {second}"
    );
    assert!(
        second.contains("The previous blocked search was"),
        "the repeat must say how recent the last block was: {second}"
    );
    assert!(second.contains("ago, so searches"), "{second}");
}

/// A streaming client gets ONE synthetic SSE sequence containing the pair.
#[tokio::test]
async fn web_search_streaming_client_gets_a_synthetic_sse_sequence() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned_per_attempt.lock().await = VecDeque::from([
        FakeResponse {
            status: StatusCode::OK,
            body: Some(web_search_call_response(&["rust"])),
        },
        FakeResponse {
            status: StatusCode::OK,
            body: Some(completed_response_with_id("resp_after", "streamed answer")),
        },
    ]);
    let (engine, _) = start_fake_search_engine(FAKE_RESULTS_HTML).await;
    let proxy_addr = start_proxy(web_search_config(upstream_addr, engine)).await;

    let res = post_messages(proxy_addr, web_search_request(true)).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()["content-type"], "text/event-stream");
    let text = res.text().await.unwrap();

    assert_eq!(text.matches("event: message_start").count(), 1);
    assert_eq!(text.matches("event: message_stop").count(), 1);
    assert!(text.contains("\"type\":\"server_tool_use\""));
    assert!(text.contains("\"type\":\"web_search_tool_result\""));
    assert!(text.contains("streamed answer"));
    assert!(!text.contains("proxy_web_search"));
}

/// A served server-tool call shows up in the request log: the client only
/// ever sees a `server_tool_use` block, so the log is the one place the
/// call itself can be seen.
#[tokio::test]
async fn a_served_tool_call_is_logged() {
    use openai_to_anthropic_proxy::SessionStatsStore;
    use openai_to_anthropic_proxy::tui::LogKind;

    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned_per_attempt.lock().await = VecDeque::from([
        FakeResponse {
            status: StatusCode::OK,
            body: Some(web_search_call_response(&["rust"])),
        },
        FakeResponse {
            status: StatusCode::OK,
            body: Some(completed_response_with_id("resp_after", "the answer")),
        },
    ]);
    let (engine, _) = start_fake_search_engine(FAKE_RESULTS_HTML).await;
    let (output, log_rx) = openai_to_anthropic_proxy::OutputSink::tui();
    let proxy_addr = start_proxy_with_captured_stores(
        web_search_config(upstream_addr, engine),
        Arc::new(SessionStatsStore::new()),
        output,
    )
    .await;

    let res = post_messages(proxy_addr, web_search_request(false)).await;
    assert_eq!(res.status(), StatusCode::OK);

    let lines: Vec<_> = std::iter::from_fn(|| log_rx.try_recv().ok()).collect();
    let tool_rows: Vec<_> = lines.iter().filter(|l| l.kind == LogKind::Tool).collect();
    assert_eq!(tool_rows.len(), 1, "one row per served tool call");
    let row = &tool_rows[0].text;
    assert!(row.contains("web_search"), "{row}");
    assert!(row.contains("\"rust\""), "{row}");
    assert!(row.contains("|  ok  |"), "{row}");
    assert!(row.contains("results"), "{row}");
}

// ─── Shared server-tool loop: Anthropic-faithful behaviour ─────────────────
//
// These tests describe how the proxy must behave when several server-side
// tools (`web_search`, `web_fetch`, `advisor`) are declared in ONE request,
// and where the proxy must honour the options the client sends with a tool.
// They were written BEFORE the implementation (plan step 1) and are
// `#[ignore]`d with the step that makes each one pass; that step removes the
// attribute. Tests that need a config knob that does not exist yet (the
// `pause_turn` round limit) are added together with that knob.

/// A request declaring several server tools at once.
fn server_tools_request(stream: bool, declarations: &[&str]) -> String {
    format!(
        r#"{{
            "model": "gpt-4o-mini",
            "max_tokens": 64,
            "stream": {stream},
            "tools": [{}],
            "messages": [{{"role": "user", "content": "Research this"}}]
        }}"#,
        declarations.join(",")
    )
}

/// A Responses body whose output is the given function calls, in order:
/// `(tool name, call id, arguments)`.
fn function_calls_response(id: &str, calls: &[(&str, &str, serde_json::Value)]) -> String {
    let output: Vec<serde_json::Value> = calls
        .iter()
        .enumerate()
        .map(|(i, (name, call_id, args))| {
            serde_json::json!({
                "type": "function_call",
                "id": format!("fc_{i}"),
                "call_id": call_id,
                "name": name,
                "arguments": args.to_string(),
            })
        })
        .collect();
    serde_json::json!({
        "id": id,
        "object": "response",
        "created_at": 1,
        "status": "completed",
        "model": "gpt-4o-mini",
        "output": output,
        "usage": {"input_tokens": 10, "output_tokens": 5, "total_tokens": 15},
    })
    .to_string()
}

fn ok_response(body: String) -> FakeResponse {
    FakeResponse {
        status: StatusCode::OK,
        body: Some(body),
    }
}

/// `web_search`, `web_fetch` and the advisor all enabled.
fn all_server_tools_config(upstream: SocketAddr, engine: SocketAddr) -> Arc<Config> {
    let mut config = (*web_search_config(upstream, engine)).clone();
    config.web_fetch = WebFetchConfig {
        enabled: true,
        timeout_secs: Some(2),
        max_bytes: None,
        max_content_bytes: None,
    };
    config.advisor = AdvisorConfig {
        enabled: true,
        model: "beam".into(),
        effort: None,
        max_uses: Some(2),
        max_tokens: Some(64),
        reasoning_max_tokens: Some(100),
        timeout_secs: Some(2),
    };
    Arc::new(config)
}

/// The names of the function tools the upstream was offered.
fn offered_tool_names(request: &[u8]) -> Vec<String> {
    let value: serde_json::Value = serde_json::from_slice(request).unwrap();
    value["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|t| t["name"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn content_of(body: &serde_json::Value) -> &Vec<serde_json::Value> {
    body["content"].as_array().expect("content array")
}

fn blocks_of_type<'a>(content: &'a [serde_json::Value], kind: &str) -> Vec<&'a serde_json::Value> {
    content.iter().filter(|b| b["type"] == kind).collect()
}

const ADVISOR_DECL: &str = r#"{"type":"advisor_20260301","name":"advisor","model":"beam"}"#;
const FETCH_DECL: &str = r#"{"type":"web_fetch_20250910","name":"web_fetch"}"#;
const SEARCH_DECL: &str = r#"{"type":"web_search_20250305","name":"web_search"}"#;

// ── Step 2: the tool options the client sends are read ──

/// With the proxy's own search OFF, the client's `allowed_domains` and
/// `user_location` must still reach the upstream's built-in search.
#[tokio::test]
async fn client_search_options_reach_the_upstreams_native_search() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned.lock().await = Some(completed_response_with_id("resp_native", "ok"));
    let proxy_addr = start_proxy(make_proxy_config(upstream_addr)).await;

    let decl = r#"{"type":"web_search_20250305","name":"web_search","max_uses":3,
        "allowed_domains":["rust-lang.org"],
        "user_location":{"type":"approximate","country":"GB","city":"London"}}"#;
    let res = post_messages(proxy_addr, server_tools_request(false, &[decl])).await;
    assert_eq!(res.status(), StatusCode::OK);

    let sent = upstream.received_all.lock().await.clone();
    let received: serde_json::Value = serde_json::from_slice(&sent[0]).unwrap();
    let tool = received["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["type"] == "web_search")
        .expect("the native search tool");
    // OpenAI's only domain control is an allow-list under `filters`.
    assert_eq!(tool["filters"]["allowed_domains"][0], "rust-lang.org");
    assert_eq!(tool["user_location"]["country"], "GB");
}

/// Anthropic rejects a search tool that sets both domain lists.
#[tokio::test]
async fn allowed_and_blocked_domains_together_is_a_400() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned.lock().await = Some(completed_response_with_id("resp_x", "ok"));
    let proxy_addr = start_proxy(make_proxy_config(upstream_addr)).await;

    let decl = r#"{"type":"web_search_20250305","name":"web_search",
        "allowed_domains":["a.example"],"blocked_domains":["b.example"]}"#;
    let res = post_messages(proxy_addr, server_tools_request(false, &[decl])).await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    assert!(
        upstream.received_all.lock().await.is_empty(),
        "an invalid request must not reach the upstream"
    );
}

// ── Steps 3-6: several server tools in one turn ──

/// With all three tools declared and enabled, the executor is offered all
/// three (today `web_search` hides the other two).
#[tokio::test]
async fn all_three_server_tools_are_offered_together() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned.lock().await = Some(completed_response_with_id("resp_done", "done"));
    let (engine, _) = start_fake_search_engine(FAKE_RESULTS_HTML).await;
    let proxy_addr = start_proxy(all_server_tools_config(upstream_addr, engine)).await;

    let decls = [SEARCH_DECL, FETCH_DECL, ADVISOR_DECL];
    let res = post_messages(proxy_addr, server_tools_request(false, &decls)).await;
    assert_eq!(res.status(), StatusCode::OK);

    let sent = upstream.received_all.lock().await.clone();
    let names = offered_tool_names(&sent[0]);
    for expected in ["proxy_web_search", "web_fetch", "advisor"] {
        assert!(names.iter().any(|n| n == expected), "{expected}: {names:?}");
    }
}

/// A turn can search, then fetch a page, then answer — the model is never
/// refused a server tool because another one was used first.
#[tokio::test]
async fn a_turn_can_search_then_fetch_then_answer() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            ok_response(function_calls_response(
                "r1",
                &[(
                    "proxy_web_search",
                    "call_s",
                    serde_json::json!({"queries": ["rust"]}),
                )],
            )),
            ok_response(function_calls_response(
                "r2",
                &[(
                    "web_fetch",
                    "call_f",
                    serde_json::json!({"url": "http://127.0.0.1:9/page"}),
                )],
            )),
            ok_response(completed_response_with_id("r3", "All done.")),
        ],
    );
    let (engine, _) = start_fake_search_engine(FAKE_RESULTS_HTML).await;
    let proxy_addr = start_proxy(all_server_tools_config(upstream_addr, engine)).await;

    let res = post_messages(
        proxy_addr,
        server_tools_request(false, &[SEARCH_DECL, FETCH_DECL]),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = res.json().await.unwrap();
    let content = content_of(&body);

    assert_eq!(blocks_of_type(content, "web_search_tool_result").len(), 1);
    assert_eq!(blocks_of_type(content, "web_fetch_tool_result").len(), 1);
    // Search first, fetch second, then the answer — in the order they happened.
    let kinds: Vec<&str> = content.iter().filter_map(|b| b["type"].as_str()).collect();
    assert_eq!(
        kinds,
        [
            "server_tool_use",
            "web_search_tool_result",
            "server_tool_use",
            "web_fetch_tool_result",
            "text"
        ]
    );
    assert_eq!(body["stop_reason"], "end_turn");
    assert_eq!(upstream.received_all.lock().await.len(), 3);
}

/// Two different server tools called in ONE response are both served, and
/// both outputs reach the executor on the re-run.
#[tokio::test]
async fn two_server_tool_calls_in_one_response_are_both_served() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            ok_response(function_calls_response(
                "r1",
                &[
                    (
                        "proxy_web_search",
                        "call_s",
                        serde_json::json!({"queries": ["rust"]}),
                    ),
                    (
                        "web_fetch",
                        "call_f",
                        serde_json::json!({"url": "http://127.0.0.1:9/page"}),
                    ),
                ],
            )),
            ok_response(completed_response_with_id("r2", "Both done.")),
        ],
    );
    let (engine, _) = start_fake_search_engine(FAKE_RESULTS_HTML).await;
    let proxy_addr = start_proxy(all_server_tools_config(upstream_addr, engine)).await;

    let res = post_messages(
        proxy_addr,
        server_tools_request(false, &[SEARCH_DECL, FETCH_DECL]),
    )
    .await;
    let body: serde_json::Value = res.json().await.unwrap();
    let content = content_of(&body);
    assert_eq!(blocks_of_type(content, "web_search_tool_result").len(), 1);
    assert_eq!(blocks_of_type(content, "web_fetch_tool_result").len(), 1);

    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(sent.len(), 2, "one executor re-run, not one per call");
    let rerun: serde_json::Value = serde_json::from_slice(&sent[1]).unwrap();
    let outputs: Vec<&str> = rerun["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["type"] == "function_call_output")
        .filter_map(|i| i["call_id"].as_str())
        .collect();
    assert!(outputs.contains(&"call_s") && outputs.contains(&"call_f"));
}

/// `web_fetch` used to serve only the FIRST call of a response and silently
/// drop the rest; every call must be served.
#[tokio::test]
async fn web_fetch_serves_every_call_in_one_response() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            ok_response(function_calls_response(
                "r1",
                &[
                    (
                        "web_fetch",
                        "call_1",
                        serde_json::json!({"url": "http://127.0.0.1:9/a"}),
                    ),
                    (
                        "web_fetch",
                        "call_2",
                        serde_json::json!({"url": "http://127.0.0.1:9/b"}),
                    ),
                ],
            )),
            ok_response(completed_response_with_id("r2", "Fetched both.")),
        ],
    );
    let proxy_addr = start_proxy(web_fetch_config(upstream_addr)).await;

    let res = post_messages(proxy_addr, web_fetch_request(false)).await;
    let body: serde_json::Value = res.json().await.unwrap();
    let content = content_of(&body);
    assert_eq!(blocks_of_type(content, "server_tool_use").len(), 2);
    assert_eq!(blocks_of_type(content, "web_fetch_tool_result").len(), 2);
}

/// The advisor and a search used in the same turn.
#[tokio::test]
async fn advisor_and_web_search_work_in_the_same_turn() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            advisor_call_response("r1", "call_adv"),
            advisor_answer("Search before answering."),
            ok_response(function_calls_response(
                "r3",
                &[(
                    "proxy_web_search",
                    "call_s",
                    serde_json::json!({"queries": ["rust"]}),
                )],
            )),
            ok_response(completed_response_with_id("r4", "Final answer.")),
        ],
    );
    let (engine, _) = start_fake_search_engine(FAKE_RESULTS_HTML).await;
    let proxy_addr = start_proxy(all_server_tools_config(upstream_addr, engine)).await;

    let res = post_messages(
        proxy_addr,
        server_tools_request(false, &[SEARCH_DECL, ADVISOR_DECL]),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = res.json().await.unwrap();
    let content = content_of(&body);
    assert_eq!(blocks_of_type(content, "advisor_tool_result").len(), 1);
    assert_eq!(blocks_of_type(content, "web_search_tool_result").len(), 1);
    assert!(content.iter().any(|b| b["text"] == "Final answer."));
    // The advisor's tokens are still billed in iterations[].
    let iterations = body["usage"]["iterations"].as_array().unwrap();
    assert!(iterations.iter().any(|i| i["type"] == "advisor_message"));
}

/// A streaming client gets ONE well-formed sequence containing every pair.
#[tokio::test]
async fn a_mixed_turn_streams_one_sequence_with_every_pair() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            ok_response(function_calls_response(
                "r1",
                &[(
                    "proxy_web_search",
                    "call_s",
                    serde_json::json!({"queries": ["rust"]}),
                )],
            )),
            ok_response(function_calls_response(
                "r2",
                &[(
                    "web_fetch",
                    "call_f",
                    serde_json::json!({"url": "http://127.0.0.1:9/page"}),
                )],
            )),
            ok_response(completed_response_with_id("r3", "streamed final")),
        ],
    );
    let (engine, _) = start_fake_search_engine(FAKE_RESULTS_HTML).await;
    let proxy_addr = start_proxy(all_server_tools_config(upstream_addr, engine)).await;

    let res = post_messages(
        proxy_addr,
        server_tools_request(true, &[SEARCH_DECL, FETCH_DECL]),
    )
    .await;
    assert_eq!(res.headers()["content-type"], "text/event-stream");
    let text = res.text().await.unwrap();
    assert_eq!(text.matches("event: message_start").count(), 1);
    assert_eq!(text.matches("event: message_stop").count(), 1);
    assert!(text.contains("\"type\":\"web_search_tool_result\""));
    assert!(text.contains("\"type\":\"web_fetch_tool_result\""));
    assert!(text.contains("streamed final"));
    assert!(!text.contains("proxy_web_search"));
}

// ── Streamed executor runs ──

/// One Responses SSE stream: `events` are `(type, payload fields)`; the
/// `response.created` / `response.completed` envelope is added around them.
/// The terminal event's `output` is empty, as many upstreams send it.
fn sse_run(id: &str, events: &[(&str, serde_json::Value)], status: &str) -> String {
    let envelope = |status: &str| {
        serde_json::json!({
            "id": id, "object": "response", "created_at": 1, "status": status,
            "model": "gpt-4o-mini", "output": [],
            "usage": {"input_tokens": 10, "output_tokens": 5, "total_tokens": 15},
        })
    };
    let mut all = vec![(
        "response.created",
        serde_json::json!({"response": envelope("in_progress")}),
    )];
    all.extend(events.iter().cloned());
    if status != "none" {
        let terminal = if status == "completed" {
            "response.completed"
        } else {
            "response.failed"
        };
        all.push((terminal, serde_json::json!({"response": envelope(status)})));
    }
    all.iter()
        .map(|(kind, payload)| {
            let mut data = payload.clone();
            data["type"] = serde_json::json!(kind);
            format!("event: {kind}\ndata: {data}\n\n")
        })
        .collect()
}

/// The events of an assistant text message at `output_index`.
fn sse_text(output_index: u32, text: &str) -> Vec<(&'static str, serde_json::Value)> {
    let item = |content: serde_json::Value, status: &str| {
        serde_json::json!({
            "type": "message", "id": format!("msg_{output_index}"),
            "role": "assistant", "status": status, "content": content,
        })
    };
    vec![
        (
            "response.output_item.added",
            serde_json::json!({"output_index": output_index, "item": item(serde_json::json!([]), "in_progress")}),
        ),
        (
            "response.output_text.delta",
            serde_json::json!({
                "item_id": format!("msg_{output_index}"), "output_index": output_index,
                "content_index": 0, "delta": text,
            }),
        ),
        (
            "response.output_item.done",
            serde_json::json!({
                "output_index": output_index,
                "item": item(serde_json::json!([{"type": "output_text", "text": text, "annotations": []}]), "completed"),
            }),
        ),
    ]
}

/// The events of a function call at `output_index`.
fn sse_call(
    output_index: u32,
    name: &str,
    call_id: &str,
    args: &serde_json::Value,
) -> Vec<(&'static str, serde_json::Value)> {
    let args = args.to_string();
    let item = |arguments: &str| {
        serde_json::json!({
            "type": "function_call", "id": format!("fc_{output_index}"),
            "call_id": call_id, "name": name, "arguments": arguments,
        })
    };
    vec![
        (
            "response.output_item.added",
            serde_json::json!({"output_index": output_index, "item": item("")}),
        ),
        (
            "response.function_call_arguments.delta",
            serde_json::json!({
                "item_id": format!("fc_{output_index}"), "output_index": output_index, "delta": args,
            }),
        ),
        (
            "response.function_call_arguments.done",
            serde_json::json!({
                "item_id": format!("fc_{output_index}"), "output_index": output_index, "arguments": args,
            }),
        ),
        (
            "response.output_item.done",
            serde_json::json!({"output_index": output_index, "item": item(&args)}),
        ),
    ]
}

/// The Anthropic events of an SSE response body: `(type, payload)`.
fn anthropic_events(text: &str) -> Vec<(String, serde_json::Value)> {
    text.split("\n\n")
        .filter_map(|chunk| {
            let data = chunk.lines().find_map(|l| l.strip_prefix("data: "))?;
            let v: serde_json::Value = serde_json::from_str(data).unwrap();
            Some((v["type"].as_str().unwrap().to_owned(), v))
        })
        .collect()
}

/// Every content block is opened once, at the next index, and closed before
/// the message ends; returns the number of blocks.
fn assert_well_formed_blocks(events: &[(String, serde_json::Value)]) -> usize {
    let mut open = std::collections::BTreeSet::new();
    let mut next = 0u64;
    for (kind, v) in events {
        match kind.as_str() {
            "content_block_start" => {
                let index = v["index"].as_u64().unwrap();
                assert_eq!(index, next, "block indices are contiguous: {events:?}");
                open.insert(index);
                next += 1;
            }
            "content_block_delta" => {
                assert!(
                    open.contains(&v["index"].as_u64().unwrap()),
                    "delta on a closed block"
                );
            }
            "content_block_stop" => {
                assert!(
                    open.remove(&v["index"].as_u64().unwrap()),
                    "stop without start"
                );
            }
            "message_delta" | "message_stop" => {
                assert!(open.is_empty(), "blocks still open at {kind}: {open:?}");
            }
            _ => {}
        }
    }
    usize::try_from(next).unwrap()
}

/// The point of streamed runs: text the executor writes before a server-tool
/// call reaches the client as one message together with the served pair and
/// the next run's text, numbered continuously, and the internal call is
/// never shown.
#[tokio::test]
async fn streamed_runs_reach_the_client_as_one_message() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    let mut run1 = sse_text(0, "Let me look. ");
    run1.extend(sse_call(
        1,
        "proxy_web_search",
        "call_s",
        &serde_json::json!({"queries": ["rust"]}),
    ));
    script(
        &upstream,
        vec![
            ok_response(sse_run("r1", &run1, "completed")),
            ok_response(sse_run("r2", &sse_text(0, "Final answer."), "completed")),
        ],
    );
    let (engine, _) = start_fake_search_engine(FAKE_RESULTS_HTML).await;
    let proxy_addr = start_proxy(all_server_tools_config(upstream_addr, engine)).await;

    let res = post_messages(proxy_addr, server_tools_request(true, &[SEARCH_DECL])).await;
    assert_eq!(res.headers()["content-type"], "text/event-stream");
    let text = res.text().await.unwrap();
    let events = anthropic_events(&text);

    assert_eq!(
        events.iter().filter(|(k, _)| k == "message_start").count(),
        1
    );
    assert_eq!(
        events.iter().filter(|(k, _)| k == "message_stop").count(),
        1
    );
    // text, server_tool_use, web_search_tool_result, text
    assert_eq!(assert_well_formed_blocks(&events), 4, "{text}");
    assert!(
        !text.contains("proxy_web_search"),
        "the internal call leaked: {text}"
    );
    assert!(text.contains("Let me look. "));
    assert!(text.contains("Final answer."));
    assert!(
        text.find("Let me look. ").unwrap() < text.find("\"type\":\"server_tool_use\"").unwrap()
    );
    assert!(
        text.find("\"type\":\"web_search_tool_result\"").unwrap()
            < text.find("Final answer.").unwrap()
    );
    let delta = events
        .iter()
        .rev()
        .find(|(k, _)| k == "message_delta")
        .unwrap();
    assert_eq!(delta.1["delta"]["stop_reason"], "end_turn");
    // Both runs' usage is in the closing delta.
    assert_eq!(delta.1["usage"]["output_tokens"], 10);

    // Both executor runs were streamed upstream, and the second saw the
    // first's text and call with the served output.
    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(sent.len(), 2);
    for body in &sent {
        let v: serde_json::Value = serde_json::from_slice(body).unwrap();
        assert_eq!(v["stream"], true);
    }
    let rerun: serde_json::Value = serde_json::from_slice(&sent[1]).unwrap();
    let input = rerun["input"].as_array().unwrap();
    assert!(
        input.iter().any(|i| i["type"] == "function_call"
            && i["call_id"] == "call_s"
            && i["arguments"].as_str().is_some_and(|a| a.contains("rust"))),
        "the call's arguments were collected from the stream: {input:?}"
    );
    assert!(
        input
            .iter()
            .any(|i| i["type"] == "function_call_output" && i["call_id"] == "call_s")
    );
}

/// A client tool the executor calls alongside the loop's own is streamed to
/// the client as a `tool_use`, and decides the stop reason.
#[tokio::test]
async fn a_streamed_client_tool_call_ends_the_turn_with_tool_use() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    let mut run = sse_text(0, "Reading it.");
    run.extend(sse_call(
        1,
        "Read",
        "call_r",
        &serde_json::json!({"path": "a.txt"}),
    ));
    script(
        &upstream,
        vec![ok_response(sse_run("r1", &run, "completed"))],
    );
    let (engine, _) = start_fake_search_engine(FAKE_RESULTS_HTML).await;
    let proxy_addr = start_proxy(all_server_tools_config(upstream_addr, engine)).await;

    let res = post_messages(proxy_addr, server_tools_request(true, &[SEARCH_DECL])).await;
    let text = res.text().await.unwrap();
    let events = anthropic_events(&text);

    assert_eq!(assert_well_formed_blocks(&events), 2, "{text}");
    let starts: Vec<&str> = events
        .iter()
        .filter(|(k, _)| k == "content_block_start")
        .map(|(_, v)| v["content_block"]["type"].as_str().unwrap())
        .collect();
    assert_eq!(starts, vec!["text", "tool_use"]);
    assert!(text.contains("a.txt"));
    let delta = events
        .iter()
        .rev()
        .find(|(k, _)| k == "message_delta")
        .unwrap();
    assert_eq!(delta.1["delta"]["stop_reason"], "tool_use");
}

/// A later run that dies mid-stream must leave the client's stream
/// well-formed, and the work already done is returned.
#[tokio::test]
async fn a_streamed_run_that_fails_midway_leaves_balanced_blocks() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    let mut run1 = sse_text(0, "First. ");
    run1.extend(sse_call(
        1,
        "proxy_web_search",
        "call_s",
        &serde_json::json!({"queries": ["rust"]}),
    ));
    // Round 2: some text, then the stream just stops (no terminal event).
    let mut run2 = sse_text(0, "Partial");
    run2.truncate(2);
    script(
        &upstream,
        vec![
            ok_response(sse_run("r1", &run1, "completed")),
            ok_response(sse_run("r2", &run2, "none")),
        ],
    );
    let (engine, _) = start_fake_search_engine(FAKE_RESULTS_HTML).await;
    let proxy_addr = start_proxy(all_server_tools_config(upstream_addr, engine)).await;

    let res = post_messages(proxy_addr, server_tools_request(true, &[SEARCH_DECL])).await;
    let text = res.text().await.unwrap();
    let events = anthropic_events(&text);

    assert_well_formed_blocks(&events);
    assert_eq!(
        events.iter().filter(|(k, _)| k == "message_start").count(),
        1
    );
    assert_eq!(events.last().unwrap().0, "message_stop", "{text}");
    assert!(text.contains("Partial"), "what was streamed stays: {text}");
}

/// A `default_model` fallback rebuilds the request, and `prepare` with it,
/// which sets `stream: false` again. The loop must turn streaming back on for
/// the runs that follow.
#[tokio::test]
async fn a_default_model_fallback_does_not_stop_later_runs_streaming() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            FakeResponse {
                status: StatusCode::NOT_FOUND,
                body: Some(REAL_GATEWAY_MODEL_404.to_owned()),
            },
            // The fallback model's run is answered whole (what the rebuilt,
            // non-streaming request got), calling the search.
            ok_response(web_search_call_response(&["rust"])),
            ok_response(sse_run("r2", &sse_text(0, "Final answer."), "completed")),
        ],
    );
    let (engine, _) = start_fake_search_engine(FAKE_RESULTS_HTML).await;
    let base = all_server_tools_config(upstream_addr, engine);
    let mut config = (*base).clone();
    config.model_aliases = ModelAliases {
        map: BTreeMap::new(),
        default_model: Some("fallback-model".to_owned()),
    };
    let proxy_addr = start_proxy(Arc::new(config)).await;

    let res = post_messages(proxy_addr, server_tools_request(true, &[SEARCH_DECL])).await;
    let text = res.text().await.unwrap();
    let events = anthropic_events(&text);

    assert_well_formed_blocks(&events);
    assert_eq!(events.last().unwrap().0, "message_stop", "{text}");
    assert!(text.contains("Final answer."), "{text}");
    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(sent.len(), 3, "{text}");
    let streams: Vec<bool> = sent
        .iter()
        .map(|b| {
            serde_json::from_slice::<serde_json::Value>(b).unwrap()["stream"]
                .as_bool()
                .unwrap()
        })
        .collect();
    // The first request streamed and was refused; the fallback's request was
    // rebuilt non-streaming; the run after it streams again.
    assert_eq!(streams, vec![true, false, true]);
}

/// An upstream that answers a streamed request with plain JSON is still
/// served: the whole response is sent when it arrives.
#[tokio::test]
async fn a_json_answer_to_a_streamed_run_is_still_served() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![ok_response(completed_response_with_id("r1", "plain json"))],
    );
    let (engine, _) = start_fake_search_engine(FAKE_RESULTS_HTML).await;
    let proxy_addr = start_proxy(all_server_tools_config(upstream_addr, engine)).await;

    let res = post_messages(proxy_addr, server_tools_request(true, &[SEARCH_DECL])).await;
    let text = res.text().await.unwrap();
    let events = anthropic_events(&text);
    assert_eq!(assert_well_formed_blocks(&events), 1, "{text}");
    assert!(text.contains("plain json"));
}

/// A re-run that fails AFTER a server tool was served must not turn the
/// whole request into an error: the client gets what was already produced.
#[tokio::test]
async fn a_failed_later_run_returns_the_work_already_done() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            ok_response(function_calls_response(
                "r1",
                &[(
                    "proxy_web_search",
                    "call_s",
                    serde_json::json!({"queries": ["rust"]}),
                )],
            )),
            FakeResponse {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                body: Some(r#"{"error":{"message":"boom"}}"#.to_owned()),
            },
        ],
    );
    let (engine, _) = start_fake_search_engine(FAKE_RESULTS_HTML).await;
    let proxy_addr = start_proxy(web_search_config(upstream_addr, engine)).await;

    let res = post_messages(proxy_addr, web_search_request(false)).await;
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = res.json().await.unwrap();
    let content = content_of(&body);
    assert_eq!(blocks_of_type(content, "web_search_tool_result").len(), 1);
}

/// Anthropic reports `web_fetch_requests` next to `web_search_requests`.
#[tokio::test]
async fn usage_counts_web_fetch_requests() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            ok_response(web_fetch_call_response("http://127.0.0.1:9/page")),
            ok_response(completed_response_with_id("r2", "done")),
        ],
    );
    let proxy_addr = start_proxy(web_fetch_config(upstream_addr)).await;

    let res = post_messages(proxy_addr, web_fetch_request(false)).await;
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["usage"]["server_tool_use"]["web_fetch_requests"], 1);
}

// ── Step 7: the client's tool options are applied ──

/// The client's `max_uses` counts queries: the one over the limit gets a
/// `max_uses_exceeded` result and the turn carries on.
#[tokio::test]
async fn web_search_honours_the_clients_max_uses() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            ok_response(web_search_call_response(&["one", "two"])),
            ok_response(completed_response_with_id("r2", "ok")),
        ],
    );
    let (engine, searched) = start_fake_search_engine(FAKE_RESULTS_HTML).await;
    let proxy_addr = start_proxy(web_search_config(upstream_addr, engine)).await;

    let decl = r#"{"type":"web_search_20250305","name":"web_search","max_uses":1}"#;
    let res = post_messages(proxy_addr, server_tools_request(false, &[decl])).await;
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = res.json().await.unwrap();
    let results = blocks_of_type(content_of(&body), "web_search_tool_result");
    assert_eq!(results.len(), 2);
    assert!(results[0]["content"].is_array(), "the first query ran");
    assert_eq!(results[1]["content"]["error_code"], "max_uses_exceeded");
    assert_eq!(
        searched.lock().await.len(),
        1,
        "only one query was searched"
    );
}

/// `allowed_domains`: results from other hosts are dropped.
#[tokio::test]
async fn web_search_honours_allowed_domains() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            ok_response(web_search_call_response(&["rust"])),
            ok_response(completed_response_with_id("r2", "ok")),
        ],
    );
    // The mock engine only returns 127.0.0.1 results.
    let (engine, _) = start_fake_search_engine(FAKE_RESULTS_HTML).await;
    let proxy_addr = start_proxy(web_search_config(upstream_addr, engine)).await;

    let decl =
        r#"{"type":"web_search_20250305","name":"web_search","allowed_domains":["rust-lang.org"]}"#;
    let res = post_messages(proxy_addr, server_tools_request(false, &[decl])).await;
    let body: serde_json::Value = res.json().await.unwrap();
    let result = &blocks_of_type(content_of(&body), "web_search_tool_result")[0];
    assert_eq!(result["content"].as_array().map(Vec::len), Some(0));

    // The executor is told nothing matched, and no page was fetched.
    let sent = upstream.received_all.lock().await.clone();
    let rerun = String::from_utf8_lossy(&sent[1]).into_owned();
    assert!(!rerun.contains("http://127.0.0.1:9/one"));
}

/// `blocked_domains`: results from the blocked host are dropped.
#[tokio::test]
async fn web_search_honours_blocked_domains() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            ok_response(web_search_call_response(&["rust"])),
            ok_response(completed_response_with_id("r2", "ok")),
        ],
    );
    let (engine, _) = start_fake_search_engine(FAKE_RESULTS_HTML).await;
    let proxy_addr = start_proxy(web_search_config(upstream_addr, engine)).await;

    let decl =
        r#"{"type":"web_search_20250305","name":"web_search","blocked_domains":["127.0.0.1"]}"#;
    let res = post_messages(proxy_addr, server_tools_request(false, &[decl])).await;
    let body: serde_json::Value = res.json().await.unwrap();
    let result = &blocks_of_type(content_of(&body), "web_search_tool_result")[0];
    assert_eq!(result["content"].as_array().map(Vec::len), Some(0));
}

/// `user_location` picks the search engine's region.
#[tokio::test]
async fn web_search_sends_the_users_region_to_the_engine() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            ok_response(web_search_call_response(&["weather"])),
            ok_response(completed_response_with_id("r2", "ok")),
        ],
    );
    let (engine, searched) = start_fake_search_engine(FAKE_RESULTS_HTML).await;
    let proxy_addr = start_proxy(web_search_config(upstream_addr, engine)).await;

    let decl = r#"{"type":"web_search_20250305","name":"web_search",
        "user_location":{"type":"approximate","country":"GB"}}"#;
    let _ = post_messages(proxy_addr, server_tools_request(false, &[decl])).await;
    let forms = searched.lock().await.clone();
    // `wt-wt` is DuckDuckGo's "no region" default.
    assert!(
        forms[0].contains("kl=") && !forms[0].contains("kl=wt-wt"),
        "the user's region was not sent: {}",
        forms[0]
    );
}

/// `web_fetch` `allowed_domains`: a URL outside the list is refused BEFORE
/// any network access. `.invalid` never resolves, so without the rule the
/// error would be `url_not_accessible`.
#[tokio::test]
async fn web_fetch_honours_allowed_domains() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            ok_response(web_fetch_call_response("https://other.invalid/page")),
            ok_response(completed_response_with_id("r2", "ok")),
        ],
    );
    let proxy_addr = start_proxy(web_fetch_config(upstream_addr)).await;

    let decl = r#"{"type":"web_fetch_20250910","name":"web_fetch","allowed_domains":["docs.rs"]}"#;
    let res = post_messages(proxy_addr, server_tools_request(false, &[decl])).await;
    let body: serde_json::Value = res.json().await.unwrap();
    let result = &blocks_of_type(content_of(&body), "web_fetch_tool_result")[0];
    assert_eq!(result["content"]["error_code"], "url_not_allowed");
}

/// `web_fetch` `max_uses`: the fetch over the limit is refused.
#[tokio::test]
async fn web_fetch_honours_the_clients_max_uses() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            ok_response(function_calls_response(
                "r1",
                &[
                    (
                        "web_fetch",
                        "c1",
                        serde_json::json!({"url": "http://127.0.0.1:9/a"}),
                    ),
                    (
                        "web_fetch",
                        "c2",
                        serde_json::json!({"url": "http://127.0.0.1:9/b"}),
                    ),
                ],
            )),
            ok_response(completed_response_with_id("r2", "ok")),
        ],
    );
    let proxy_addr = start_proxy(web_fetch_config(upstream_addr)).await;

    let decl = r#"{"type":"web_fetch_20250910","name":"web_fetch","max_uses":1}"#;
    let res = post_messages(proxy_addr, server_tools_request(false, &[decl])).await;
    let body: serde_json::Value = res.json().await.unwrap();
    let results = blocks_of_type(content_of(&body), "web_fetch_tool_result");
    assert_eq!(results.len(), 2);
    assert_eq!(results[1]["content"]["error_code"], "max_uses_exceeded");
}

// ── Step 8: results stay in context on later turns ──

/// An earlier `web_search` result echoed back by the client (from the
/// upstream's native search, so the proxy did not make it) must reach the
/// model instead of being dropped.
#[tokio::test]
async fn an_echoed_native_search_result_reaches_the_model() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned.lock().await = Some(completed_response_with_id("r1", "ok"));
    let proxy_addr = start_proxy(make_proxy_config(upstream_addr)).await;

    let request = serde_json::json!({
        "model": "gpt-4o-mini",
        "max_tokens": 64,
        "tools": [serde_json::from_str::<serde_json::Value>(SEARCH_DECL).unwrap()],
        "messages": [
            {"role": "user", "content": "What is new in Rust?"},
            {"role": "assistant", "content": [
                {"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search",
                 "input": {"query": "rust news"}},
                {"type": "web_search_tool_result", "tool_use_id": "srvtoolu_1", "content": [
                    {"type": "web_search_result", "title": "Rust 2026 released",
                     "url": "https://blog.rust-lang.org/2026", "page_age": null,
                     "encrypted_content": "opaque-not-ours"}
                ]},
                {"type": "text", "text": "Rust 2026 shipped."}
            ]},
            {"role": "user", "content": "Tell me more"}
        ]
    });
    let res = post_messages(proxy_addr, request.to_string()).await;
    assert_eq!(res.status(), StatusCode::OK);

    let sent = upstream.received_all.lock().await.clone();
    let upstream_body = String::from_utf8_lossy(&sent[0]).into_owned();
    assert!(upstream_body.contains("Rust 2026 released"), "title lost");
    assert!(
        upstream_body.contains("https://blog.rust-lang.org/2026"),
        "url lost"
    );
}

/// The whole round trip through the proxy's own search: what the model saw
/// on turn one is back in its context on turn two, using only the blocks the
/// client was given.
#[tokio::test]
async fn search_results_survive_into_the_next_turn() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            ok_response(web_search_call_response(&["rust news"])),
            ok_response(completed_response_with_id("r2", "First answer.")),
            ok_response(completed_response_with_id("r3", "Second answer.")),
        ],
    );
    let (engine, _) = start_fake_search_engine(FAKE_RESULTS_HTML).await;
    let proxy_addr = start_proxy(web_search_config(upstream_addr, engine)).await;

    let first = post_messages(proxy_addr, web_search_request(false)).await;
    let first: serde_json::Value = first.json().await.unwrap();

    let second_request = serde_json::json!({
        "model": "gpt-4o-mini",
        "max_tokens": 64,
        "tools": [serde_json::from_str::<serde_json::Value>(SEARCH_DECL).unwrap()],
        "messages": [
            {"role": "user", "content": "What is new in Rust?"},
            {"role": "assistant", "content": first["content"].clone()},
            {"role": "user", "content": "Tell me more"}
        ]
    });
    let res = post_messages(proxy_addr, second_request.to_string()).await;
    assert_eq!(res.status(), StatusCode::OK);

    let sent = upstream.received_all.lock().await.clone();
    let last: serde_json::Value = serde_json::from_slice(sent.last().unwrap()).unwrap();
    let third = last["input"].to_string();
    assert!(
        third.contains("first snippet"),
        "turn-one results were lost"
    );
    assert!(
        !third.contains("proxy_web_search"),
        "the internal tool must not appear in the replayed history"
    );
}

// ── Step 10: blocks are streamed as they happen ──

/// The first bytes of a streaming reply must arrive while the turn is still
/// running, not after the last executor call.
#[tokio::test]
async fn a_streaming_reply_starts_before_the_turn_finishes() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            ok_response(web_search_call_response(&["rust"])),
            ok_response(completed_response_with_id("r2", "late answer")),
        ],
    );
    // Stall the SECOND executor call.
    *upstream.delay_per_attempt.lock().await =
        VecDeque::from([Duration::ZERO, Duration::from_millis(1500)]);
    let (engine, _) = start_fake_search_engine(FAKE_RESULTS_HTML).await;
    let proxy_addr = start_proxy(web_search_config(upstream_addr, engine)).await;

    let started = std::time::Instant::now();
    let mut res = post_messages(proxy_addr, web_search_request(true)).await;
    let first_chunk = res.chunk().await.unwrap().expect("a first chunk");
    assert!(
        started.elapsed() < Duration::from_millis(1000),
        "the stream only started after {:?}",
        started.elapsed()
    );
    assert!(String::from_utf8_lossy(&first_chunk).contains("message_start"));
}

/// The advisor's cap stops ONLY the advisor: once it is refused, the executor
/// can still search, and a further advisor call in the same response is
/// dropped rather than served or leaked to the client.
#[tokio::test]
async fn the_advisor_cap_does_not_stop_the_other_tools() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            advisor_call_response("r1", "call_a1"),
            advisor_answer("First advice."),
            advisor_call_response("r2", "call_a2"),
            advisor_answer("Second advice."),
            // The third call is over `max_uses: 2` and is refused.
            advisor_call_response("r3", "call_a3"),
            // The executor ignores the refusal AND asks to search.
            ok_response(function_calls_response(
                "r4",
                &[
                    ("advisor", "call_a4", serde_json::json!({})),
                    (
                        "proxy_web_search",
                        "call_s",
                        serde_json::json!({"queries": ["rust"]}),
                    ),
                ],
            )),
            ok_response(completed_response_with_id("r5", "Final answer.")),
        ],
    );
    let (engine, _) = start_fake_search_engine(FAKE_RESULTS_HTML).await;
    let proxy_addr = start_proxy(all_server_tools_config(upstream_addr, engine)).await;

    let res = post_messages(
        proxy_addr,
        server_tools_request(false, &[SEARCH_DECL, ADVISOR_DECL]),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = res.json().await.unwrap();
    let content = content_of(&body);

    let advisor_results = blocks_of_type(content, "advisor_tool_result");
    assert_eq!(advisor_results.len(), 3, "two answers and one refusal");
    assert_eq!(
        advisor_results[2]["content"]["error_code"],
        "max_uses_exceeded"
    );
    assert_eq!(blocks_of_type(content, "web_search_tool_result").len(), 1);
    assert!(content.iter().any(|b| b["text"] == "Final answer."));
    assert!(
        !content.iter().any(|b| b["type"] == "tool_use"),
        "no internal call may reach the client: {content:?}"
    );
    // r1, advice, r2, advice, r3, r4, r5.
    assert_eq!(upstream.received_all.lock().await.len(), 7);
}

/// The advisor's error scrubbing still applies when other tools are active:
/// an upstream 400 naming the advisor beta must never reach the client.
#[tokio::test]
async fn advisor_tokens_are_scrubbed_in_a_mixed_turn() {
    const BETA_400: &str = r#"{"error":{"type":"invalid_request_error","message":"Input tag 'advisor_20260301' found; advisor-tool-2026-03-01 beta required"}}"#;
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![FakeResponse {
            status: StatusCode::BAD_REQUEST,
            body: Some(BETA_400.into()),
        }],
    );
    let (engine, _) = start_fake_search_engine(FAKE_RESULTS_HTML).await;
    let proxy_addr = start_proxy(all_server_tools_config(upstream_addr, engine)).await;

    let res = post_messages(
        proxy_addr,
        server_tools_request(false, &[SEARCH_DECL, FETCH_DECL, ADVISOR_DECL]),
    )
    .await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let raw = res.text().await.unwrap();
    for token in ["advisor_20260301", "advisor-tool-2026-03-01", "Input tag"] {
        assert!(!raw.contains(token), "{token} leaked in a mixed turn");
    }
}

// ── The server-tool round limit and `pause_turn` ──

/// A search config with the loop's round limit set.
fn rounds_config(
    upstream: SocketAddr,
    engine: SocketAddr,
    max_rounds: usize,
    pause_turn: bool,
) -> Arc<Config> {
    let mut config = (*web_search_config(upstream, engine)).clone();
    config.server_tools = ServerToolsConfig {
        max_rounds: Some(max_rounds),
        pause_turn,
    };
    Arc::new(config)
}

fn search_round(id: &str, call_id: &str, query: &str) -> FakeResponse {
    ok_response(function_calls_response(
        id,
        &[(
            "proxy_web_search",
            call_id,
            serde_json::json!({"queries": [query]}),
        )],
    ))
}

/// At the limit (the default behaviour) the model is told to answer: the
/// extra search it asks for is refused, it gets one last run, and the client
/// sees an ordinary finished turn.
#[tokio::test]
async fn at_the_round_limit_the_model_is_made_to_answer() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            search_round("r1", "c1", "one"),
            search_round("r2", "c2", "two"),
            // Over the limit of 2: this call is refused, not run.
            search_round("r3", "c3", "three"),
            ok_response(completed_response_with_id("r4", "Answering now.")),
        ],
    );
    let (engine, searched) = start_fake_search_engine(FAKE_RESULTS_HTML).await;
    let proxy_addr = start_proxy(rounds_config(upstream_addr, engine, 2, false)).await;

    let res = post_messages(proxy_addr, web_search_request(false)).await;
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = res.json().await.unwrap();
    let content = content_of(&body);

    assert_eq!(blocks_of_type(content, "web_search_tool_result").len(), 2);
    assert!(content.iter().any(|b| b["text"] == "Answering now."));
    assert_eq!(body["stop_reason"], "end_turn");
    assert!(!content.iter().any(|b| b["type"] == "tool_use"));
    assert_eq!(searched.lock().await.len(), 2, "the third search never ran");

    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(sent.len(), 4);
    let last: serde_json::Value = serde_json::from_slice(&sent[3]).unwrap();
    assert!(
        last["input"]
            .to_string()
            .contains("limit for this request was reached"),
        "the model must be told why"
    );
}

/// A model that answers on its own before the limit is not affected at all.
#[tokio::test]
async fn a_turn_under_the_limit_is_untouched() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            search_round("r1", "c1", "one"),
            ok_response(completed_response_with_id("r2", "Done.")),
        ],
    );
    let (engine, _) = start_fake_search_engine(FAKE_RESULTS_HTML).await;
    let proxy_addr = start_proxy(rounds_config(upstream_addr, engine, 5, true)).await;

    let res = post_messages(proxy_addr, web_search_request(false)).await;
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["stop_reason"], "end_turn");
    assert_eq!(upstream.received_all.lock().await.len(), 2);
}

/// With `pause_turn` on, the limit returns the work so far with
/// `stop_reason: "pause_turn"`, and makes no further upstream call.
#[tokio::test]
async fn pause_turn_returns_the_work_so_far_at_the_limit() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            search_round("r1", "c1", "one"),
            search_round("r2", "c2", "two"),
            ok_response(completed_response_with_id("r3", "never reached")),
        ],
    );
    let (engine, _) = start_fake_search_engine(FAKE_RESULTS_HTML).await;
    let proxy_addr = start_proxy(rounds_config(upstream_addr, engine, 2, true)).await;

    let res = post_messages(proxy_addr, web_search_request(false)).await;
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = res.json().await.unwrap();
    let content = content_of(&body);

    assert_eq!(body["stop_reason"], "pause_turn");
    assert_eq!(blocks_of_type(content, "server_tool_use").len(), 2);
    assert_eq!(blocks_of_type(content, "web_search_tool_result").len(), 2);
    assert!(
        !content.iter().any(|b| b["type"] == "text"),
        "no answer yet"
    );
    assert_eq!(
        upstream.received_all.lock().await.len(),
        2,
        "no run is spent after the limit"
    );
}

/// The same, to a streaming client.
#[tokio::test]
async fn pause_turn_is_streamed_as_one_sequence() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            search_round("r1", "c1", "one"),
            search_round("r2", "c2", "two"),
        ],
    );
    let (engine, _) = start_fake_search_engine(FAKE_RESULTS_HTML).await;
    let proxy_addr = start_proxy(rounds_config(upstream_addr, engine, 2, true)).await;

    let res = post_messages(proxy_addr, web_search_request(true)).await;
    let text = res.text().await.unwrap();
    assert_eq!(text.matches("event: message_start").count(), 1);
    assert_eq!(text.matches("event: message_stop").count(), 1);
    assert!(text.contains("\"stop_reason\":\"pause_turn\""), "{text}");
}

/// A client tool the model asked for in the paused round still wins: the
/// client must run it, and sending its result resumes the turn.
#[tokio::test]
async fn a_pending_client_tool_beats_pause_turn() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![ok_response(function_calls_response(
            "r1",
            &[
                (
                    "proxy_web_search",
                    "c1",
                    serde_json::json!({"queries": ["one"]}),
                ),
                ("Bash", "c2", serde_json::json!({"command": "ls"})),
            ],
        ))],
    );
    let (engine, _) = start_fake_search_engine(FAKE_RESULTS_HTML).await;
    let proxy_addr = start_proxy(rounds_config(upstream_addr, engine, 1, true)).await;

    let bash = r#"{"name":"Bash","description":"run","input_schema":{"type":"object","properties":{"command":{"type":"string"}}}}"#;
    let res = post_messages(
        proxy_addr,
        server_tools_request(false, &[SEARCH_DECL, bash]),
    )
    .await;
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["stop_reason"], "tool_use");
    assert!(
        content_of(&body)
            .iter()
            .any(|b| b["type"] == "tool_use" && b["name"] == "Bash")
    );
}

/// Resuming: the client resends the paused assistant message as the last one.
/// The proxy accepts it and the model sees what was found.
#[tokio::test]
async fn a_paused_turn_can_be_resumed() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            search_round("r1", "c1", "one"),
            ok_response(completed_response_with_id("r2", "Resumed and answered.")),
        ],
    );
    let (engine, _) = start_fake_search_engine(FAKE_RESULTS_HTML).await;
    let proxy_addr = start_proxy(rounds_config(upstream_addr, engine, 1, true)).await;

    let first = post_messages(proxy_addr, web_search_request(false)).await;
    let first: serde_json::Value = first.json().await.unwrap();
    assert_eq!(first["stop_reason"], "pause_turn");

    let resume = serde_json::json!({
        "model": "gpt-4o-mini",
        "max_tokens": 64,
        "tools": [serde_json::from_str::<serde_json::Value>(SEARCH_DECL).unwrap()],
        "messages": [
            {"role": "user", "content": "What is new in Rust?"},
            {"role": "assistant", "content": first["content"].clone()},
        ]
    });
    let res = post_messages(proxy_addr, resume.to_string()).await;
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = res.json().await.unwrap();
    assert!(
        content_of(&body)
            .iter()
            .any(|b| b["text"] == "Resumed and answered.")
    );

    let sent = upstream.received_all.lock().await.clone();
    let resumed: serde_json::Value = serde_json::from_slice(sent.last().unwrap()).unwrap();
    assert!(
        resumed["input"].to_string().contains("first snippet"),
        "the model must still see what the search found"
    );
}

// ── Streaming as it happens ──

/// The search blocks reach a streaming client while the model is still working
/// on the answer, not together with it.
#[tokio::test]
async fn search_blocks_arrive_before_the_final_run_finishes() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            search_round("r1", "c1", "one"),
            ok_response(completed_response_with_id("r2", "late answer")),
        ],
    );
    *upstream.delay_per_attempt.lock().await =
        VecDeque::from([Duration::ZERO, Duration::from_millis(1500)]);
    let (engine, _) = start_fake_search_engine(FAKE_RESULTS_HTML).await;
    let proxy_addr = start_proxy(web_search_config(upstream_addr, engine)).await;

    let started = std::time::Instant::now();
    let mut res = post_messages(proxy_addr, web_search_request(true)).await;
    let mut seen = String::new();
    while !seen.contains("web_search_tool_result") {
        let chunk = res.chunk().await.unwrap().expect("stream ended early");
        seen.push_str(&String::from_utf8_lossy(&chunk));
    }
    assert!(
        started.elapsed() < Duration::from_millis(1000),
        "the result only arrived after {:?}",
        started.elapsed()
    );
    assert!(
        !seen.contains("late answer"),
        "the answer was not ready yet"
    );

    // The rest follows, in one well-formed message.
    seen.push_str(&res.text().await.unwrap());
    assert!(seen.contains("late answer"));
    assert_eq!(seen.matches("event: message_start").count(), 1);
    assert_eq!(seen.matches("event: message_stop").count(), 1);
}

/// Block indices run on across the pieces the stream is sent in.
#[tokio::test]
async fn streamed_blocks_have_contiguous_indices() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            search_round("r1", "c1", "one"),
            search_round("r2", "c2", "two"),
            ok_response(completed_response_with_id("r3", "answer")),
        ],
    );
    let (engine, _) = start_fake_search_engine(FAKE_RESULTS_HTML).await;
    let proxy_addr = start_proxy(web_search_config(upstream_addr, engine)).await;

    let text = post_messages(proxy_addr, web_search_request(true))
        .await
        .text()
        .await
        .unwrap();
    let starts: Vec<u64> = text
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|d| serde_json::from_str::<serde_json::Value>(d).ok())
        .filter(|v| v["type"] == "content_block_start")
        .map(|v| v["index"].as_u64().unwrap())
        .collect();
    // Two pairs and the answer.
    assert_eq!(starts, [0, 1, 2, 3, 4]);
}

/// A failure of the FIRST model run is still an HTTP error with its status
/// (which clients retry on), not a 200 with an error buried in the stream. The
/// advisor tokens are still scrubbed.
#[tokio::test]
async fn a_streaming_turn_whose_first_run_fails_is_an_http_error() {
    const BETA_400: &str = r#"{"error":{"type":"invalid_request_error","message":"Input tag 'advisor_20260301' found; advisor-tool-2026-03-01 beta required"}}"#;
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![FakeResponse {
            status: StatusCode::BAD_REQUEST,
            body: Some(BETA_400.into()),
        }],
    );
    let proxy_addr = start_proxy(advisor_config(upstream_addr)).await;

    let res = post_messages(proxy_addr, advisor_request(true)).await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let raw = res.text().await.unwrap();
    assert!(!raw.contains("advisor_20260301"), "{raw}");
}

/// A client that goes away stops the turn: the proxy does not keep paying for
/// upstream calls nobody will read.
#[tokio::test]
async fn a_client_that_disconnects_stops_the_turn() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            search_round("r1", "c1", "one"),
            search_round("r2", "c2", "two"),
            ok_response(completed_response_with_id("r3", "never wanted")),
        ],
    );
    // Round 2 is slow enough for the client to leave during it.
    *upstream.delay_per_attempt.lock().await =
        VecDeque::from([Duration::ZERO, Duration::from_millis(400), Duration::ZERO]);
    let (engine, _) = start_fake_search_engine(FAKE_RESULTS_HTML).await;
    let proxy_addr = start_proxy(web_search_config(upstream_addr, engine)).await;

    let mut res = post_messages(proxy_addr, web_search_request(true)).await;
    // Leave once the first search has been delivered: round 2 is in flight.
    let mut seen = String::new();
    while !seen.contains("web_search_tool_result") {
        let chunk = res.chunk().await.unwrap().expect("stream ended early");
        seen.push_str(&String::from_utf8_lossy(&chunk));
    }
    drop(res);

    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        upstream.received_all.lock().await.len(),
        2,
        "no third run for a client that left"
    );
}

/// Run one advisor turn against a scripted upstream with `max_uses` set;
/// returns the client's response body and every POST the upstream received.
async fn advisor_turn_with_max_uses(
    max_uses: u32,
    responses: Vec<FakeResponse>,
) -> (serde_json::Value, Vec<serde_json::Value>) {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(&upstream, responses);
    let mut config = (*advisor_config(upstream_addr)).clone();
    config.advisor.max_uses = Some(max_uses);
    let proxy_addr = start_proxy(Arc::new(config)).await;
    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = res.json().await.unwrap();
    let sent = upstream.received_all.lock().await.clone();
    let bodies = sent
        .iter()
        .map(|b| serde_json::from_slice(b).unwrap())
        .collect();
    (body, bodies)
}

fn advisor_results(body: &serde_json::Value) -> Vec<&serde_json::Value> {
    body["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|b| b["type"] == "advisor_tool_result")
        .collect()
}

fn upstream_failure(status: StatusCode) -> FakeResponse {
    FakeResponse {
        status,
        body: Some(r#"{"error":{"message":"advisor trouble"}}"#.into()),
    }
}

/// A transient failure (503) is retried once, and the retry's advice is used.
#[tokio::test]
async fn an_advisor_failure_that_a_retry_fixes_still_gives_advice() {
    let (body, sent) = advisor_turn_with_max_uses(
        2,
        vec![
            advisor_call_response("resp_call", "call_retry"),
            upstream_failure(StatusCode::SERVICE_UNAVAILABLE),
            advisor_answer("Use a channel."),
            advisor_executor_message("resp_final", "Done."),
        ],
    )
    .await;
    let results = advisor_results(&body);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["content"]["text"], "Use a channel.");
    assert_eq!(sent.len(), 4, "executor, advisor, its retry, executor");
    assert_eq!(sent[1]["model"], "beam");
    assert_eq!(sent[2]["model"], "beam");
}

/// A permanent failure (a 4xx) is NOT retried: it would fail the same way.
#[tokio::test]
async fn a_permanent_advisor_failure_is_not_retried() {
    let (body, sent) = advisor_turn_with_max_uses(
        2,
        vec![
            advisor_call_response("resp_call", "call_perm"),
            upstream_failure(StatusCode::UNPROCESSABLE_ENTITY),
            advisor_executor_message("resp_final", "Done."),
        ],
    )
    .await;
    assert_eq!(
        advisor_results(&body)[0]["content"]["error_code"],
        "unavailable"
    );
    assert_eq!(sent.len(), 3, "executor, ONE advisor POST, executor");
}

/// A consultation that failed does not use up `max_uses`: with a limit of 1,
/// a failed first consultation still leaves the second one available.
#[tokio::test]
async fn a_failed_consultation_does_not_use_up_max_uses() {
    let (body, _) = advisor_turn_with_max_uses(
        1,
        vec![
            advisor_call_response("resp_c1", "call_f1"),
            upstream_failure(StatusCode::UNPROCESSABLE_ENTITY),
            advisor_call_response("resp_c2", "call_f2"),
            advisor_answer("Second time lucky."),
            advisor_executor_message("resp_final", "Done."),
        ],
    )
    .await;
    let results = advisor_results(&body);
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["content"]["error_code"], "unavailable");
    assert_eq!(results[1]["content"]["text"], "Second time lucky.");
    assert!(
        results
            .iter()
            .all(|r| r["content"]["error_code"] != "max_uses_exceeded"),
        "the failure was not counted against max_uses"
    );
}

/// Failures are not free: after two failed consultations the advisor is
/// treated as down for the rest of the turn. The executor is told so, and a
/// further call to it is dropped without another advisor POST.
#[tokio::test]
async fn a_broken_advisor_is_given_up_on_after_two_failures() {
    let (body, sent) = advisor_turn_with_max_uses(
        5,
        vec![
            advisor_call_response("resp_c1", "call_g1"),
            upstream_failure(StatusCode::UNPROCESSABLE_ENTITY),
            advisor_call_response("resp_c2", "call_g2"),
            upstream_failure(StatusCode::UNPROCESSABLE_ENTITY),
            advisor_executor_message("resp_final", "Answer without advice."),
        ],
    )
    .await;
    assert_eq!(advisor_results(&body).len(), 2, "two failed consultations");
    assert!(
        body["content"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["text"] == "Answer without advice."),
        "the turn ends with the executor's answer"
    );
    // The executor's last run was told the advisor is gone for good.
    let last = sent.last().unwrap().to_string();
    assert!(last.contains("will not be consulted again"), "{last}");
    // A first failure was told only that it is temporary.
    assert!(sent[2].to_string().contains("request to it was refused"));
}

/// An executor that ignores that and calls the advisor a third time gets no
/// third advisor POST.
#[tokio::test]
async fn a_third_call_to_a_given_up_advisor_is_dropped() {
    let (_, sent) = advisor_turn_with_max_uses(
        5,
        vec![
            advisor_call_response("resp_c1", "call_h1"),
            upstream_failure(StatusCode::UNPROCESSABLE_ENTITY),
            advisor_call_response("resp_c2", "call_h2"),
            upstream_failure(StatusCode::UNPROCESSABLE_ENTITY),
            advisor_call_response("resp_c3", "call_h3"),
            advisor_executor_message("resp_final", "Never needed."),
        ],
    )
    .await;
    let advisor_posts = sent.iter().filter(|b| b["model"] == "beam").count();
    assert_eq!(advisor_posts, 2, "no advisor POST for the third call");
}

/// Each kind of advisor failure reaches the client as its own known error
/// code and tells the executor its own reason.
#[tokio::test]
async fn advisor_failures_report_their_own_code_and_reason() {
    let cases = [
        (
            FakeResponse {
                status: StatusCode::BAD_REQUEST,
                body: Some(
                    r#"{"error":{"message":"The prompt is too long: 1424454, model maximum context length: 1048576"}}"#
                        .into(),
                ),
            },
            1,
            "prompt_too_long",
            "too long for it",
        ),
        (
            upstream_failure(StatusCode::TOO_MANY_REQUESTS),
            2,
            "too_many_requests",
            "rate limited",
        ),
        (
            upstream_failure(StatusCode::BAD_GATEWAY),
            2,
            "overloaded",
            "overloaded or unreachable",
        ),
    ];
    for (failure, advisor_attempts, code, reason) in cases {
        // A too-long prompt is permanent (one attempt); 429 and 5xx are
        // retried once, so the retry fails the same way.
        let mut script = vec![advisor_call_response("resp_call", "call_kind")];
        script.extend((0..advisor_attempts).map(|_| FakeResponse {
            status: failure.status,
            body: failure.body.clone(),
        }));
        script.push(advisor_executor_message("resp_final", "Done."));
        let (body, sent) = advisor_turn_with_max_uses(2, script).await;
        assert_eq!(advisor_results(&body)[0]["content"]["error_code"], code);
        let told = sent.last().unwrap().to_string();
        assert!(told.contains(reason), "{code}: {told}");
        assert!(told.contains("without a second opinion"), "{code}: {told}");
    }
}

// ── System messages inside the conversation ─────────────────────────────

/// Claude Code sends `role: "system"` messages inside `messages` (the
/// environment block, "the user hasn't heard from you", hook output).
fn request_with_mid_conversation_system() -> &'static str {
    r#"{"model":"gpt-4o-mini","max_tokens":16,"system":"You are helpful.",
        "messages":[
          {"role":"user","content":"hello"},
          {"role":"system","content":[{"type":"text","text":"the user has not heard from you"},
                                       {"type":"tool_addition","tool":{"type":"tool_reference","name":"x"}}]},
          {"role":"user","content":"next"}]}"#
}

async fn upstream_body_for_mid_conversation_system(
    placement: openai_to_anthropic_proxy::config::SystemMessagePlacement,
) -> serde_json::Value {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned.lock().await = Some(completed_response_with_id("resp_ok", "ok"));
    let mut config = (*make_proxy_config(upstream_addr)).clone();
    config.system_message_placement = placement;
    let proxy_addr = start_proxy(Arc::new(config)).await;
    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(request_with_mid_conversation_system()))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let sent = upstream.received_all.lock().await.clone();
    serde_json::from_slice(&sent[0]).unwrap()
}

#[tokio::test]
async fn instructions_placement_sends_a_mid_conversation_system_message_to_the_instructions() {
    use openai_to_anthropic_proxy::config::SystemMessagePlacement;
    let body =
        upstream_body_for_mid_conversation_system(SystemMessagePlacement::Instructions).await;
    assert!(
        body["instructions"]
            .as_str()
            .unwrap()
            .contains("the user has not heard from you"),
        "{body}"
    );
    assert_eq!(body["input"].as_array().unwrap().len(), 2, "{body}");
}

#[tokio::test]
async fn inline_placement_sends_the_system_message_in_the_conversation() {
    use openai_to_anthropic_proxy::config::SystemMessagePlacement;
    let body = upstream_body_for_mid_conversation_system(SystemMessagePlacement::Inline).await;
    assert_eq!(body["instructions"], "You are helpful.", "{body}");
    let input = body["input"].as_array().unwrap();
    assert_eq!(input.len(), 3, "{body}");
    assert_eq!(input[1]["role"], "system");
    assert_eq!(
        input[1]["content"][0]["text"],
        "the user has not heard from you"
    );
    assert_eq!(input[2]["role"], "user");
}

/// With no setting at all, a mid-conversation system message stays in place.
#[tokio::test]
async fn a_mid_conversation_system_message_stays_in_place_by_default() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned.lock().await = Some(completed_response_with_id("resp_ok", "ok"));
    let proxy_addr = start_proxy(make_proxy_config(upstream_addr)).await;
    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(request_with_mid_conversation_system()))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let sent = upstream.received_all.lock().await.clone();
    let body: serde_json::Value = serde_json::from_slice(&sent[0]).unwrap();
    assert_eq!(body["instructions"], "You are helpful.", "{body}");
    assert_eq!(body["input"][1]["role"], "system", "{body}");
}

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

// ── document blocks ─────────────────────────────────────

fn request_with_pdf() -> String {
    r#"{
        "model":"claude-sonnet-5",
        "max_tokens":16,
        "stream":false,
        "messages":[{"role":"user","content":[
            {"type":"text","text":"read this"},
            {"type":"document","title":"a.pdf","source":{"type":"base64","media_type":"application/pdf","data":"JVBERi0="}},
            {"type":"document","source":{"type":"text","media_type":"text/plain","data":"plain body"}}
        ]}]
    }"#
    .to_string()
}

async fn post_pdf(forward_documents: bool) -> Vec<serde_json::Value> {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned.lock().await = Some(CANNED_RESPONSE.to_string());
    let mut config = (*make_proxy_config(upstream_addr)).clone();
    config.forward_documents = forward_documents;
    let proxy_addr = start_proxy(Arc::new(config)).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(request_with_pdf())
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let sent = upstream.received.lock().await.clone().expect("not called");
    let body: serde_json::Value = serde_json::from_slice(&sent).unwrap();
    body["input"][0]["content"].as_array().unwrap().clone()
}

#[tokio::test]
async fn a_pdf_is_noted_and_a_text_document_forwarded_by_default() {
    let parts = post_pdf(false).await;
    let kinds: Vec<&str> = parts.iter().map(|p| p["type"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["input_text", "input_text", "input_text"]);
    assert_eq!(
        parts[1]["text"],
        "[attached document omitted: a.pdf (application/pdf)]"
    );
    assert!(parts[2]["text"].as_str().unwrap().ends_with("plain body"));
}

#[tokio::test]
async fn a_pdf_reaches_the_upstream_as_input_file_when_enabled() {
    let parts = post_pdf(true).await;
    assert_eq!(parts[1]["type"], "input_file");
    assert_eq!(parts[1]["filename"], "a.pdf");
    assert_eq!(
        parts[1]["file_data"],
        "data:application/pdf;base64,JVBERi0="
    );
}

// ── upstream overload and rate-limit errors ─────────────

/// Send a non-streaming request to a proxy whose upstream answers with the
/// given error; return the proxy's response and the upstream's request count.
async fn post_with_upstream_error(
    status: u16,
    body: &str,
    headers: &[(&str, &str)],
) -> (reqwest::Response, usize) {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned_error.lock().await =
        Some((StatusCode::from_u16(status).unwrap(), body.to_owned()));
    *upstream.canned_error_headers.lock().await = headers
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    let proxy_addr = start_proxy(make_proxy_config(upstream_addr)).await;
    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(
            r#"{"model":"gpt-4o","max_tokens":16,"messages":[{"role":"user","content":"x"}]}"#,
        ))
        .send()
        .await
        .unwrap();
    let count = *upstream.request_count.lock().await;
    (res, count)
}

#[tokio::test]
async fn a_429_keeps_its_status_type_and_retry_after_and_is_not_retried() {
    let (res, count) = post_with_upstream_error(
        429,
        r#"{"error":{"message":"slow down","type":"rate_limit_error"}}"#,
        &[("retry-after", "7")],
    )
    .await;
    assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(res.headers()["retry-after"], "7");
    assert_eq!(res.headers()["retry-after-ms"], "7000");
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["error"]["type"], "rate_limit_error");
    assert_eq!(count, 1, "the proxy must leave retrying to the client");
}

#[tokio::test]
async fn overload_statuses_are_reported_as_overloaded_error() {
    for status in [503, 529] {
        let (res, _) =
            post_with_upstream_error(status, r#"{"error":{"message":"busy"}}"#, &[]).await;
        assert_eq!(res.status().as_u16(), status);
        assert!(!res.headers().contains_key("retry-after"));
        let body: serde_json::Value = res.json().await.unwrap();
        assert_eq!(body["error"]["type"], "overloaded_error", "{status}");
    }
}

#[tokio::test]
async fn a_403_is_a_permission_error() {
    let (res, _) = post_with_upstream_error(403, r#"{"error":{"message":"no"}}"#, &[]).await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["error"]["type"], "permission_error");
}

#[tokio::test]
async fn a_tokens_per_minute_429_stays_a_rate_limit_not_an_overflow() {
    let (res, count) = post_with_upstream_error(
        429,
        r#"{"error":{"message":"Request too large for gpt-4: tokens per min (TPM): Limit 30000, Requested 45000","code":"rate_limit_exceeded"}}"#,
        &[("retry-after", "2")],
    )
    .await;
    assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["error"]["type"], "rate_limit_error");
    assert!(
        !body["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("prompt is too long")
    );
    assert_eq!(count, 1);
}

#[tokio::test]
async fn a_stream_error_event_gets_the_type_its_code_names() {
    for (code, kind) in [
        ("rate_limit_exceeded", "rate_limit_error"),
        ("server_is_overloaded", "overloaded_error"),
        ("server_error", "api_error"),
    ] {
        let (upstream_addr, upstream) = start_fake_upstream().await;
        let sse = format!(
            r#"event: response.created
data: {{"type":"response.created","response":{{"id":"resp_1","object":"response","created_at":0,"status":"in_progress","model":"gpt-4o","output":[]}}}}

event: error
data: {{"type":"error","code":"{code}","message":"upstream trouble"}}

"#
        );
        *upstream.canned_stream.lock().await = Some(sse);
        let proxy_addr = start_proxy(make_proxy_config(upstream_addr)).await;
        let text = reqwest::Client::new()
            .post(format!("http://{proxy_addr}/v1/messages"))
            .header("content-type", "application/json")
            .body(ReqBody::from(
                r#"{"model":"gpt-4o","max_tokens":16,"stream":true,"messages":[{"role":"user","content":"x"}]}"#,
            ))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(
            text.contains(&format!(r#""type":"{kind}""#)),
            "{code}: {text}"
        );
    }
}

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

// ── A buffered upstream reply that is `200` with `status: "failed"` ──

/// A Responses body with `status: "failed"`, an optional `error` object and
/// optional partial output text.
fn failed_reply(error: Option<&str>, partial_text: Option<&str>) -> String {
    let output = partial_text.map_or_else(
        || "[]".to_owned(),
        |text| {
            format!(
                r#"[{{"type":"message","role":"assistant",
                    "content":[{{"type":"output_text","text":"{text}","annotations":[]}}]}}]"#
            )
        },
    );
    let error = error.unwrap_or("null");
    format!(
        r#"{{"id":"resp_f","object":"response","created_at":1,"status":"failed",
            "model":"any","output":{output},"error":{error},
            "usage":{{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}}"#
    )
}

/// POST a plain buffered request to a proxy whose upstream answers `canned`.
async fn post_to_canned_upstream(canned: String) -> reqwest::Response {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned.lock().await = Some(canned);
    let proxy_addr = start_proxy(make_proxy_config(upstream_addr)).await;
    reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(
            r#"{"model":"gpt-4o","max_tokens":64,"messages":[{"role":"user","content":"hi"}]}"#,
        ))
        .send()
        .await
        .unwrap()
}

async fn error_of(res: reqwest::Response) -> (u16, serde_json::Value) {
    let status = res.status().as_u16();
    (status, res.json().await.unwrap())
}

#[tokio::test]
async fn a_failed_200_is_a_real_error_not_a_message() {
    let res = post_to_canned_upstream(failed_reply(
        Some(r#"{"code":"server_error","message":"boom"}"#),
        None,
    ))
    .await;
    let (status, body) = error_of(res).await;
    assert_eq!(status, 502);
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "api_error");
    assert!(body["error"]["message"].as_str().unwrap().contains("boom"));
}

#[tokio::test]
async fn a_failed_200_rate_limit_is_a_429_the_client_can_retry() {
    let res = post_to_canned_upstream(failed_reply(
        Some(r#"{"code":"rate_limit_exceeded","message":"slow down"}"#),
        None,
    ))
    .await;
    let (status, body) = error_of(res).await;
    assert_eq!(status, 429);
    assert_eq!(body["error"]["type"], "rate_limit_error");
}

#[tokio::test]
async fn a_failed_200_overload_is_a_529() {
    let res = post_to_canned_upstream(failed_reply(
        Some(r#"{"code":"server_overloaded","message":"busy"}"#),
        None,
    ))
    .await;
    let (status, body) = error_of(res).await;
    assert_eq!(status, 529);
    assert_eq!(body["error"]["type"], "overloaded_error");
}

#[tokio::test]
async fn a_failed_200_context_overflow_is_anthropics_prompt_too_long() {
    let res = post_to_canned_upstream(failed_reply(
        Some(
            r#"{"code":"context_length_exceeded",
                "message":"The prompt is too long: 10, model maximum context length: 5"}"#,
        ),
        None,
    ))
    .await;
    let (status, body) = error_of(res).await;
    assert_eq!(status, 400);
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert_eq!(
        body["error"]["message"],
        "prompt is too long: 10 tokens > 5 maximum"
    );
}

#[tokio::test]
async fn a_failed_200_with_no_error_object_is_a_generic_502() {
    let (status, body) = error_of(post_to_canned_upstream(failed_reply(None, None)).await).await;
    assert_eq!(status, 502);
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("upstream response failed")
    );
}

#[tokio::test]
async fn a_failed_200_with_partial_output_is_still_an_error() {
    // Streaming reports an error even after some text, and so does this.
    let res = post_to_canned_upstream(failed_reply(
        Some(r#"{"code":"server_error","message":"cut off"}"#),
        Some("half an ans"),
    ))
    .await;
    assert_eq!(res.status().as_u16(), 502);
}

#[tokio::test]
async fn an_incomplete_200_is_still_a_message() {
    // Only `failed` is an error: a reply cut off at max_output_tokens stays a
    // normal message with `stop_reason: max_tokens`.
    let body = r#"{"id":"resp_i","object":"response","created_at":1,
        "status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},
        "model":"any","output":[{"type":"message","role":"assistant",
            "content":[{"type":"output_text","text":"partial","annotations":[]}]}],
        "usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}"#;
    let res = post_to_canned_upstream(body.to_owned()).await;
    assert_eq!(res.status().as_u16(), 200);
    let json: serde_json::Value = res.json().await.unwrap();
    assert_eq!(json["stop_reason"], "max_tokens");
}

#[tokio::test]
async fn a_failed_first_server_tool_run_is_the_clients_error() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![FakeResponse {
            status: StatusCode::OK,
            body: Some(failed_reply(
                Some(r#"{"code":"server_error","message":"first run failed"}"#),
                None,
            )),
        }],
    );
    let proxy_addr = start_proxy(advisor_config(upstream_addr)).await;
    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 502);
}

#[tokio::test]
async fn a_failed_later_server_tool_run_keeps_the_work_already_done() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    script(
        &upstream,
        vec![
            advisor_call_with_text_and_bash("resp_first", "call_f1", "Let me consult the advisor."),
            advisor_answer("Use a channel."),
            FakeResponse {
                status: StatusCode::OK,
                body: Some(failed_reply(
                    Some(r#"{"code":"server_error","message":"re-run failed"}"#),
                    None,
                )),
            },
        ],
    );
    let proxy_addr = start_proxy(advisor_config(upstream_addr)).await;
    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(advisor_request(false)))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status().as_u16(), 200, "a later failure is swallowed");
    let body: serde_json::Value = res.json().await.unwrap();
    assert!(
        body["content"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["text"] == "Let me consult the advisor."),
        "the last good answer survives: {body}"
    );
}

/// A tool call the upstream cut short (it ran out of output tokens)
/// keeps the parameters that were finished and reports
/// `stop_reason: max_tokens`. Anthropic's own API buffers each
/// parameter and drops the one that was still being written, so the
/// client sees the same shape here — not `{}`, which would look like a
/// valid empty call.
///
/// The proxy also does not spend a corrective round-trip on it: the
/// arguments did not break the schema, the output cap ended the call,
/// and re-asking with the same cap truncates the same way.
#[tokio::test]
async fn a_truncated_tool_call_keeps_its_finished_parameters() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned.lock().await = Some(
        r#"{
            "id": "resp_cut",
            "object": "response",
            "created_at": 1,
            "status": "incomplete",
            "incomplete_details": {"reason": "max_output_tokens"},
            "model": "gpt-4o",
            "output": [{
                "type": "function_call",
                "id": "fc_1",
                "status": "incomplete",
                "call_id": "call_1",
                "name": "Workflow",
                "arguments": "{\"script\":\"export const meta = {}\",\"resumeFromRunId\":\"half a "
            }],
            "usage": {"input_tokens": 5, "output_tokens": 60, "total_tokens": 65}
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
                "messages": [{"role": "user", "content": "run it"}],
                "tools": [{
                    "name": "Workflow",
                    "description": "Run a workflow",
                    "input_schema": {
                        "type": "object",
                        "properties": {
                            "script": {"type": "string"},
                            "resumeFromRunId": {"type": "string", "pattern": "^wf_[a-z0-9-]{6,}$"}
                        },
                        "required": ["script"]
                    }
                }]
            }"#,
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = res.json().await.unwrap();

    // The parameter that was finished survives; the one that was being
    // written when the cap hit is dropped.
    assert_eq!(
        body["content"][0]["input"],
        serde_json::json!({"script": "export const meta = {}"})
    );
    // The stop reason is what tells the client the call is incomplete.
    assert_eq!(body["stop_reason"], "max_tokens");

    // No corrective round-trip: the upstream was asked exactly once.
    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(sent.len(), 1, "a truncated reply is not retried");
}

/// The same, through the streaming path: the argument deltas are
/// buffered and shipped as one `input_json_delta` carrying the
/// parameters that were finished.
#[tokio::test]
async fn a_truncated_tool_call_streams_its_finished_parameters() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    let sse = [
        r#"event: response.created
data: {"type":"response.created","response":{"id":"resp_1","object":"response","created_at":0,"status":"in_progress","model":"gpt-4o","output":[]}}"#,
        r#"event: response.output_item.added
data: {"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_1","status":"in_progress","call_id":"call_1","name":"Workflow","arguments":""}}"#,
        r#"event: response.function_call_arguments.delta
data: {"type":"response.function_call_arguments.delta","item_id":"fc_1","output_index":0,"delta":"{\"script\":\"export const meta = {}\",\"resumeFromRunId\":\"half a "}"#,
        r#"event: response.incomplete
data: {"type":"response.incomplete","response":{"id":"resp_1","object":"response","created_at":0,"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"model":"gpt-4o","output":[{"type":"function_call","id":"fc_1","status":"incomplete","call_id":"call_1","name":"Workflow","arguments":"{\"script\":\"export const meta = {}\",\"resumeFromRunId\":\"half a "}],"usage":{"input_tokens":5,"output_tokens":60,"total_tokens":65}}}"#,
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
                "messages": [{"role": "user", "content": "run it"}],
                "tools": [{
                    "name": "Workflow",
                    "description": "Run a workflow",
                    "input_schema": {
                        "type": "object",
                        "properties": {
                            "script": {"type": "string"},
                            "resumeFromRunId": {"type": "string", "pattern": "^wf_[a-z0-9-]{6,}$"}
                        },
                        "required": ["script"]
                    }
                }]
            }"#,
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);
    let text = res.text().await.unwrap();

    // Exactly one input_json_delta, carrying the salvaged blob.
    let input_deltas: Vec<serde_json::Value> = text
        .split("\n\n")
        .filter(|s| s.contains("\"input_json_delta\""))
        .filter_map(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("data: "))
                .and_then(|d| serde_json::from_str(d).ok())
        })
        .collect();
    assert_eq!(input_deltas.len(), 1);
    let partial = input_deltas[0]["delta"]["partial_json"].as_str().unwrap();
    let parsed: serde_json::Value = serde_json::from_str(partial).unwrap();
    assert_eq!(
        parsed,
        serde_json::json!({"script": "export const meta = {}"})
    );

    // The stop reason is what tells the client the call is incomplete.
    let deltas: Vec<serde_json::Value> = text
        .split("\n\n")
        .filter(|s| s.contains("\"message_delta\""))
        .filter_map(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("data: "))
                .and_then(|d| serde_json::from_str(d).ok())
        })
        .collect();
    assert!(
        deltas
            .iter()
            .any(|d| d["delta"]["stop_reason"] == "max_tokens"),
        "expected stop_reason max_tokens in: {text}"
    );
}

// ─── a reply the upstream refused to give ───────────────────────────────────

/// A buffered reply the upstream declined: the message carries a `refusal`
/// content part, as OpenAI sends a model that declines to answer.
fn refusal_reply(refusal: &str) -> String {
    format!(
        r#"{{"id":"resp_r","object":"response","created_at":1,"status":"completed",
            "model":"any",
            "output":[{{"type":"message","role":"assistant",
                "content":[{{"type":"refusal","refusal":"{refusal}"}}]}}],
            "usage":{{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}}"#
    )
}

#[tokio::test]
async fn a_refused_reply_reports_the_refusal_stop_reason() {
    let res = post_to_canned_upstream(refusal_reply("I won't do that")).await;
    assert_eq!(res.status().as_u16(), 200);
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["stop_reason"], "refusal");
    // The refusal text still reaches the client, marked as a refusal.
    let text = body["content"][0]["text"].as_str().unwrap_or_default();
    assert!(text.starts_with("[refusal] "), "{text}");
    assert!(text.contains("I won't do that"), "{text}");
}

#[tokio::test]
async fn a_content_filtered_reply_reports_the_refusal_stop_reason() {
    // The other way an upstream declines: generation stopped by a safety
    // filter, so the reply is `incomplete` with that reason.
    let body = r#"{"id":"resp_c","object":"response","created_at":1,
        "status":"incomplete","incomplete_details":{"reason":"content_filter"},
        "model":"any","output":[],
        "usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}"#;
    let res = post_to_canned_upstream(body.to_owned()).await;
    assert_eq!(res.status().as_u16(), 200);
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["stop_reason"], "refusal");
}

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
