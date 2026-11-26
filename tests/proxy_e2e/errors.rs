//! Upstream status/error mapping into Anthropic error envelopes,
//! refusals, and truncated tool calls.

use crate::harness::*;

// ── upstream overload and rate-limit errors ─────────────

/// Send a non-streaming request to a proxy whose upstream answers with the
/// given error; return the proxy's response and the upstream's request count.
async fn post_with_upstream_error(
    status: u16,
    body: &str,
    headers: &[(&str, &str)],
) -> (reqwest::Response, usize) {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned_error.lock().await =
        Some((StatusCode::from_u16(status).unwrap(), body.to_owned()));
    *upstream.canned_error_headers.lock().await = headers
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    let proxy_addr = start_proxy(make_proxy_config(upstream_addr)).await;
    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(
            r#"{"model":"gpt-4o","max_tokens":16,"messages":[{"role":"user","content":"x"}]}"#,
        ))
        .send()
        .await
        .unwrap();
    let count = *upstream.request_count.lock().await;
    (res, count)
}

#[tokio::test]
async fn a_429_keeps_its_status_type_and_retry_after_and_is_not_retried() {
    let (res, count) = post_with_upstream_error(
        429,
        r#"{"error":{"message":"slow down","type":"rate_limit_error"}}"#,
        &[("retry-after", "7")],
    )
    .await;
    assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(res.headers()["retry-after"], "7");
    assert_eq!(res.headers()["retry-after-ms"], "7000");
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["error"]["type"], "rate_limit_error");
    assert_eq!(count, 1, "the proxy must leave retrying to the client");
}

#[tokio::test]
async fn overload_statuses_are_reported_as_overloaded_error() {
    for status in [503, 529] {
        let (res, _) =
            post_with_upstream_error(status, r#"{"error":{"message":"busy"}}"#, &[]).await;
        assert_eq!(res.status().as_u16(), status);
        assert!(!res.headers().contains_key("retry-after"));
        let body: serde_json::Value = res.json().await.unwrap();
        assert_eq!(body["error"]["type"], "overloaded_error", "{status}");
    }
}

#[tokio::test]
async fn a_403_is_a_permission_error() {
    let (res, _) = post_with_upstream_error(403, r#"{"error":{"message":"no"}}"#, &[]).await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["error"]["type"], "permission_error");
}

#[tokio::test]
async fn a_tokens_per_minute_429_stays_a_rate_limit_not_an_overflow() {
    let (res, count) = post_with_upstream_error(
        429,
        r#"{"error":{"message":"Request too large for gpt-4: tokens per min (TPM): Limit 30000, Requested 45000","code":"rate_limit_exceeded"}}"#,
        &[("retry-after", "2")],
    )
    .await;
    assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["error"]["type"], "rate_limit_error");
    assert!(
        !body["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("prompt is too long")
    );
    assert_eq!(count, 1);
}

#[tokio::test]
async fn a_stream_error_event_gets_the_type_its_code_names() {
    for (code, kind) in [
        ("rate_limit_exceeded", "rate_limit_error"),
        ("server_is_overloaded", "overloaded_error"),
        ("server_error", "api_error"),
    ] {
        let (upstream_addr, upstream) = start_fake_upstream().await;
        let sse = format!(
            r#"event: response.created
data: {{"type":"response.created","response":{{"id":"resp_1","object":"response","created_at":0,"status":"in_progress","model":"gpt-4o","output":[]}}}}

event: error
data: {{"type":"error","code":"{code}","message":"upstream trouble"}}

"#
        );
        *upstream.canned_stream.lock().await = Some(sse);
        let proxy_addr = start_proxy(make_proxy_config(upstream_addr)).await;
        let text = reqwest::Client::new()
            .post(format!("http://{proxy_addr}/v1/messages"))
            .header("content-type", "application/json")
            .body(ReqBody::from(
                r#"{"model":"gpt-4o","max_tokens":16,"stream":true,"messages":[{"role":"user","content":"x"}]}"#,
            ))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(
            text.contains(&format!(r#""type":"{kind}""#)),
            "{code}: {text}"
        );
    }
}

/// POST a plain buffered request to a proxy whose upstream answers `canned`.
async fn post_to_canned_upstream(canned: String) -> reqwest::Response {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned.lock().await = Some(canned);
    let proxy_addr = start_proxy(make_proxy_config(upstream_addr)).await;
    reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(
            r#"{"model":"gpt-4o","max_tokens":64,"messages":[{"role":"user","content":"hi"}]}"#,
        ))
        .send()
        .await
        .unwrap()
}

async fn error_of(res: reqwest::Response) -> (u16, serde_json::Value) {
    let status = res.status().as_u16();
    (status, res.json().await.unwrap())
}

#[tokio::test]
async fn a_failed_200_is_a_real_error_not_a_message() {
    let res = post_to_canned_upstream(failed_reply(
        Some(r#"{"code":"server_error","message":"boom"}"#),
        None,
    ))
    .await;
    let (status, body) = error_of(res).await;
    assert_eq!(status, 502);
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "api_error");
    assert!(body["error"]["message"].as_str().unwrap().contains("boom"));
}

#[tokio::test]
async fn a_failed_200_rate_limit_is_a_429_the_client_can_retry() {
    let res = post_to_canned_upstream(failed_reply(
        Some(r#"{"code":"rate_limit_exceeded","message":"slow down"}"#),
        None,
    ))
    .await;
    let (status, body) = error_of(res).await;
    assert_eq!(status, 429);
    assert_eq!(body["error"]["type"], "rate_limit_error");
}

#[tokio::test]
async fn a_failed_200_overload_is_a_529() {
    let res = post_to_canned_upstream(failed_reply(
        Some(r#"{"code":"server_overloaded","message":"busy"}"#),
        None,
    ))
    .await;
    let (status, body) = error_of(res).await;
    assert_eq!(status, 529);
    assert_eq!(body["error"]["type"], "overloaded_error");
}

#[tokio::test]
async fn a_failed_200_context_overflow_is_anthropics_prompt_too_long() {
    let res = post_to_canned_upstream(failed_reply(
        Some(
            r#"{"code":"context_length_exceeded",
                "message":"The prompt is too long: 10, model maximum context length: 5"}"#,
        ),
        None,
    ))
    .await;
    let (status, body) = error_of(res).await;
    assert_eq!(status, 400);
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert_eq!(
        body["error"]["message"],
        "prompt is too long: 10 tokens > 5 maximum"
    );
}

#[tokio::test]
async fn a_failed_200_with_no_error_object_is_a_generic_502() {
    let (status, body) = error_of(post_to_canned_upstream(failed_reply(None, None)).await).await;
    assert_eq!(status, 502);
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("upstream response failed")
    );
}

#[tokio::test]
async fn a_failed_200_with_partial_output_is_still_an_error() {
    // Streaming reports an error even after some text, and so does this.
    let res = post_to_canned_upstream(failed_reply(
        Some(r#"{"code":"server_error","message":"cut off"}"#),
        Some("half an ans"),
    ))
    .await;
    assert_eq!(res.status().as_u16(), 502);
}

#[tokio::test]
async fn an_incomplete_200_is_still_a_message() {
    // Only `failed` is an error: a reply cut off at max_output_tokens stays a
    // normal message with `stop_reason: max_tokens`.
    let body = r#"{"id":"resp_i","object":"response","created_at":1,
        "status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},
        "model":"any","output":[{"type":"message","role":"assistant",
            "content":[{"type":"output_text","text":"partial","annotations":[]}]}],
        "usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}"#;
    let res = post_to_canned_upstream(body.to_owned()).await;
    assert_eq!(res.status().as_u16(), 200);
    let json: serde_json::Value = res.json().await.unwrap();
    assert_eq!(json["stop_reason"], "max_tokens");
}

/// A tool call the upstream cut short (it ran out of output tokens)
/// keeps the parameters that were finished and reports
/// `stop_reason: max_tokens`. Anthropic's own API buffers each
/// parameter and drops the one that was still being written, so the
/// client sees the same shape here — not `{}`, which would look like a
/// valid empty call.
///
/// The proxy also does not spend a corrective round-trip on it: the
/// arguments did not break the schema, the output cap ended the call,
/// and re-asking with the same cap truncates the same way.
#[tokio::test]
async fn a_truncated_tool_call_keeps_its_finished_parameters() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned.lock().await = Some(
        r#"{
            "id": "resp_cut",
            "object": "response",
            "created_at": 1,
            "status": "incomplete",
            "incomplete_details": {"reason": "max_output_tokens"},
            "model": "gpt-4o",
            "output": [{
                "type": "function_call",
                "id": "fc_1",
                "status": "incomplete",
                "call_id": "call_1",
                "name": "Workflow",
                "arguments": "{\"script\":\"export const meta = {}\",\"resumeFromRunId\":\"half a "
            }],
            "usage": {"input_tokens": 5, "output_tokens": 60, "total_tokens": 65}
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

    // The parameter that was finished survives; the one that was being
    // written when the cap hit is dropped.
    assert_eq!(
        body["content"][0]["input"],
        serde_json::json!({"script": "export const meta = {}"})
    );
    // The stop reason is what tells the client the call is incomplete.
    assert_eq!(body["stop_reason"], "max_tokens");

    // No corrective round-trip: the upstream was asked exactly once.
    let sent = upstream.received_all.lock().await.clone();
    assert_eq!(sent.len(), 1, "a truncated reply is not retried");
}

/// The same, through the streaming path: the argument deltas are
/// buffered and shipped as one `input_json_delta` carrying the
/// parameters that were finished.
#[tokio::test]
async fn a_truncated_tool_call_streams_its_finished_parameters() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    let sse = [
        r#"event: response.created
data: {"type":"response.created","response":{"id":"resp_1","object":"response","created_at":0,"status":"in_progress","model":"gpt-4o","output":[]}}"#,
        r#"event: response.output_item.added
data: {"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_1","status":"in_progress","call_id":"call_1","name":"Workflow","arguments":""}}"#,
        r#"event: response.function_call_arguments.delta
data: {"type":"response.function_call_arguments.delta","item_id":"fc_1","output_index":0,"delta":"{\"script\":\"export const meta = {}\",\"resumeFromRunId\":\"half a "}"#,
        r#"event: response.incomplete
data: {"type":"response.incomplete","response":{"id":"resp_1","object":"response","created_at":0,"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"model":"gpt-4o","output":[{"type":"function_call","id":"fc_1","status":"incomplete","call_id":"call_1","name":"Workflow","arguments":"{\"script\":\"export const meta = {}\",\"resumeFromRunId\":\"half a "}],"usage":{"input_tokens":5,"output_tokens":60,"total_tokens":65}}}"#,
    ]
    .join("\n\n")
        + "\n\n";
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
    let text = res.text().await.unwrap();

    // Exactly one input_json_delta, carrying the salvaged blob.
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
    assert_eq!(
        parsed,
        serde_json::json!({"script": "export const meta = {}"})
    );

    // The stop reason is what tells the client the call is incomplete.
    let deltas: Vec<serde_json::Value> = text
        .split("\n\n")
        .filter(|s| s.contains("\"message_delta\""))
        .filter_map(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("data: "))
                .and_then(|d| serde_json::from_str(d).ok())
        })
        .collect();
    assert!(
        deltas
            .iter()
            .any(|d| d["delta"]["stop_reason"] == "max_tokens"),
        "expected stop_reason max_tokens in: {text}"
    );
}

// ─── a reply the upstream refused to give ───────────────────────────────────

/// A buffered reply the upstream declined: the message carries a `refusal`
/// content part, as OpenAI sends a model that declines to answer.
fn refusal_reply(refusal: &str) -> String {
    format!(
        r#"{{"id":"resp_r","object":"response","created_at":1,"status":"completed",
            "model":"any",
            "output":[{{"type":"message","role":"assistant",
                "content":[{{"type":"refusal","refusal":"{refusal}"}}]}}],
            "usage":{{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}}"#
    )
}

#[tokio::test]
async fn a_refused_reply_reports_the_refusal_stop_reason() {
    let res = post_to_canned_upstream(refusal_reply("I won't do that")).await;
    assert_eq!(res.status().as_u16(), 200);
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["stop_reason"], "refusal");
    // The refusal text still reaches the client, marked as a refusal.
    let text = body["content"][0]["text"].as_str().unwrap_or_default();
    assert!(text.starts_with("[refusal] "), "{text}");
    assert!(text.contains("I won't do that"), "{text}");
}

#[tokio::test]
async fn a_content_filtered_reply_reports_the_refusal_stop_reason() {
    // The other way an upstream declines: generation stopped by a safety
    // filter, so the reply is `incomplete` with that reason.
    let body = r#"{"id":"resp_c","object":"response","created_at":1,
        "status":"incomplete","incomplete_details":{"reason":"content_filter"},
        "model":"any","output":[],
        "usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}"#;
    let res = post_to_canned_upstream(body.to_owned()).await;
    assert_eq!(res.status().as_u16(), 200);
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["stop_reason"], "refusal");
}
