use super::*;

fn call(args: &str) -> Call {
    Call {
        call_id: "call_1".to_owned(),
        name: WEB_FETCH_TOOL_NAME.to_owned(),
        arguments: args.to_owned(),
    }
}

/// A URL the SSRF guard refuses before any network access, so the failure
/// path runs offline and deterministically.
const REFUSED: &str = r#"{"url":"http://127.0.0.1:9/page"}"#;

fn raw(block: &ResponseContentBlock) -> &Value {
    match block {
        ResponseContentBlock::Raw(v) => v,
        other => panic!("expected a raw block, got {other:?}"),
    }
}

fn fetch_usage(tool: &WebFetchTool) -> Option<u32> {
    let mut usage = Usage::default();
    tool.contribute_usage(&mut usage);
    usage.server_tool_use.map(|u| u.web_fetch_requests)
}

#[test]
fn the_tool_answers_to_web_fetch() {
    assert_eq!(
        WebFetchTool::new(&WebFetchConfig::default(), &ServerToolOptions::default())
            .executor_name(),
        "web_fetch"
    );
}

#[test]
fn the_url_is_read_and_trimmed() {
    assert_eq!(
        parse_url_argument(r#"{"url": "  https://example.com/page  "}"#).as_deref(),
        Some("https://example.com/page")
    );
}

#[test]
fn a_call_with_no_usable_url_has_no_url() {
    for args in [
        r#"{"url": ""}"#,
        r#"{"nope": 1}"#,
        r#"{"url": 3}"#,
        "not json",
    ] {
        assert!(parse_url_argument(args).is_none(), "{args}");
    }
}

#[tokio::test]
async fn a_refused_fetch_is_an_error_pair_and_the_executor_is_told() {
    let mut tool = WebFetchTool::new(&WebFetchConfig::default(), &ServerToolOptions::default());

    let answer = tool.answer(&call(REFUSED)).await;

    let blocks = &answer.served.blocks;
    assert_eq!(blocks.len(), 2);
    let (use_block, result) = (raw(&blocks[0]), raw(&blocks[1]));
    assert_eq!(use_block["type"], "server_tool_use");
    assert_eq!(use_block["name"], "web_fetch");
    assert_eq!(use_block["input"]["url"], "http://127.0.0.1:9/page");
    assert_eq!(result["type"], "web_fetch_tool_result");
    assert_eq!(result["tool_use_id"], use_block["id"]);
    assert_eq!(result["content"]["type"], "web_fetch_tool_result_error");
    assert_eq!(result["content"]["error_code"], "url_not_allowed");
    assert_eq!(answer.served.output, FETCH_FAILED);
    assert_eq!(answer.failure.map(|f| f.0), Some("url_not_allowed"));
}

#[tokio::test]
async fn a_call_with_no_url_is_answered_not_skipped_and_is_not_a_fetch() {
    let mut tool = WebFetchTool::new(&WebFetchConfig::default(), &ServerToolOptions::default());

    let answer = tool.answer(&call(r#"{"nope":1}"#)).await;

    let blocks = &answer.served.blocks;
    assert_eq!(blocks.len(), 2);
    assert_eq!(
        raw(&blocks[1])["content"]["error_code"],
        "invalid_tool_input"
    );
    assert_eq!(answer.served.output, INVALID_INPUT);
    assert!(answer.failure.is_none());
    assert_eq!(fetch_usage(&tool), None, "nothing was fetched");
}

#[tokio::test]
async fn usage_counts_every_fetch_attempted_even_failed_ones() {
    let mut tool = WebFetchTool::new(&WebFetchConfig::default(), &ServerToolOptions::default());
    assert_eq!(fetch_usage(&tool), None);

    tool.answer(&call(REFUSED)).await;
    tool.answer(&call(REFUSED)).await;

    assert_eq!(fetch_usage(&tool), Some(2));
}

#[tokio::test]
async fn only_the_tenth_fetch_is_flagged_as_a_runaway() {
    let mut tool = WebFetchTool::new(&WebFetchConfig::default(), &ServerToolOptions::default());
    let mut flagged = Vec::new();
    for n in 1..=12 {
        if tool.answer(&call(REFUSED)).await.runaway {
            flagged.push(n);
        }
    }
    assert_eq!(flagged, vec![10], "warn once, never cap");
    assert_eq!(fetch_usage(&tool), Some(12), "nothing is refused");
}

fn options(json: serde_json::Value) -> ServerToolOptions {
    serde_json::from_value(json).unwrap()
}

#[tokio::test]
async fn max_uses_refuses_the_fetch_over_the_limit_without_counting_it() {
    let mut tool = WebFetchTool::new(
        &WebFetchConfig::default(),
        &options(serde_json::json!({"max_uses": 1})),
    );

    tool.answer(&call(REFUSED)).await;
    let answer = tool.answer(&call(REFUSED)).await;

    let result = raw(&answer.served.blocks[1]);
    assert_eq!(result["content"]["error_code"], "max_uses_exceeded");
    assert_eq!(answer.served.output, LIMIT_REACHED);
    assert!(answer.failure.is_none());
    assert_eq!(
        fetch_usage(&tool),
        Some(1),
        "the refused call is not a fetch"
    );
}

#[tokio::test]
async fn without_a_client_max_uses_fetches_are_unlimited() {
    let mut tool = WebFetchTool::new(&WebFetchConfig::default(), &ServerToolOptions::default());
    for _ in 0..15 {
        let answer = tool.answer(&call(REFUSED)).await;
        assert_ne!(
            raw(&answer.served.blocks[1])["content"]["error_code"],
            "max_uses_exceeded"
        );
    }
}

#[tokio::test]
async fn a_url_outside_allowed_domains_is_refused_before_any_network_access() {
    let mut tool = WebFetchTool::new(
        &WebFetchConfig::default(),
        &options(serde_json::json!({"allowed_domains": ["docs.rs"]})),
    );

    // `.invalid` never resolves: only the domain rule can explain this code.
    let answer = tool
        .answer(&call(r#"{"url":"https://other.invalid/page"}"#))
        .await;

    assert_eq!(
        raw(&answer.served.blocks[1])["content"]["error_code"],
        "url_not_allowed"
    );
    assert_eq!(answer.failure.map(|f| f.0), Some("url_not_allowed"));
}

#[tokio::test]
async fn a_blocked_domain_is_refused() {
    let mut tool = WebFetchTool::new(
        &WebFetchConfig::default(),
        &options(serde_json::json!({"blocked_domains": ["blocked.invalid"]})),
    );
    let answer = tool
        .answer(&call(r#"{"url":"https://www.blocked.invalid/x"}"#))
        .await;
    assert_eq!(answer.failure.map(|f| f.0), Some("url_not_allowed"));
}

#[test]
fn max_content_tokens_tightens_the_content_budget() {
    let tool = WebFetchTool::new(
        &WebFetchConfig::default(),
        &options(serde_json::json!({"max_content_tokens": 100})),
    );
    assert_eq!(tool.cfg.max_content_bytes, Some(400));
}

#[test]
fn max_content_tokens_never_loosens_the_operators_own_cap() {
    let own = WebFetchConfig {
        max_content_bytes: Some(50),
        ..WebFetchConfig::default()
    };
    let tool = WebFetchTool::new(
        &own,
        &options(serde_json::json!({"max_content_tokens": 100})),
    );
    assert_eq!(tool.cfg.max_content_bytes, Some(50));
}

#[test]
fn no_max_content_tokens_leaves_the_budget_alone() {
    let tool = WebFetchTool::new(&WebFetchConfig::default(), &ServerToolOptions::default());
    assert_eq!(
        tool.cfg.max_content_bytes,
        WebFetchConfig::default().max_content_bytes
    );
}

#[tokio::test]
async fn the_tool_is_exhausted_once_a_call_is_refused_at_max_uses() {
    let mut tool = WebFetchTool::new(
        &WebFetchConfig::default(),
        &options(serde_json::json!({"max_uses": 1})),
    );
    assert!(!tool.exhausted());

    tool.answer(&call(REFUSED)).await;
    assert!(
        !tool.exhausted(),
        "reaching the limit is not a refusal: the executor has not been told yet"
    );

    tool.answer(&call(REFUSED)).await;
    assert!(
        tool.exhausted(),
        "after the refusal, further calls are dropped so the turn can end"
    );
}

#[tokio::test]
async fn without_max_uses_the_tool_is_never_exhausted() {
    let mut tool = WebFetchTool::new(&WebFetchConfig::default(), &ServerToolOptions::default());
    for _ in 0..3 {
        tool.answer(&call(REFUSED)).await;
    }
    assert!(!tool.exhausted());
}
