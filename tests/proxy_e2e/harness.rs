//! Shared e2e scaffolding: the fake upstream, proxy startup, canned
//! responses, and the request/`Config` builders used across topics.
//! No tests live here.

pub use std::collections::{BTreeMap, VecDeque};

pub use std::net::SocketAddr;

pub use std::sync::Arc;

pub use std::time::Duration;

pub use axum::Router;

pub use axum::body::Body;

pub use axum::extract::State;

pub use axum::http::StatusCode;

pub use axum::response::{IntoResponse, Response};

pub use axum::routing::post;

pub use bytes::Bytes;

pub use openai_to_anthropic_proxy::config::{
    AdvisorConfig, Config, ModelAliases, ReasoningConfig, ServerToolsConfig, ToolResultImages,
    WebFetchConfig, WebSearchConfig,
};

pub use reqwest::Body as ReqBody;

pub use tokio::net::TcpListener;

pub use tokio::sync::Mutex;

/// State for the fake upstream.
#[derive(Clone, Default)]
pub struct FakeUpstream {
    /// Canned JSON body to return for any POST.
    pub canned: Arc<Mutex<Option<String>>>,
    /// Canned SSE body to return for any POST.
    pub canned_stream: Arc<Mutex<Option<String>>>,
    /// Canned error response (status + body) to return.
    pub canned_error: Arc<Mutex<Option<(StatusCode, String)>>>,
    /// Extra response headers sent with `canned_error`.
    pub canned_error_headers: Arc<Mutex<Vec<(String, String)>>>,
    /// Per-attempt canned responses, served in order. When the vec is
    /// exhausted the handler falls back to the legacy `canned` /
    /// `canned_error` fields. Lets tests drive multi-attempt
    /// scenarios (e.g. proxy's default_model fallback) where the
    /// first send must fail and the second must succeed.
    pub canned_per_attempt: Arc<Mutex<VecDeque<FakeResponse>>>,
    /// Optional delay before returning a canned response.
    pub delay: Arc<Mutex<Option<Duration>>>,
    /// Per-attempt delay, consumed in order. Lets a test stall ONLY the
    /// advisor POST (attempt 2) while the executor calls stay fast.
    pub delay_per_attempt: Arc<Mutex<VecDeque<Duration>>>,
    /// The most recent JSON body the proxy sent to the upstream.
    pub received: Arc<Mutex<Option<Bytes>>>,
    /// Every JSON body the proxy sent to the upstream, in order.
    /// Used to assert the proxy actually retried (vs. silently
    /// succeeding on the first attempt).
    pub received_all: Arc<Mutex<Vec<Bytes>>>,
    /// Number of times the handler has been hit. Mirrors the
    /// length of `received_all` but exposed for quick assertions.
    pub request_count: Arc<Mutex<usize>>,
}

/// A scripted response the fake upstream returns on a given attempt.
pub struct FakeResponse {
    pub status: StatusCode,
    pub body: Option<String>,
}

pub async fn handle_fake(State(s): State<FakeUpstream>, body: Bytes) -> Response {
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

pub fn build_fake_response(resp: FakeResponse) -> Response {
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
pub const REAL_GATEWAY_MODEL_404: &str = r#"{"error":{"message":"model \"claude-sonnet-5\" not found. Try glm-5.3-flash:cloud.","type":"not_found_error","param":null,"code":null}}"#;

/// A proxy config with an inbound→upstream alias map and a
/// `default_model`, for the model-fallback tests.
pub fn make_config_with_default_model(
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

pub async fn start_fake_upstream() -> (SocketAddr, FakeUpstream) {
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

pub fn make_proxy_config(addr: SocketAddr) -> Arc<Config> {
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

pub async fn start_proxy(config: Arc<Config>) -> SocketAddr {
    use openai_to_anthropic_proxy::{MappingsStore, RuntimeMappings};
    let store = Arc::new(MappingsStore::seeded(Arc::new(
        RuntimeMappings::from_config(&config),
    )));
    start_proxy_with_store(config, store).await
}

/// Like `start_proxy`, but lets the caller inject a pre-built
/// `MappingsStore` so tests can drive runtime-edits of the alias
/// table (the contract the TUI exposes).
pub async fn start_proxy_with_store(
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

/// Like `start_proxy`, but shares the caller's `SessionStatsStore` and TUI
/// `OutputSink` so a test can assert on attribution and emitted log lines.
/// Uses `router_with_stores` with a fresh capability store to keep every
/// test's learned-parameter latch independent.
pub async fn start_proxy_with_captured_stores(
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

// ─── Phase-3 advisor integration tests ─────────────────────────────────────
//
// All of these use the existing in-process `FakeUpstream`. No real API is
// called, and the only advisor model name anywhere in this file is the cheap
// default `beam`. `enabled: false` (the shipped default) is verified to be a
// byte-for-byte no-op before any of the enabled tests run.

pub const ADVISOR_DECLARATION: &str =
    r#"{"type":"advisor_20260301","name":"advisor","model":"beam"}"#;

/// Claude Code's three-field declaration plus an advisor-enabled config.
/// `max_tokens: 64` and `timeout_secs: 2` keep every test cheap and bounded.
pub fn advisor_config(addr: SocketAddr) -> Arc<Config> {
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

pub fn advisor_request(stream: bool) -> String {
    advisor_request_with_decl(stream, ADVISOR_DECLARATION)
}

pub fn advisor_request_with_decl(stream: bool, declaration: &str) -> String {
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

pub fn completed_response_with_id(id: &str, text: &str) -> String {
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
pub fn advisor_call_response(id: &str, call_id: &str) -> FakeResponse {
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
pub fn advisor_answer(text: &str) -> FakeResponse {
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

pub fn script(upstream: &FakeUpstream, responses: Vec<FakeResponse>) {
    *upstream.canned_per_attempt.try_lock().unwrap() = VecDeque::from(responses);
}

// ─── Regression tests for the confirmed advisor bugs (B1–B12) ──────────────
//
// All use the in-process fake upstream. No real API calls, no expensive
// model names: the only advisor model is `beam`.

/// An executor sampling that produces BOTH visible text and a sibling client
/// tool_use alongside the advisor call. Used by the B1/B6 fidelity tests.
pub fn advisor_call_with_text_and_bash(id: &str, call_id: &str, text: &str) -> FakeResponse {
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

pub const CANNED_RESPONSE: &str = r#"{
    "id":"resp_img",
    "object":"response",
    "created_at":1,
    "status":"completed",
    "model":"any",
    "output":[{"type":"message","id":"m1","status":"completed","role":"assistant","content":[{"type":"output_text","text":"ok","annotations":[]}]}],
    "usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}
}"#;

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
pub const WEB_FETCH_DECLARATION: &str =
    r#"{"type":"web_fetch_20250910","name":"web_fetch","max_uses":10}"#;

/// A request that declares the server-side `web_fetch` tool.
pub fn web_fetch_request(stream: bool) -> String {
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
pub fn web_fetch_call_response(url: &str) -> String {
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
pub fn web_fetch_config(upstream: SocketAddr) -> Arc<Config> {
    let mut config = (*make_proxy_config(upstream)).clone();
    config.web_fetch = WebFetchConfig {
        enabled: true,
        timeout_secs: Some(2),
        max_bytes: None,
        max_content_bytes: None,
    };
    Arc::new(config)
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
pub const WEB_SEARCH_DECLARATION: &str =
    r#"{"type":"web_search_20250305","name":"web_search","max_uses":8}"#;

pub fn web_search_request(stream: bool) -> String {
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
pub fn web_search_call_response(queries: &[&str]) -> String {
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
pub async fn start_fake_search_engine(html: &'static str) -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
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

pub const FAKE_RESULTS_HTML: &str = r#"<div id="links">
  <div class="result web-result"><h2><a href="//duckduckgo.com/l/?uddg=http%3A%2F%2F127.0.0.1%3A9%2Fone">First hit</a></h2>
    <a class="result__snippet">first snippet</a></div>
  <div class="result web-result"><h2><a href="//duckduckgo.com/l/?uddg=http%3A%2F%2F127.0.0.1%3A9%2Ftwo">Second hit</a></h2>
    <a class="result__snippet">second snippet</a></div>
</div>"#;

pub fn web_search_config(upstream: SocketAddr, engine: SocketAddr) -> Arc<Config> {
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

pub async fn post_messages(proxy: SocketAddr, body: String) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("http://{proxy}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(body))
        .send()
        .await
        .unwrap()
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
pub fn server_tools_request(stream: bool, declarations: &[&str]) -> String {
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
pub fn function_calls_response(id: &str, calls: &[(&str, &str, serde_json::Value)]) -> String {
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

pub fn ok_response(body: String) -> FakeResponse {
    FakeResponse {
        status: StatusCode::OK,
        body: Some(body),
    }
}

/// `web_search`, `web_fetch` and the advisor all enabled.
pub fn all_server_tools_config(upstream: SocketAddr, engine: SocketAddr) -> Arc<Config> {
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

pub fn content_of(body: &serde_json::Value) -> &Vec<serde_json::Value> {
    body["content"].as_array().expect("content array")
}

pub fn blocks_of_type<'a>(
    content: &'a [serde_json::Value],
    kind: &str,
) -> Vec<&'a serde_json::Value> {
    content.iter().filter(|b| b["type"] == kind).collect()
}

pub const SEARCH_DECL: &str = r#"{"type":"web_search_20250305","name":"web_search"}"#;

pub fn search_round(id: &str, call_id: &str, query: &str) -> FakeResponse {
    ok_response(function_calls_response(
        id,
        &[(
            "proxy_web_search",
            call_id,
            serde_json::json!({"queries": [query]}),
        )],
    ))
}

pub fn advisor_results(body: &serde_json::Value) -> Vec<&serde_json::Value> {
    body["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|b| b["type"] == "advisor_tool_result")
        .collect()
}

// ── A buffered upstream reply that is `200` with `status: "failed"` ──

/// A Responses body with `status: "failed"`, an optional `error` object and
/// optional partial output text.
pub fn failed_reply(error: Option<&str>, partial_text: Option<&str>) -> String {
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
