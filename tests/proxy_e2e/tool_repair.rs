//! Repair of tool inputs back to the original schema and the
//! corrective retry that follows.

use crate::harness::*;

/// The live regression: the upstream model obeys the *mutated* strict
/// schema and emits a fabricated value for an optional parameter
/// (`resumeFromRunId`-style, pattern-constrained) plus null sentinels.
/// The proxy must ship the client an input that validates against the
/// *original* schema: fabricated optional dropped, nulls stripped.
#[tokio::test]
async fn non_streaming_tool_input_is_repaired_to_original_schema() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned.lock().await = Some(
        r#"{
            "id": "resp_wf",
            "object": "response",
            "created_at": 1,
            "status": "completed",
            "model": "gpt-4o",
            "output": [{
                "type": "function_call",
                "id": "fc_1",
                "status": "completed",
                "call_id": "call_wf",
                "name": "Workflow",
                "arguments": "{\"script\":\"export const meta = {}\",\"resumeFromRunId\":\"not-a-real-run-id\",\"args\":null}"
            }],
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
                "messages": [{"role": "user", "content": "run it"}],
                "tools": [{
                    "name": "Workflow",
                    "description": "Run a workflow",
                    "input_schema": {
                        "type": "object",
                        "properties": {
                            "script": {"type": "string"},
                            "resumeFromRunId": {"type": "string", "pattern": "^wf_[a-z0-9-]{6,}$"},
                            "args": {"description": "any value"}
                        },
                        "required": ["script"]
                    }
                }]
            }"#,
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["stop_reason"], "tool_use");
    let input = &body["content"][0]["input"];
    // The fabricated optional value is gone; the null sentinel is gone;
    // the required value survives.
    assert_eq!(
        input,
        &serde_json::json!({"script": "export const meta = {}"})
    );
}

/// Streaming path: argument deltas are buffered and the repaired blob
/// is emitted as a single input_json_delta before content_block_stop.
#[tokio::test]
async fn streaming_tool_input_is_repaired_before_shipping() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    let sse = [
        r#"event: response.created
data: {"type":"response.created","response":{"id":"resp_1","object":"response","created_at":0,"status":"in_progress","model":"gpt-4o","output":[]}}"#,
        r#"event: response.output_item.added
data: {"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_1","status":"in_progress","call_id":"call_wf","name":"Workflow","arguments":""}}"#,
        r#"event: response.function_call_arguments.delta
data: {"type":"response.function_call_arguments.delta","item_id":"fc_1","output_index":0,"delta":"{\"script\":\"s\",\"resumeFromRunId\":\"bogus\""}"#,
        r#"event: response.function_call_arguments.delta
data: {"type":"response.function_call_arguments.delta","item_id":"fc_1","output_index":0,"delta":",\"args\":null}"}"#,
        r#"event: response.output_item.done
data: {"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc_1","status":"completed","call_id":"call_wf","name":"Workflow","arguments":"{\"script\":\"s\",\"resumeFromRunId\":\"bogus\",\"args\":null}"}}"#,
        r#"event: response.completed
data: {"type":"response.completed","response":{"id":"resp_1","object":"response","created_at":0,"status":"completed","model":"gpt-4o","output":[{"type":"function_call","id":"fc_1","status":"completed","call_id":"call_wf","name":"Workflow","arguments":"{\"script\":\"s\",\"resumeFromRunId\":\"bogus\",\"args\":null}"}],"usage":{"input_tokens":4,"output_tokens":2,"total_tokens":6}}}"#,
    ].join("\n\n") + "\n\n";
    *upstream.canned_stream.lock().await = Some(sse);

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
                "stream": true,
                "messages": [{"role": "user", "content": "run it"}],
                "tools": [{
                    "name": "Workflow",
                    "description": "Run a workflow",
                    "input_schema": {
                        "type": "object",
                        "properties": {
                            "script": {"type": "string"},
                            "resumeFromRunId": {"type": "string", "pattern": "^wf_[a-z0-9-]{6,}$"},
                            "args": {"description": "any value"}
                        },
                        "required": ["script"]
                    }
                }]
            }"#,
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);
    let text = res.text().await.unwrap();

    // Exactly one input_json_delta, carrying the repaired blob.
    let input_deltas: Vec<serde_json::Value> = text
        .split("\n\n")
        .filter(|s| s.contains("\"input_json_delta\""))
        .filter_map(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("data: "))
                .and_then(|d| serde_json::from_str(d).ok())
        })
        .collect();
    assert_eq!(input_deltas.len(), 1);
    let partial = input_deltas[0]["delta"]["partial_json"].as_str().unwrap();
    let parsed: serde_json::Value = serde_json::from_str(partial).unwrap();
    assert_eq!(parsed, serde_json::json!({"script": "s"}));

    // And the event ordering is well-formed: start, one delta, stop.
    let kinds: Vec<&str> = text
        .split("\n\n")
        .filter(|s| !s.is_empty())
        .filter_map(|e| e.lines().find_map(|l| l.strip_prefix("event: ")))
        .collect();
    assert_eq!(
        kinds,
        vec![
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop",
        ]
    );
}

/// Hidden validation retry: when the repaired tool input still violates
/// the original schema (a genuinely required parameter missing), the
/// proxy re-queries the upstream once with a corrective tool result
/// instead of shipping a call the client would reject.
#[tokio::test]
async fn invalid_tool_input_triggers_corrective_retry() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    // Attempt 1: model omits the required `script` parameter entirely.
    // Attempt 2: model complies.
    let mut per_attempt = std::collections::VecDeque::new();
    per_attempt.push_back(FakeResponse {
        status: StatusCode::OK,
        body: Some(
            r#"{
                "id": "resp_bad",
                "object": "response",
                "created_at": 1,
                "status": "completed",
                "model": "gpt-4o",
                "output": [{
                    "type": "function_call",
                    "id": "fc_1",
                    "status": "completed",
                    "call_id": "call_1",
                    "name": "Workflow",
                    "arguments": "{\"resumeFromRunId\":\"bogus\"}"
                }],
                "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
            }"#
            .into(),
        ),
    });
    per_attempt.push_back(FakeResponse {
        status: StatusCode::OK,
        body: Some(
            r#"{
                "id": "resp_good",
                "object": "response",
                "created_at": 1,
                "status": "completed",
                "model": "gpt-4o",
                "output": [{
                    "type": "function_call",
                    "id": "fc_2",
                    "status": "completed",
                    "call_id": "call_2",
                    "name": "Workflow",
                    "arguments": "{\"script\":\"export const meta = {}\"}"
                }],
                "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
            }"#
            .into(),
        ),
    });
    *upstream.canned_per_attempt.lock().await = per_attempt;

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
                "messages": [{"role": "user", "content": "run it"}],
                "tools": [{
                    "name": "Workflow",
                    "description": "Run a workflow",
                    "input_schema": {
                        "type": "object",
                        "properties": {
                            "script": {"type": "string"},
                            "resumeFromRunId": {"type": "string", "pattern": "^wf_[a-z0-9-]{6,}$"}
                        },
                        "required": ["script"]
                    }
                }]
            }"#,
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = res.json().await.unwrap();
    // The client sees the SECOND (valid) attempt's call.
    assert_eq!(body["content"][0]["id"], "call_2");
    assert_eq!(
        body["content"][0]["input"],
        serde_json::json!({"script": "export const meta = {}"})
    );

    // The upstream was queried twice; the second request carries the
    // original bad call plus a corrective tool result.
    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(sent.len(), 2);
    let second: serde_json::Value = serde_json::from_slice(&sent[1]).unwrap();
    let items = second["input"].as_array().unwrap();
    let has_corrective = items.iter().any(|i| {
        i["type"] == "function_call_output"
            && i["output"]
                .as_str()
                .is_some_and(|o| o.contains("[TOOL_ERROR]"))
    });
    assert!(
        has_corrective,
        "second request must carry a corrective tool result"
    );
}
