//! PDF and text document blocks.

use crate::harness::*;

// ── document blocks ─────────────────────────────────────

fn request_with_pdf() -> String {
    r#"{
        "model":"claude-sonnet-5",
        "max_tokens":16,
        "stream":false,
        "messages":[{"role":"user","content":[
            {"type":"text","text":"read this"},
            {"type":"document","title":"a.pdf","source":{"type":"base64","media_type":"application/pdf","data":"JVBERi0="}},
            {"type":"document","source":{"type":"text","media_type":"text/plain","data":"plain body"}}
        ]}]
    }"#
    .to_string()
}

async fn post_pdf(forward_documents: bool) -> Vec<serde_json::Value> {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned.lock().await = Some(CANNED_RESPONSE.to_string());
    let mut config = (*make_proxy_config(upstream_addr)).clone();
    config.forward_documents = forward_documents;
    let proxy_addr = start_proxy(Arc::new(config)).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(request_with_pdf())
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let sent = upstream.received.lock().await.clone().expect("not called");
    let body: serde_json::Value = serde_json::from_slice(&sent).unwrap();
    body["input"][0]["content"].as_array().unwrap().clone()
}

#[tokio::test]
async fn a_pdf_is_noted_and_a_text_document_forwarded_by_default() {
    let parts = post_pdf(false).await;
    let kinds: Vec<&str> = parts.iter().map(|p| p["type"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["input_text", "input_text", "input_text"]);
    assert_eq!(
        parts[1]["text"],
        "[attached document omitted: a.pdf (application/pdf)]"
    );
    assert!(parts[2]["text"].as_str().unwrap().ends_with("plain body"));
}

#[tokio::test]
async fn a_pdf_reaches_the_upstream_as_input_file_when_enabled() {
    let parts = post_pdf(true).await;
    assert_eq!(parts[1]["type"], "input_file");
    assert_eq!(parts[1]["filename"], "a.pdf");
    assert_eq!(
        parts[1]["file_data"],
        "data:application/pdf;base64,JVBERi0="
    );
}
