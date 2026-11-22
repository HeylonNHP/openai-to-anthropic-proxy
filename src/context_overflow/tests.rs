use super::*;

const GATEWAY: &str = r#"{"error":{"message":"The prompt is too long: 1424454, model maximum context length: 1048576","type":"invalid_request_error"}}"#;

fn message(status: u16, body: &str) -> Option<String> {
    detect(status, body).map(|o| o.anthropic_message())
}

#[test]
fn the_gateway_wording_becomes_anthropics() {
    assert_eq!(
        message(400, GATEWAY).as_deref(),
        Some("prompt is too long: 1424454 tokens > 1048576 maximum")
    );
}

#[test]
fn openai_wording_becomes_anthropics() {
    let body = r#"{"error":{"message":"This model's maximum context length is 128000 tokens. However, your messages resulted in 130000 tokens. Please reduce the length of the messages.","type":"invalid_request_error","code":"context_length_exceeded"}}"#;
    assert_eq!(
        message(400, body).as_deref(),
        Some("prompt is too long: 130000 tokens > 128000 maximum")
    );
}

#[test]
fn anthropics_own_wording_round_trips() {
    let body = r#"{"error":{"message":"prompt is too long: 213462 tokens > 200000 maximum"}}"#;
    assert_eq!(
        message(400, body).as_deref(),
        Some("prompt is too long: 213462 tokens > 200000 maximum")
    );
}

#[test]
fn a_strong_code_needs_no_matching_message_and_no_numbers() {
    let body = r#"{"error":{"code":"context_length_exceeded","message":"Input too big"}}"#;
    assert_eq!(message(400, body).as_deref(), Some("prompt is too long"));
}

#[test]
fn a_plain_text_body_and_the_flat_shape_are_recognised() {
    assert_eq!(
        message(400, "Your input is too long.").as_deref(),
        Some("prompt is too long")
    );
    let flat = r#"{"message":"x","code":"context_length_exceeded"}"#;
    assert!(detect(400, flat).is_some());
}

#[test]
fn thousands_separators_and_swapped_numbers_are_normalised() {
    let body = r#"{"error":{"message":"maximum context length is 8,192 tokens, you requested 9,000 tokens"}}"#;
    assert_eq!(
        message(400, body).as_deref(),
        Some("prompt is too long: 9000 tokens > 8192 maximum")
    );
    let swapped = "The prompt is too long: 5, model maximum context length: 10";
    assert_eq!(
        message(400, swapped).as_deref(),
        Some("prompt is too long: 10 tokens > 5 maximum")
    );
}

#[test]
fn unrelated_errors_are_not_overflows() {
    let cases = [
        (
            400,
            "max_tokens is too large: 99999. This model supports at most 16384 completion tokens",
        ),
        (400, "image exceeds 5 MB maximum"),
        (400, "tool name too long"),
        (400, "string too long"),
        (413, "request too large: body exceeds 10 MB"),
        (413, "Request Entity Too Large"),
        (
            429,
            "Request too large for gpt-4: context window tokens per min (TPM) limit",
        ),
        (400, "model \"x\" not found"),
        (400, "temperature is not supported with this model"),
    ];
    for (status, text) in cases {
        let body = format!(r#"{{"error":{{"message":"{text}"}}}}"#);
        assert!(detect(status, &body).is_none(), "{status}: {text}");
    }
}

#[test]
fn only_400_413_and_422_are_considered() {
    for status in [401, 404, 429, 500, 503] {
        assert!(detect(status, GATEWAY).is_none(), "{status}");
    }
    assert!(detect(422, GATEWAY).is_some());
}

#[test]
fn a_413_needs_a_token_signal() {
    assert!(detect(413, GATEWAY).is_some());
    let strong = r#"{"error":{"code":"context_length_exceeded","message":"too big"}}"#;
    assert!(detect(413, strong).is_some());
}

#[test]
fn an_output_limit_that_also_names_the_context_window_is_an_overflow() {
    let body = "max_tokens plus the prompt exceeds the context window of 8192 tokens";
    assert!(detect_event("", body).is_some());
}

#[test]
fn stream_errors_are_reworded_or_left_alone() {
    assert_eq!(
        stream_error(
            "context_length_exceeded",
            "The prompt is too long: 10, model maximum context length: 5"
        ),
        (
            "invalid_request_error".to_owned(),
            "prompt is too long: 10 tokens > 5 maximum".to_owned()
        )
    );
    assert_eq!(
        stream_error("server_error", "boom"),
        ("upstream_error".to_owned(), "boom".to_owned())
    );
}
