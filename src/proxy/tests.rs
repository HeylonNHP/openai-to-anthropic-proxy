use super::*;

#[test]
fn synthetic_message_id_uses_msg_prefix() {
    let req = CreateMessageRequest {
        model: "gpt-4o".into(),
        max_tokens: 16,
        messages: vec![],
        system: None,
        tools: None,
        tool_choice: None,
        temperature: None,
        top_p: None,
        top_k: None,
        stop_sequences: None,
        stream: Some(true),
        metadata: None,
        output_config: None,
        thinking: None,
    };
    let id = synthetic_message_id(&req);
    assert!(id.starts_with("msg_"));
    assert!(id.contains("gpt-4o"));
}

#[test]
fn sanitize_model_keeps_safe_chars() {
    assert_eq!(sanitize_model("gpt-4o"), "gpt-4o");
    assert_eq!(sanitize_model("claude/opus 4.8"), "claude_opus_4_8");
}

#[test]
fn stats_model_groups_mapped_requests_by_inbound_name() {
    assert_eq!(
        stats_model("claude-sonnet", "gpt-sonnet", false),
        "claude-sonnet"
    );
}

#[test]
fn stats_model_attributes_fallback_to_actual_model() {
    assert_eq!(
        stats_model("claude-sonnet", "gpt-fallback", true),
        "gpt-fallback"
    );
}

#[test]
fn stats_model_keeps_unmapped_inbound_name_separate() {
    assert_eq!(
        stats_model("claude-unknown", "claude-unknown", false),
        "claude-unknown"
    );
}

fn test_config_with_key(key: Option<&str>) -> Arc<Config> {
    // Build a minimal config that satisfies the fields
    // `check_proxy_key` reads. The other fields don't matter.
    Arc::new(Config {
        proxy_key: key.map(String::from),
        ..test_config_minimal()
    })
}

/// The operator-facing docs (the config guide, the scratchpad) tell the
/// operator to watch `replayed_chars` on the "→ upstream" line. This
/// guards against that field being documented but never actually added
/// to the log statement — which is exactly what happened once already.
#[test]
fn the_upstream_log_line_reports_the_replayed_reasoning_count() {
    // The log statement lives in the PARENT module (`src/proxy.rs`), not in
    // this test file, so the path is explicit rather than `file!()`.
    let source = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/proxy.rs"))
        .expect("read proxy.rs");
    let marker = "\"→ upstream\"";
    let marker_at = source.rfind(marker).expect("the → upstream log statement");
    // The fields of a `tracing::info!` sit between its opening paren
    // and the message, so the statement is the text from the last
    // `tracing::info!(` before the marker up to the marker itself.
    let before = &source[..marker_at];
    let statement = before
        .rfind("tracing::info!(")
        .map(|at| &before[at..])
        .expect("the → upstream tracing::info! statement");
    assert!(
        statement.contains("replayed_chars"),
        "the → upstream log line does not report replayed_chars"
    );
    assert!(
        statement.contains("reasoning_replay.replayed_chars.get()"),
        "the → upstream log line does not read the replayed counter"
    );
}

fn test_config_minimal() -> Config {
    Config {
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        upstream_base_url: "https://example.com".into(),
        upstream_api_key: "test".into(),
        upstream_path: "/v1/responses".into(),
        request_timeout: std::time::Duration::from_secs(60),
        max_request_body_bytes: crate::config::DEFAULT_MAX_REQUEST_BODY_BYTES,
        reasoning_effort: None,
        reasoning: crate::config::ReasoningConfig::default(),
        reasoning_replay: crate::config::ReasoningReplayConfig::default(),
        model_aliases: crate::config::ModelAliases::default(),
        prompt_caching: crate::config::PromptCachingConfig::default(),
        log_to_disk: false,
        proxy_key: None,
        advisor: crate::config::AdvisorConfig::default(),
        web_fetch: crate::config::WebFetchConfig::default(),
        web_search: crate::config::WebSearchConfig::default(),
        server_tools: crate::config::ServerToolsConfig::default(),
        prefix_models_for_discovery: false,
        tool_result_images: crate::config::ToolResultImages::default(),
        forward_documents: false,
        min_output_tokens: crate::config::DEFAULT_MIN_OUTPUT_TOKENS,
        system_message_placement: Default::default(),
        warnings: Vec::new(),
    }
}

#[test]
fn ct_eq_matches_identical_strings() {
    assert!(ct_eq(b"hello", b"hello"));
    assert!(ct_eq(b"", b""));
    assert!(ct_eq(b"a-longer-key-12345", b"a-longer-key-12345"));
}

#[test]
fn ct_eq_rejects_different_strings() {
    assert!(!ct_eq(b"hello", b"world"));
    assert!(!ct_eq(b"hello", b"hell"));
    assert!(!ct_eq(b"hell", b"hello"));
    // Empty vs non-empty.
    assert!(!ct_eq(b"", b"x"));
    assert!(!ct_eq(b"x", b""));
}

#[test]
fn check_proxy_key_no_key_configured_allows_all() {
    // No `proxy_key` set → no auth, no header required. Current
    // behavior: any reachable client can use the proxy.
    let config = test_config_with_key(None);
    let headers = HeaderMap::new();
    assert!(check_proxy_key(&config, &headers).is_ok());
}

#[test]
fn check_proxy_key_correct_header_passes() {
    let config = test_config_with_key(Some("shared-secret-1234"));
    let mut headers = HeaderMap::new();
    headers.insert(PROXY_KEY_HEADER, "shared-secret-1234".parse().unwrap());
    assert!(check_proxy_key(&config, &headers).is_ok());
}

#[test]
fn check_proxy_key_missing_header_rejects() {
    let config = test_config_with_key(Some("shared-secret-1234"));
    let headers = HeaderMap::new();
    let err = check_proxy_key(&config, &headers).unwrap_err();
    // Should be Unauthorized, not BadRequest or Internal.
    assert!(matches!(err, AppError::Unauthorized));
}

#[test]
fn check_proxy_key_wrong_header_rejects() {
    let config = test_config_with_key(Some("shared-secret-1234"));
    let mut headers = HeaderMap::new();
    headers.insert(PROXY_KEY_HEADER, "wrong".parse().unwrap());
    let err = check_proxy_key(&config, &headers).unwrap_err();
    assert!(matches!(err, AppError::Unauthorized));
}

#[test]
fn check_proxy_key_header_is_case_insensitive() {
    // HTTP header names are case-insensitive. Verify the lookup
    // works regardless of how the client capitalized the header.
    let config = test_config_with_key(Some("k"));
    for variant in ["X-Proxy-Key", "x-proxy-key", "X-PROXY-KEY", "x-Proxy-Key"] {
        let mut headers = HeaderMap::new();
        headers.insert(variant, "k".parse().unwrap());
        assert!(
            check_proxy_key(&config, &headers).is_ok(),
            "{variant:?} should match"
        );
    }
}

#[test]
fn unauthorized_response_returns_401() {
    // Pin the wire shape of an unauthorized response so a future
    // refactor can't accidentally downgrade it to 400 or 500.
    let resp = AppError::Unauthorized.into_response();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
