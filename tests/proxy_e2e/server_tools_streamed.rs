//! Streamed executor runs and live streaming of
//! server-tool turns.

use crate::harness::*;

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

/// The streaming mirror of the client's own `WebSearch` in a server-tool turn:
/// one `tool_use` block, no invented search.
#[tokio::test]
async fn a_streamed_client_websearch_call_is_a_plain_tool_use() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    let run = sse_call(
        0,
        "WebSearch",
        "call_ws",
        &serde_json::json!({"query": "rust"}),
    );
    script(
        &upstream,
        vec![ok_response(sse_run("r1", &run, "completed"))],
    );
    let (engine, _) = start_fake_search_engine(FAKE_RESULTS_HTML).await;
    let proxy_addr = start_proxy(all_server_tools_config(upstream_addr, engine)).await;

    let client_decl = r#"{"name":"WebSearch","input_schema":{"type":"object",
        "properties":{"query":{"type":"string"}},"required":["query"]}}"#;
    let search_decl = r#"{"type":"web_search_20250305","name":"web_search"}"#;
    let request = server_tools_request(true, &[client_decl, search_decl]);
    let res = post_messages(proxy_addr, request).await;
    assert_eq!(res.status(), StatusCode::OK);
    let events = anthropic_events(&res.text().await.unwrap());

    assert_eq!(assert_well_formed_blocks(&events), 1, "{events:?}");
    let block = events
        .iter()
        .find(|(kind, _)| kind == "content_block_start")
        .map(|(_, v)| &v["content_block"])
        .expect("a content block");
    assert_eq!(block["type"], "tool_use");
    assert_eq!(block["name"], "WebSearch");
    let delta = events
        .iter()
        .find(|(kind, _)| kind == "message_delta")
        .map(|(_, v)| v)
        .expect("message_delta");
    assert_eq!(delta["delta"]["stop_reason"], "tool_use");
    assert!(delta["usage"]["server_tool_use"].is_null(), "{delta}");
}
