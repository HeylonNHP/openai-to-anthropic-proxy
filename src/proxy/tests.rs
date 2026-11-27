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
        model_info: Default::default(),
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
        max_concurrent_requests: 0,
        rate_limit_retries: 0,
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

// ─── The plain streaming path's keep-alive ───────────────────────────────

/// A `response.created` payload: enough for the translator to open the
/// message.
const CREATED: &str = r#"{"type":"response.created","response":{"id":"resp_1","object":"response","created_at":0,"status":"in_progress","model":"test-model","output":[]}}"#;

/// A text delta.
const DELTA: &str = r#"{"type":"response.output_text.delta","item_id":"msg_1","output_index":0,"content_index":0,"delta":"hello"}"#;

/// The terminal event.
const COMPLETED: &str = r#"{"type":"response.completed","response":{"id":"resp_1","object":"response","created_at":0,"status":"completed","model":"test-model","output":[]}}"#;

/// The upstream stream: the given events, then silence for good.
fn upstream(
    events: &[&str],
) -> impl Stream<Item = Result<SseEvent, EventStreamError<reqwest::Error>>> + Unpin {
    let items: Vec<Result<SseEvent, EventStreamError<reqwest::Error>>> = events
        .iter()
        .map(|data| {
            Ok(SseEvent {
                data: (*data).to_owned(),
                ..SseEvent::default()
            })
        })
        .collect();
    futures_util::stream::iter(items).chain(futures_util::stream::pending())
}

/// A translator over `inner`. The TUI sink keeps the response line out
/// of stdout.
fn translator_stream<S>(inner: S) -> TranslatorStream<S>
where
    S: Stream<Item = Result<SseEvent, EventStreamError<reqwest::Error>>> + Unpin,
{
    translator_stream_with(inner, false)
}

/// The same, with the early-start placeholder already sent: the
/// translator suppresses its own `message_start` and never pings — the
/// wrapping body owns both.
fn translator_stream_with<S>(inner: S, placeholder_sent: bool) -> TranslatorStream<S>
where
    S: Stream<Item = Result<SseEvent, EventStreamError<reqwest::Error>>> + Unpin,
{
    let (sink, _rx) = crate::tui::OutputSink::tui();
    TranslatorStream::new(
        inner,
        TranslatorStreamConfig {
            msg_id: "msg_test".to_owned(),
            model: "test-model".to_owned(),
            registry: crate::repair::ToolSchemaRegistry::new(),
            start: Instant::now(),
            sink,
            session_stats: Arc::new(crate::tui::SessionStatsStore::new()),
            stats_model: "test-model".to_owned(),
            fallback_used: false,
            placeholder_sent,
        },
    )
}

/// Poll a stream once by hand with a no-op waker: the test drives the
/// stream, so the paused clock only moves when the test says so.
fn poll_once<S>(stream: &mut S) -> Poll<Option<S::Item>>
where
    S: Stream + Unpin,
{
    let mut cx = Context::from_waker(std::task::Waker::noop());
    Pin::new(stream).poll_next(&mut cx)
}

/// The chunk the poll produced, as text.
fn chunk(poll: Poll<Option<Result<Bytes, std::io::Error>>>) -> String {
    let Poll::Ready(Some(Ok(bytes))) = poll else {
        panic!("expected a chunk");
    };
    String::from_utf8(bytes.to_vec()).expect("the chunk is UTF-8")
}

#[tokio::test(start_paused = true)]
async fn the_plain_stream_pings_after_a_quiet_spell() {
    let mut stream = translator_stream(upstream(&[CREATED]));

    // The first thing the client hears is the message opening.
    let first = chunk(poll_once(&mut stream));
    assert!(first.contains("event: message_start"), "{first}");

    // Nothing has been said since, but the keep-alive is not due yet.
    assert!(poll_once(&mut stream).is_pending());

    // A full interval later it is.
    tokio::time::advance(PING_INTERVAL).await;
    assert_eq!(
        chunk(poll_once(&mut stream)),
        "event: ping\ndata: {\"type\":\"ping\"}\n\n"
    );
}

#[tokio::test(start_paused = true)]
async fn the_plain_stream_restarts_the_keep_alive_on_content() {
    let mut stream = translator_stream(upstream(&[CREATED, DELTA]));

    let first = chunk(poll_once(&mut stream));
    assert!(first.contains("event: message_start"), "{first}");

    // Content arrives; it must go out as content, not as a ping.
    tokio::time::advance(PING_INTERVAL / 2).await;
    let second = chunk(poll_once(&mut stream));
    assert!(second.contains("event: content_block_delta"), "{second}");
    assert!(!second.contains("event: ping"), "{second}");

    // The content restarted the keep-alive: it is due a full interval
    // from now, not from the message start.
    assert_eq!(
        stream
            .ping_timer
            .as_ref()
            .expect("the keep-alive is running")
            .deadline(),
        tokio::time::Instant::now() + PING_INTERVAL
    );

    // And so the stream stays quiet until then.
    assert!(poll_once(&mut stream).is_pending());
    tokio::time::advance(PING_INTERVAL).await;
    assert_eq!(
        chunk(poll_once(&mut stream)),
        "event: ping\ndata: {\"type\":\"ping\"}\n\n"
    );
}

#[tokio::test(start_paused = true)]
async fn the_plain_stream_does_not_ping_before_the_message_starts() {
    // The upstream says nothing at all: there is no message yet, so
    // there is nothing to keep alive.
    let mut stream = translator_stream(upstream(&[]));
    assert!(poll_once(&mut stream).is_pending());
    tokio::time::advance(PING_INTERVAL * 2).await;
    assert!(poll_once(&mut stream).is_pending());

    // The upstream sends content without the `response.created` that
    // opens the message. The keep-alive is running now, but it must
    // still wait for the message itself.
    let mut stream = translator_stream(upstream(&[DELTA]));
    let first = chunk(poll_once(&mut stream));
    assert!(first.contains("event: content_block_delta"), "{first}");
    assert!(!first.contains("event: message_start"), "{first}");
    tokio::time::advance(PING_INTERVAL).await;
    assert!(poll_once(&mut stream).is_pending());
}

/// Behind an early-start placeholder the translator must keep quiet about
/// `message_start` (the placeholder was it) and never start its own
/// keep-alive (the wrapping body pings): content flows, nothing else.
#[tokio::test(start_paused = true)]
async fn a_spliced_translator_neither_reopens_the_message_nor_pings() {
    let mut stream = translator_stream_with(upstream(&[CREATED, DELTA]), true);

    // The `response.created` that would normally open the message is
    // swallowed; the content that follows goes straight out.
    let first = chunk(poll_once(&mut stream));
    assert!(first.contains("event: content_block_delta"), "{first}");
    assert!(!first.contains("event: message_start"), "{first}");

    // A long quiet spell never produces a ping from the translator — no
    // timer was ever created.
    tokio::time::advance(PING_INTERVAL * 3).await;
    assert!(poll_once(&mut stream).is_pending());
    assert!(
        stream.ping_timer.is_none(),
        "the external body owns the keep-alive"
    );

    // The closing events still arrive, and still without a second
    // `message_start`.
    let mut stream = translator_stream_with(upstream(&[CREATED, COMPLETED]), true);
    let last = chunk(poll_once(&mut stream));
    assert!(last.contains("event: message_stop"), "{last}");
    assert!(!last.contains("event: message_start"), "{last}");
}

#[tokio::test(start_paused = true)]
async fn the_plain_stream_does_not_ping_after_the_terminal_event() {
    let mut stream = translator_stream(upstream(&[CREATED, COMPLETED]));

    let first = chunk(poll_once(&mut stream));
    assert!(first.contains("event: message_start"), "{first}");

    // The upstream finishes long after the keep-alive would have fired.
    tokio::time::advance(PING_INTERVAL * 2).await;
    let last = chunk(poll_once(&mut stream));
    assert!(last.contains("event: message_stop"), "{last}");
    assert!(!last.contains("event: ping"), "{last}");

    // And the stream ends there: no ping follows the message.
    assert!(matches!(poll_once(&mut stream), Poll::Ready(None)));
}

/// A terminal event whose `response` omits `model` and `output`: serde
/// used to reject it, so the translator never saw the turn end and the
/// client got a spurious `api_error` for a successful response.
const COMPLETED_MINIMAL: &str = r#"{"type":"response.completed","response":{"id":"resp_1","object":"response","created_at":0,"status":"completed"}}"#;

#[tokio::test(start_paused = true)]
async fn a_terminal_event_missing_model_and_output_still_ends_the_message() {
    let mut stream = translator_stream(upstream(&[CREATED, COMPLETED_MINIMAL]));

    let first = chunk(poll_once(&mut stream));
    assert!(first.contains("event: message_start"), "{first}");

    // The turn ends normally instead of being dropped: no `error`
    // event, stop reason `end_turn`.
    let last = chunk(poll_once(&mut stream));
    assert!(last.contains("event: message_delta"), "{last}");
    assert!(last.contains("end_turn"), "{last}");
    assert!(last.contains("event: message_stop"), "{last}");
    assert!(!last.contains("event: error"), "{last}");

    // And the stream closes cleanly after that.
    assert!(matches!(poll_once(&mut stream), Poll::Ready(None)));
}

// ─── GET /v1/models: metadata and paging ─────────────────────────────────

/// Model rows for paging tests, built through the same constructor the
/// handler uses for synthesised names.
fn model_rows(ids: &[&str]) -> Vec<AnthropicModelInfo> {
    ids.iter().map(|id| alias_model_row(id, None)).collect()
}

#[test]
fn the_models_list_pages_twenty_by_default_and_sorts_by_id() {
    // 25 rows, deliberately reversed: paging sorts first.
    let mut ids: Vec<String> = (0..25).map(|i| format!("m{i:02}")).collect();
    ids.reverse();
    let refs: Vec<&str> = ids.iter().map(String::as_str).collect();

    let page = page_models(model_rows(&refs), None, None, None).expect("valid default page");
    assert_eq!(page.data.len(), 20);
    assert!(page.has_more);
    assert_eq!(page.first_id.as_deref(), Some("m00"));
    assert_eq!(page.last_id.as_deref(), Some("m19"));
    assert_eq!(page.data[0].id, "m00", "sorted, not input order");
}

#[test]
fn cursors_and_limit_window_the_page() {
    let ids: Vec<String> = (0..10).map(|i| format!("m{i:02}")).collect();
    let refs: Vec<&str> = ids.iter().map(String::as_str).collect();

    // `after_id` is exclusive: the page starts at the row after it.
    let page = page_models(model_rows(&refs), Some("m02"), None, Some(3)).unwrap();
    let got: Vec<&str> = page.data.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(got, ["m03", "m04", "m05"]);
    assert!(page.has_more);
    assert_eq!(page.first_id.as_deref(), Some("m03"));
    assert_eq!(page.last_id.as_deref(), Some("m05"));

    // `before_id` is exclusive the other way; the window is exhausted,
    // so there is no next page.
    let page = page_models(model_rows(&refs), None, Some("m03"), None).unwrap();
    assert_eq!(page.data.len(), 3);
    assert!(!page.has_more);

    // Both cursors with limit 1: one row out of the window, more behind it.
    let page = page_models(model_rows(&refs), Some("m02"), Some("m06"), Some(1)).unwrap();
    let got: Vec<&str> = page.data.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(got, ["m03"]);
    assert!(page.has_more);
}

#[test]
fn an_out_of_range_limit_is_rejected() {
    let err = page_models(model_rows(&["a"]), None, None, Some(0)).unwrap_err();
    assert!(matches!(err, AppError::BadRequest(_)));
    let err = page_models(model_rows(&["a"]), None, None, Some(1001)).unwrap_err();
    assert!(matches!(err, AppError::BadRequest(_)));
}

#[test]
fn an_empty_page_has_no_boundaries() {
    let page = page_models(Vec::new(), None, None, None).unwrap();
    assert!(!page.has_more);
    assert!(page.first_id.is_none());
    assert!(page.last_id.is_none());
}

#[test]
fn model_info_metadata_lands_on_the_row() {
    let mut row = alias_model_row("claude-sonnet-5", None);
    assert_eq!(row.created_at, epoch_to_rfc3339(0), "nothing to inherit");

    let info = crate::config::ModelInfoEntry {
        max_input_tokens: Some(1_000_000),
        max_tokens: Some(64_000),
        capabilities: Some(serde_json::json!({"thinking": {"supported": true}})),
        description: Some("GPT-5.4-mini behind the alias".to_owned()),
        created_at: Some("2026-01-02T03:04:05Z".to_owned()),
    };
    apply_model_info(&mut row, Some(&info));
    assert_eq!(row.max_input_tokens, Some(1_000_000));
    assert_eq!(row.max_tokens, Some(64_000));
    assert_eq!(
        row.capabilities,
        Some(serde_json::json!({"thinking": {"supported": true}}))
    );
    assert_eq!(
        row.description.as_deref(),
        Some("GPT-5.4-mini behind the alias")
    );
    // The configured date wins over the epoch fallback.
    assert_eq!(row.created_at, "2026-01-02T03:04:05Z");
}

#[test]
fn an_alias_row_inherits_a_real_release_date() {
    // 1_700_000_000 is 2023-11-14: a real date, not the epoch.
    let row = alias_model_row("claude-sonnet-5", Some(1_700_000_000));
    assert_ne!(row.created_at, epoch_to_rfc3339(0));
    assert!(row.created_at.contains("2023"), "{}", row.created_at);

    // An upstream that reported no release date gives nothing to inherit.
    let row = alias_model_row("claude-sonnet-5", Some(0));
    assert_eq!(row.created_at, epoch_to_rfc3339(0));
    let row = alias_model_row("claude-sonnet-5", None);
    assert_eq!(row.created_at, epoch_to_rfc3339(0));
}

#[test]
fn no_config_leaves_the_row_alone() {
    let mut row = alias_model_row("claude-sonnet-5", Some(1_700_000_000));
    let before = row.created_at.clone();
    apply_model_info(&mut row, None);
    assert!(row.description.is_none());
    assert!(row.max_input_tokens.is_none());
    assert_eq!(row.created_at, before);
}

// ─── GET /v1/models/{model_id}: the row lookup ──────────────────────────

#[test]
fn a_model_row_is_found_by_its_id() {
    let rows = vec![
        alias_model_row("claude-sonnet-5", None),
        alias_model_row("gpt-4o-mini", None),
    ];
    assert_eq!(
        find_model_row(&rows, "claude-sonnet-5").map(|r| r.id.as_str()),
        Some("claude-sonnet-5"),
        "an alias key is a row of its own"
    );
    assert_eq!(
        find_model_row(&rows, "gpt-4o-mini").map(|r| r.id.as_str()),
        Some("gpt-4o-mini"),
        "the default-model row answers under its own id"
    );
}

#[test]
fn a_model_row_is_found_by_the_raw_id_behind_a_discovery_prefix() {
    let mut row = alias_model_row("gpt-4o", None);
    // What prefix_models_for_discovery produces: the client-facing id
    // prefixed, the raw upstream id kept as the display name.
    row.id = "claude-gpt-4o".to_owned();
    let rows = vec![row];

    assert_eq!(
        find_model_row(&rows, "claude-gpt-4o").map(|r| r.id.as_str()),
        Some("claude-gpt-4o"),
        "the client-facing id matches exactly"
    );
    assert_eq!(
        find_model_row(&rows, "gpt-4o").map(|r| r.id.as_str()),
        Some("claude-gpt-4o"),
        "the raw upstream id resolves to the same row"
    );
}

#[test]
fn an_unknown_model_id_finds_nothing() {
    let rows = vec![alias_model_row("claude-sonnet-5", None)];
    assert!(find_model_row(&rows, "claude-opus-9000").is_none());
    assert!(find_model_row(&[], "claude-sonnet-5").is_none());
}
