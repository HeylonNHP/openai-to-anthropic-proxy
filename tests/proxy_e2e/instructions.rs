//! Placement of the mid-conversation system message
//! (instructions vs inline).

use crate::harness::*;

// ── System messages inside the conversation ─────────────────────────────

/// Claude Code sends `role: "system"` messages inside `messages` (the
/// environment block, "the user hasn't heard from you", hook output).
fn request_with_mid_conversation_system() -> &'static str {
    r#"{"model":"gpt-4o-mini","max_tokens":16,"system":"You are helpful.",
        "messages":[
          {"role":"user","content":"hello"},
          {"role":"system","content":[{"type":"text","text":"the user has not heard from you"},
                                       {"type":"tool_addition","tool":{"type":"tool_reference","name":"x"}}]},
          {"role":"user","content":"next"}]}"#
}

async fn upstream_body_for_mid_conversation_system(
    placement: openai_to_anthropic_proxy::config::SystemMessagePlacement,
) -> serde_json::Value {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned.lock().await = Some(completed_response_with_id("resp_ok", "ok"));
    let mut config = (*make_proxy_config(upstream_addr)).clone();
    config.system_message_placement = placement;
    let proxy_addr = start_proxy(Arc::new(config)).await;
    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(request_with_mid_conversation_system()))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let sent = upstream.received_all.lock().await.clone();
    serde_json::from_slice(&sent[0]).unwrap()
}

#[tokio::test]
async fn instructions_placement_sends_a_mid_conversation_system_message_to_the_instructions() {
    use openai_to_anthropic_proxy::config::SystemMessagePlacement;
    let body =
        upstream_body_for_mid_conversation_system(SystemMessagePlacement::Instructions).await;
    assert!(
        body["instructions"]
            .as_str()
            .unwrap()
            .contains("the user has not heard from you"),
        "{body}"
    );
    assert_eq!(body["input"].as_array().unwrap().len(), 2, "{body}");
}

#[tokio::test]
async fn inline_placement_sends_the_system_message_in_the_conversation() {
    use openai_to_anthropic_proxy::config::SystemMessagePlacement;
    let body = upstream_body_for_mid_conversation_system(SystemMessagePlacement::Inline).await;
    assert_eq!(body["instructions"], "You are helpful.", "{body}");
    let input = body["input"].as_array().unwrap();
    assert_eq!(input.len(), 3, "{body}");
    assert_eq!(input[1]["role"], "system");
    assert_eq!(
        input[1]["content"][0]["text"],
        "the user has not heard from you"
    );
    assert_eq!(input[2]["role"], "user");
}

/// With no setting at all, a mid-conversation system message stays in place.
#[tokio::test]
async fn a_mid_conversation_system_message_stays_in_place_by_default() {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned.lock().await = Some(completed_response_with_id("resp_ok", "ok"));
    let proxy_addr = start_proxy(make_proxy_config(upstream_addr)).await;
    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(ReqBody::from(request_with_mid_conversation_system()))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let sent = upstream.received_all.lock().await.clone();
    let body: serde_json::Value = serde_json::from_slice(&sent[0]).unwrap();
    assert_eq!(body["instructions"], "You are helpful.", "{body}");
    assert_eq!(body["input"][1]["role"], "system", "{body}");
}
