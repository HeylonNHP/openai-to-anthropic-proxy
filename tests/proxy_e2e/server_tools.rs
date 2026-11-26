//! The shared server-tool loop across advisor / web_search /
//! web_fetch, including mixed-tool turns.

use crate::harness::*;

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

const ADVISOR_DECL: &str = r#"{"type":"advisor_20260301","name":"advisor","model":"beam"}"#;

const FETCH_DECL: &str = r#"{"type":"web_fetch_20250910","name":"web_fetch"}"#;

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
