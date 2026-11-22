use super::*;
use crate::responses::ResponsesRequest;

fn translated(body: &str) -> ResponsesRequest {
    let req = parse_count_request(body.as_bytes()).expect("parses");
    crate::translate::anthropic_to_responses(&req, None, &Default::default(), &Default::default())
        .expect("translates")
        .0
}

#[test]
fn a_count_request_without_max_tokens_parses() {
    let body = br#"{"model":"claude-sonnet-5","messages":[{"role":"user","content":"hi"}]}"#;
    let req = parse_count_request(body).expect("parses");
    assert_eq!(req.model, "claude-sonnet-5");
    assert_eq!(req.max_tokens, 1);
}

#[test]
fn an_explicit_max_tokens_is_kept() {
    let body = br#"{"model":"m","max_tokens":77,"messages":[{"role":"user","content":"hi"}]}"#;
    assert_eq!(parse_count_request(body).unwrap().max_tokens, 77);
}

#[test]
fn a_bad_count_request_is_a_400() {
    for body in [&b"not json"[..], br#"{"model":"m"}"#] {
        assert!(matches!(
            parse_count_request(body),
            Err(AppError::BadRequest(_))
        ));
    }
}

#[test]
fn the_count_body_carries_only_input_fields() {
    let mut req = translated(
        r#"{"model":"m","system":"be brief","messages":[{"role":"user","content":"hello"}]}"#,
    );
    req.max_output_tokens = Some(99);
    req.stream = true;
    req.store = Some(false);
    req.user = Some("u".into());
    req.prompt_cache_key = Some("k".into());
    let value = serde_json::to_value(InputTokensRequest::from(&req)).unwrap();
    let keys: Vec<&str> = value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    for forbidden in [
        "stream",
        "max_output_tokens",
        "store",
        "user",
        "prompt_cache_key",
        "reasoning",
    ] {
        assert!(
            !keys.contains(&forbidden),
            "{forbidden} leaked into {keys:?}"
        );
    }
    assert_eq!(value["model"], "m");
    assert!(value["input"].is_array());
}

#[test]
fn text_is_estimated_at_four_characters_per_token_rounded_up() {
    // `input` key (5) + 7 characters of value = 12 characters -> 3 tokens.
    assert_eq!(estimate_tokens(&json!({"input": "abcdefg"})), 3);
    assert_eq!(estimate_tokens(&json!({"a": "bcd"})), 1);
    assert_eq!(estimate_tokens(&json!({"a": "bcde"})), 2);
}

#[test]
fn a_base64_image_costs_a_flat_amount_not_its_length() {
    let uri = format!("data:image/png;base64,{}", "A".repeat(400_000));
    assert_eq!(estimate_tokens(&json!([uri])), IMAGE_TOKENS);
    // A plain data URI without base64 payload is just text.
    assert!(estimate_tokens(&json!(["data:text/plain,hello"])) < IMAGE_TOKENS);
}

#[test]
fn tool_definitions_are_counted() {
    let without = estimate_tokens(&json!({"input": "hi"}));
    let with = estimate_tokens(&json!({
        "input": "hi",
        "tools": [{"type": "function", "name": "read_file", "description": "Read a file"}],
    }));
    assert!(with > without + 5);
}
