//! Tool-result image handling: array form, lift/drop modes, size.

use crate::harness::*;

/// Anthropic request whose single user message carries a tool_result with
/// an `image` block — the shape Claude Code emits when a tool returns a
/// screenshot (e.g. a browser/computer-use tool).
fn request_with_image_tool_result() -> String {
    r#"{
        "model":"claude-sonnet-5",
        "max_tokens":16,
        "stream":false,
        "messages":[
            {"role":"user","content":[
                {"type":"tool_result","tool_use_id":"toolu_01","content":[
                    {"type":"text","text":"screenshot attached"},
                    {"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgo="}}
                ]}
            ]}
        ]
    }"#
    .to_string()
}

async fn setup_with_images(
    mode: Option<ToolResultImages>,
) -> (SocketAddr, SocketAddr, FakeUpstream) {
    let (upstream_addr, upstream) = start_fake_upstream().await;
    *upstream.canned.lock().await = Some(CANNED_RESPONSE.to_string());
    let base = make_proxy_config(upstream_addr);
    let config = match mode {
        Some(m) => {
            let mut c = (*base).clone();
            c.tool_result_images = m;
            Arc::new(c)
        }
        None => base,
    };
    let proxy_addr = start_proxy(config).await;
    (proxy_addr, upstream_addr, upstream)
}

#[tokio::test]
async fn tool_result_image_reaches_upstream_as_array_form() {
    let (proxy_addr, _upstream_addr, upstream) = setup_with_images(None).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(request_with_image_tool_result())
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        *upstream.request_count.lock().await,
        1,
        "proxy must not retry"
    );

    let sent = upstream
        .received
        .lock()
        .await
        .clone()
        .expect("upstream was not called");
    let body: serde_json::Value = serde_json::from_slice(&sent).unwrap();
    let items = body["input"].as_array().expect("input is an array");
    let fco = items
        .iter()
        .find(|i| i["type"] == "function_call_output")
        .expect("expected a function_call_output item");
    // Default mode: the output is the array-of-parts form with an
    // input_image part carrying the data URI.
    let output = fco["output"].as_array().expect("output must be an array");
    let parts: Vec<&str> = output.iter().map(|p| p["type"].as_str().unwrap()).collect();
    assert_eq!(parts, ["input_text", "input_image"]);
    assert_eq!(output[0]["text"], "screenshot attached");
    assert_eq!(output[1]["image_url"], "data:image/png;base64,iVBORw0KGgo=");
}

#[tokio::test]
async fn tool_result_image_lift_mode_sends_follow_up_user_message() {
    let (proxy_addr, _upstream_addr, upstream) =
        setup_with_images(Some(ToolResultImages::Lift)).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(request_with_image_tool_result())
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        *upstream.request_count.lock().await,
        1,
        "proxy must not retry"
    );

    let sent = upstream
        .received
        .lock()
        .await
        .clone()
        .expect("upstream was not called");
    let body: serde_json::Value = serde_json::from_slice(&sent).unwrap();
    let items = body["input"].as_array().unwrap();
    let fco_pos = items
        .iter()
        .position(|i| i["type"] == "function_call_output")
        .expect("expected a function_call_output item");
    assert_eq!(items[fco_pos]["output"], "screenshot attached");
    // The image must be forwarded in a user message immediately after the
    // tool output.
    let lifted = &items[fco_pos + 1];
    assert_eq!(lifted["type"], "message");
    assert_eq!(lifted["role"], "user");
    // First part labels which tool call the image came from.
    assert_eq!(lifted["content"][0]["type"], "input_text");
    assert_eq!(lifted["content"][1]["type"], "input_image");
    assert_eq!(
        lifted["content"][1]["image_url"],
        "data:image/png;base64,iVBORw0KGgo="
    );
}

#[tokio::test]
async fn tool_result_image_drop_mode_keeps_text_only() {
    let (proxy_addr, _upstream_addr, upstream) =
        setup_with_images(Some(ToolResultImages::Drop)).await;

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(request_with_image_tool_result())
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        *upstream.request_count.lock().await,
        1,
        "proxy must not retry"
    );

    let sent = upstream
        .received
        .lock()
        .await
        .clone()
        .expect("upstream was not called");
    let body: serde_json::Value = serde_json::from_slice(&sent).unwrap();
    let text = String::from_utf8_lossy(&sent);
    assert!(
        !text.contains("input_image"),
        "drop mode must not emit any image part: {text}"
    );
    let items = body["input"].as_array().unwrap();
    let fco = items
        .iter()
        .find(|i| i["type"] == "function_call_output")
        .unwrap();
    assert_eq!(fco["output"], "screenshot attached");
}

#[tokio::test]
async fn large_tool_result_image_is_accepted() {
    let (proxy_addr, _upstream_addr, upstream) = setup_with_images(None).await;

    // ~6 MB of base64 (~4.5 MB of binary image data).
    let big_b64 = "A".repeat(6 * 1024 * 1024);
    let body = format!(
        r#"{{"model":"claude-sonnet-5","max_tokens":16,"stream":false,"messages":[
            {{"role":"user","content":[
                {{"type":"tool_result","tool_use_id":"toolu_big","content":[
                    {{"type":"text","text":"screenshot attached"}},
                    {{"type":"image","source":{{"type":"base64","media_type":"image/png","data":"{big_b64}"}}}}
                ]}}
            ]}}
        ]}}"#
    );

    let res = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/v1/messages"))
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let sent = upstream
        .received
        .lock()
        .await
        .clone()
        .expect("upstream was not called");
    let text = String::from_utf8_lossy(&sent);
    assert!(
        text.contains("\"input_image\""),
        "image part must reach the upstream"
    );
}
