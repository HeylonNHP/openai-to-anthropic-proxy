//! Per-model `reasoning_effort` lookup, the effort map, and the
//! thinking-disabled map.

use crate::harness::*;

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
