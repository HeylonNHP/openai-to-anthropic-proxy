use super::*;

#[test]
fn function_call_output_serializes_text_as_a_bare_string() {
    let item = InputItem::FunctionCallOutput {
        call_id: "call_1".into(),
        output: FunctionCallOutputBody::from("72F sunny".to_string()),
    };
    let json = serde_json::to_value(&item).unwrap();
    assert_eq!(json["output"], "72F sunny");
}

#[test]
fn function_call_output_serializes_parts_as_an_array() {
    let item = InputItem::FunctionCallOutput {
        call_id: "call_1".into(),
        output: FunctionCallOutputBody::from(vec![
            InputContentPart::InputText {
                text: "see image".into(),
                prompt_cache_breakpoint: None,
            },
            InputContentPart::InputImage {
                image_url: "data:image/png;base64,AAAA".into(),
                detail: None,
                prompt_cache_breakpoint: None,
            },
        ]),
    };
    let json = serde_json::to_value(&item).unwrap();
    assert_eq!(json["output"][0]["type"], "input_text");
    assert_eq!(json["output"][0]["text"], "see image");
    assert_eq!(json["output"][1]["type"], "input_image");
    assert_eq!(json["output"][1]["image_url"], "data:image/png;base64,AAAA");
    assert!(json["output"][1].get("detail").is_none());
}

#[test]
fn function_call_output_round_trips_both_forms() {
    let text = InputItem::FunctionCallOutput {
        call_id: "c".into(),
        output: FunctionCallOutputBody::Text("hi".into()),
    };
    let back: InputItem = serde_json::from_str(&serde_json::to_string(&text).unwrap()).unwrap();
    match back {
        InputItem::FunctionCallOutput { output, .. } => {
            assert_eq!(output, FunctionCallOutputBody::Text("hi".into()))
        }
        _ => panic!("wrong item type"),
    }

    let parts = InputItem::FunctionCallOutput {
        call_id: "c".into(),
        output: FunctionCallOutputBody::Parts(vec![InputContentPart::InputImage {
            image_url: "https://example.com/x.png".into(),
            detail: Some("low".into()),
            prompt_cache_breakpoint: None,
        }]),
    };
    let back: InputItem = serde_json::from_str(&serde_json::to_string(&parts).unwrap()).unwrap();
    match back {
        InputItem::FunctionCallOutput { output, .. } => match output {
            FunctionCallOutputBody::Parts(p) => {
                assert_eq!(p.len(), 1)
            }
            other => panic!("expected parts, got {other:?}"),
        },
        _ => panic!("wrong item type"),
    }
}

fn usage_with(input: u32, cached: u32, written: u32) -> ResponsesUsage {
    ResponsesUsage {
        input_tokens: input,
        output_tokens: 0,
        total_tokens: input,
        input_tokens_details: Some(InputTokensDetails {
            cached_tokens: cached,
            cache_write_tokens: written,
        }),
        output_tokens_details: None,
    }
}

#[test]
fn net_input_plus_cache_fields_equals_the_upstream_gross_total() {
    // Anthropic clients sum input + cache read + cache creation for the
    // context size, so the three must add back up to the upstream's figure.
    let u = usage_with(12_000, 4_000, 7_000);
    assert_eq!(u.net_input_tokens(), 1_000);
    assert_eq!(u.net_input_tokens() + 4_000 + 7_000, u.input_tokens);
}

#[test]
fn net_input_without_details_is_the_gross_total() {
    let mut u = usage_with(50, 0, 0);
    u.input_tokens_details = None;
    assert_eq!(u.net_input_tokens(), 50);
}

#[test]
fn net_input_saturates_when_cache_counts_exceed_the_total() {
    assert_eq!(usage_with(10, 8, 8).net_input_tokens(), 0);
    assert_eq!(net_input_tokens(5, u32::MAX, u32::MAX), 0);
}

// ─── B7: a terminal event whose response omits model/output ──────────────

/// A terminal `response.completed` with `model` and `output` absent.
#[test]
fn a_terminal_event_without_model_or_output_deserializes() {
    let json = r#"{
        "type": "response.completed",
        "response": {
            "id": "resp_1",
            "object": "response",
            "created_at": 0,
            "status": "completed"
        }
    }"#;
    let event: ResponsesStreamEvent = serde_json::from_str(json).expect("must not be dropped");
    let ResponsesStreamEvent::Completed { response } = event else {
        panic!("expected a completed event");
    };
    assert_eq!(response.model, "");
    assert!(response.output.is_empty());
}

/// The same event with explicit nulls: `#[serde(default)]` alone does
/// not cover this — serde rejects `null` for `String`/`Vec` outright.
#[test]
fn a_terminal_event_with_null_model_or_output_deserializes() {
    let json = r#"{
        "type": "response.completed",
        "response": {
            "id": "resp_1",
            "object": "response",
            "created_at": 0,
            "status": "completed",
            "model": null,
            "output": null
        }
    }"#;
    let event: ResponsesStreamEvent = serde_json::from_str(json).expect("must not be dropped");
    let ResponsesStreamEvent::Completed { response } = event else {
        panic!("expected a completed event");
    };
    assert_eq!(response.model, "");
    assert!(response.output.is_empty());
}

/// A well-formed terminal event still round-trips its real fields.
#[test]
fn a_normal_terminal_event_is_unaffected_by_the_defaults() {
    let json = r#"{
        "type": "response.completed",
        "response": {
            "id": "resp_1",
            "object": "response",
            "created_at": 0,
            "status": "completed",
            "model": "gpt-4o",
            "output": []
        }
    }"#;
    let event: ResponsesStreamEvent = serde_json::from_str(json).expect("must deserialize");
    let ResponsesStreamEvent::Completed { response } = event else {
        panic!("expected a completed event");
    };
    assert_eq!(response.model, "gpt-4o");
    assert!(response.output.is_empty());
}
