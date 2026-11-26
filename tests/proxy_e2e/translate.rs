//! Translation of tool declarations and echoed thinking blocks
//! to the upstream (mirrors `src/translate.rs`).

use crate::harness::*;

/// Tools in the outbound Responses request must include
/// `strict: true` and `additionalProperties: false` on the parameters
/// schema. Without these, the airia gateway returns 400
/// `invalid_function_parameters`. This is the headline behavior
/// change of the migration: the proxy bridges Claude Code's
/// lenient tools into the strict Responses shape.
#[tokio::test]
async fn tools_get_strict_and_additional_properties_false() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned.lock().await = Some(
        r#"{
            "id": "resp_ok",
            "object": "response",
            "created_at": 1,
            "status": "completed",
            "model": "gpt-4o",
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "ok", "annotations": []}]
            }],
            "usage": {"input_tokens":1,"output_tokens":1,"total_tokens":2}
        }"#
        .into(),
    );
    let config = make_proxy_config(upstream_addr);
    let proxy_addr = start_proxy(config).await;

    let client = reqwest::Client::new();
    let _ = client
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(
            r#"{
                "model": "gpt-4o",
                "max_tokens": 4,
                "messages": [{"role": "user", "content": "x"}],
                "tools": [{
                    "name": "get_weather",
                    "description": "Get the weather",
                    "input_schema": {
                        "type": "object",
                        "properties": {"location": {"type": "string"}}
                    }
                }]
            }"#,
        ))
        .send()
        .await
        .unwrap();

    let body = upstream.received.lock().await.clone().unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let tools = body["tools"].as_array().expect("tools is array");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["type"], "function");
    assert!(tools[0]["strict"].as_bool().unwrap());
    assert_eq!(
        tools[0]["parameters"]["additionalProperties"],
        serde_json::Value::Bool(false)
    );
    // Properties object is preserved (not overwritten) when already
    // present. `location` is NOT in the original schema's `required`,
    // so the strict encoding marks it nullable (`["string","null"]`)
    // while still listing it in `required` — the model signals
    // "omitted" with `null` and the proxy strips the sentinel on the
    // response side (src/repair.rs).
    assert_eq!(
        tools[0]["parameters"]["properties"],
        serde_json::json!({"location": {"type": ["string", "null"]}})
    );
    assert_eq!(
        tools[0]["parameters"]["required"],
        serde_json::json!(["location"])
    );
}

/// A client's echoed `thinking` block must reach the upstream. The proxy
/// used to drop it outright (no `Thinking` variant in the inbound enum),
/// which destroyed the model's memory of its own prior reasoning on every
/// turn — 10,467 drops on 2026-11-12 alone.
#[tokio::test]
async fn thinking_block_is_replayed_to_the_upstream() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned.lock().await = Some(
        r#"{
            "id": "resp_1",
            "object": "response",
            "created_at": 1,
            "status": "completed",
            "model": "gpt-4o",
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "ok", "annotations": []}]
            }],
            "usage": {"input_tokens":1,"output_tokens":1,"total_tokens":2}
        }"#
        .into(),
    );
    let config = make_proxy_config(upstream_addr);
    let proxy_addr = start_proxy(config).await;

    let client = reqwest::Client::new();
    let _ = client
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(
            r#"{
                "model": "gpt-4o",
                "max_tokens": 4,
                "messages": [
                    {"role": "user", "content": "x"},
                    {"role": "assistant", "content": [
                        {"type": "thinking", "thinking": "the model reasoned about this", "signature": "SIG"},
                        {"type": "text", "text": "the answer"}
                    ]}
                ]
            }"#,
        ))
        .send()
        .await
        .unwrap();

    let body = upstream.received.lock().await.clone().unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let input = body["input"].as_array().expect("input is array");
    let assistant = input
        .iter()
        .find(|i| i["role"] == "assistant")
        .expect("assistant message in the upstream body");
    let text = assistant["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("the model reasoned about this"),
        "the reasoning did not reach the upstream: {text:?}"
    );
    assert!(text.contains("the answer"));
    // Text mode must not introduce a new input-item type — only
    // `message` / `function_call` / `function_call_output` are ever sent,
    // because those are the only shapes proven to work against an
    // upstream we know nothing about.
    assert!(
        input.iter().all(|i| matches!(
            i["type"].as_str(),
            Some("message" | "function_call" | "function_call_output")
        )),
        "unexpected input item types: {input:?}"
    );
}

/// A wrong-typed field inside a thinking block must NOT fail the whole
/// request. Before the block was recognised it fell into `Unknown` and was
/// merely dropped; recognising it must not turn a tolerated block into a
/// 400 for the entire conversation.
#[tokio::test]
async fn a_wrong_typed_thinking_field_does_not_fail_the_request() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned.lock().await = Some(
        r#"{
            "id": "resp_1",
            "object": "response",
            "created_at": 1,
            "status": "completed",
            "model": "gpt-4o",
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "ok", "annotations": []}]
            }],
            "usage": {"input_tokens":1,"output_tokens":1,"total_tokens":2}
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
                "max_tokens": 4,
                "messages": [
                    {"role": "user", "content": "x"},
                    {"role": "assistant", "content": [
                        {"type": "thinking", "thinking": 123, "signature": "SIG"},
                        {"type": "text", "text": "the answer"}
                    ]}
                ]
            }"#,
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        StatusCode::OK,
        "a wrong-typed thinking field must not 400 the whole request"
    );
    // The upstream must have been called.
    assert!(
        upstream.received.lock().await.is_some(),
        "the upstream was never called"
    );
}

/// `reasoning_replay: "off"` restores the old behaviour: the thinking
/// block is dropped, and the upstream sees only the visible answer.
#[tokio::test]
async fn reasoning_replay_off_drops_the_thinking_block() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned.lock().await = Some(
        r#"{
            "id": "resp_1",
            "object": "response",
            "created_at": 1,
            "status": "completed",
            "model": "gpt-4o",
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "ok", "annotations": []}]
            }],
            "usage": {"input_tokens":1,"output_tokens":1,"total_tokens":2}
        }"#
        .into(),
    );
    let mut config = (*make_proxy_config(upstream_addr)).clone();
    config.reasoning_replay.default = openai_to_anthropic_proxy::config::ReasoningReplayMode::Off;
    let proxy_addr = start_proxy(Arc::new(config)).await;

    let client = reqwest::Client::new();
    let _ = client
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(
            r#"{
                "model": "gpt-4o",
                "max_tokens": 4,
                "messages": [
                    {"role": "user", "content": "x"},
                    {"role": "assistant", "content": [
                        {"type": "thinking", "thinking": "the model reasoned about this", "signature": "SIG"},
                        {"type": "text", "text": "the answer"}
                    ]}
                ]
            }"#,
        ))
        .send()
        .await
        .unwrap();

    let body = upstream.received.lock().await.clone().unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let input = body["input"].as_array().expect("input is array");
    let assistant = input
        .iter()
        .find(|i| i["role"] == "assistant")
        .expect("assistant message in the upstream body");
    let text = assistant["content"][0]["text"].as_str().unwrap();
    assert_eq!(text, "the answer");
    assert!(!text.contains("the model reasoned about this"));
}
