//! The proxy-native web_search server tool (mirrors
//! `src/tools/web_search`).

use crate::harness::*;

const FAKE_CAPTCHA_HTML: &str = r#"<form id="challenge-form"></form>"#;

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

/// Regression: the proxy used to mint `server_tool_use` ids of the form
/// `stoolu_web_search_01` when the upstream ran its own built-in web
/// search. Anthropic's API constrains a `server_tool_use` id to
/// `^srvtoolu_[a-zA-Z0-9_]+$` and validates it on INBOUND history, so a
/// conversation recorded through the proxy could not be resumed against
/// a real Anthropic subscription: the replayed history was rejected with
/// `messages.N.content.M.server_tool_use.id: String should match
/// pattern ^srvtoolu_[a-zA-Z0-9_-]+$`.
///
/// This drives the real path: an upstream that reports a `web_search_call`
/// output item, through the proxy, to a client. The `server_tool_use`
/// block the client receives must carry an id Anthropic will accept.
#[tokio::test]
async fn upstream_web_search_yields_a_server_tool_use_id_anthropic_accepts() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned.lock().await = Some(
        r#"{
            "id": "resp_abc",
            "object": "response",
            "created_at": 1,
            "status": "completed",
            "model": "gpt-4o",
            "output": [
                {
                    "type": "web_search_call",
                    "id": "ws_01",
                    "status": "completed"
                },
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [
                        {"type": "output_text", "text": "Found it.", "annotations": []}
                    ]
                }
            ],
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
                "messages": [{"role": "user", "content": "search for something"}]
            }"#,
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = res.json().await.unwrap();

    // The upstream's search is dressed up as a `server_tool_use` block so
    // Claude Code's search counter registers it.
    let use_block = body["content"]
        .as_array()
        .expect("content is an array")
        .iter()
        .find(|b| b["type"] == "server_tool_use")
        .expect("expected a server_tool_use block")
        .clone();

    let id = use_block["id"].as_str().expect("server_tool_use has an id");
    assert!(
        openai_to_anthropic_proxy::server_tool_id::is_server_tool_id(id),
        "server_tool_use id must match Anthropic's pattern \
         ^srvtoolu_[a-zA-Z0-9_]+$: {id}"
    );

    // The paired result block must reference the same id, or Anthropic
    // cannot pair the two.
    let result = body["content"]
        .as_array()
        .expect("content is an array")
        .iter()
        .find(|b| b["type"] == "web_search_tool_result")
        .expect("expected a web_search_tool_result block")
        .clone();
    assert_eq!(result["tool_use_id"], id);
}
