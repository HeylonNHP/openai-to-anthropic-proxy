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
