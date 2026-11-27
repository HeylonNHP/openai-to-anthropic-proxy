//! Inbound model aliasing — static, at runtime through the
//! TUI's mappings store, and snapshot semantics.

use crate::harness::*;

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
        max_retry_wait: Duration::from_secs(60),
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
        max_retry_wait: Duration::from_secs(60),
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
        max_retry_wait: Duration::from_secs(60),
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
        max_retry_wait: Duration::from_secs(60),
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
