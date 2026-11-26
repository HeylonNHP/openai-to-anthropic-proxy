//! Client-issued search options, usage accounting, the
//! round limit, and pause_turn.

use crate::harness::*;

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
