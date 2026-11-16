use super::*;

/// The exact body the gateway sent on 2026-11-12.
const REAL_404: &str = r#"{"error":{"message":"model \"claude-sonnet-5\" not found. Try glm-5.3-flash:cloud.","type":"not_found_error","param":null,"code":null}}"#;
/// The exact body the gateway sent on 2026-08-16 (single quotes).
const REAL_404_SINGLE_QUOTED: &str = r#"{"error":{"message":"model 'claude-opus-5' not found","type":"not_found_error","param":null,"code":null}}"#;
/// The exact body the gateway sent on 2026-09-18 (double quotes, no hint).
const REAL_404_DOUBLE_QUOTED: &str = r#"{"error":{"message":"model \"claude-fable-5-1\" not found","type":"not_found_error","param":null,"code":null}}"#;

#[test]
fn real_gateway_body_is_a_model_rejection() {
    let rejection =
        detect_model_rejection(404, REAL_404).expect("the real gateway body must be detected");
    assert_eq!(rejection.rejected.as_deref(), Some("claude-sonnet-5"));
    assert_eq!(rejection.suggested.as_deref(), Some("glm-5.3-flash:cloud"));
}

#[test]
fn real_gateway_body_single_quoted() {
    let rejection = detect_model_rejection(404, REAL_404_SINGLE_QUOTED)
        .expect("the single-quoted gateway body must be detected");
    assert_eq!(rejection.rejected.as_deref(), Some("claude-opus-5"));
    assert_eq!(rejection.suggested, None);
}

#[test]
fn real_gateway_body_double_quoted() {
    let rejection = detect_model_rejection(404, REAL_404_DOUBLE_QUOTED)
        .expect("the double-quoted gateway body must be detected");
    assert_eq!(rejection.rejected.as_deref(), Some("claude-fable-5-1"));
}

#[test]
fn flat_responses_body_is_a_model_rejection() {
    let body = r#"{"message":"The requested model claude-sonnet-5 is not supported for the selected provider.","type":"invalid_request_error","code":"model_not_supported"}"#;
    assert!(detect_model_rejection(400, body).is_some());
}

#[test]
fn nested_code_is_a_model_rejection() {
    let body = r#"{"error":{"code":"model_not_found","message":"nope"}}"#;
    assert!(detect_model_rejection(404, body).is_some());
}

#[test]
fn plain_text_body_is_a_model_rejection() {
    assert!(detect_model_rejection(400, "Model not found").is_some());
}

#[test]
fn corpus_non_model_errors_are_not_model_rejections() {
    // The four real non-model bodies in the corpus. The `web_search` one
    // contains "not supported" — a detector keyed on that would fire
    // the model fallback on a tool rejection.
    let bodies = [
        (
            400,
            r#"{"error":{"message":"the web_search tool is not supported (ref: 490f64b3-1e45-4671-aaef-8e7ff3a71f4b)","type":"invalid_request_error","param":null,"code":null}}"#,
        ),
        (
            400,
            r#"{"error":{"message":"invalid reasoning value: \"xhigh\" (must be \"high\", \"medium\", \"low\", \"max\", or \"none\") (ref: 4644c95a)","type":"invalid_request_error","param":null,"code":null}}"#,
        ),
        (
            400,
            r#"{"error":{"message":"Provider returned error","code":400,"metadata":{"raw":"{\"error\": {\"message\": \"Invalid schema for function 'SendMessage': In context=('properties', 'to'), 'allOf' is not permitted.\",\"type\": \"invalid_request_error\",\"param\": \"tools[21].parameters\",\"code\": \"invalid_function_parameters\"}}"},"user_id":"user_3GnW"}}"#,
        ),
        (
            402,
            r#"{"error":{"message":"This request would exceed your available credits given your current in-flight requests. Retry after in-flight requests settle, or add credits.","code":402,"metadata":{"reason":"in_flight_budget_exhausted"}},"user_id":"user_3GnW"}"#,
        ),
    ];
    for (status, body) in bodies {
        assert!(
            detect_model_rejection(status, body).is_none(),
            "must not treat as a model rejection: {body}"
        );
    }
}

#[test]
fn empty_502_and_503_are_not_model_rejections() {
    assert!(detect_model_rejection(502, "").is_none());
    assert!(detect_model_rejection(503, "").is_none());
}

#[test]
fn non_model_404_is_not_a_model_rejection() {
    // A bad path on the upstream: "Not Found" with no model mentioned.
    let body =
        r#"{"error":{"message":"Not Found","type":"not_found_error","param":null,"code":null}}"#;
    assert!(detect_model_rejection(404, body).is_none());
}

#[test]
fn transient_outage_is_not_a_model_rejection() {
    let body = r#"{"error":{"message":"claude-sonnet-5 is temporarily unavailable","type":"invalid_request_error"}}"#;
    assert!(detect_model_rejection(404, body).is_none());
}

#[test]
fn model_word_in_an_unrelated_message_is_not_a_model_rejection() {
    assert!(detect_model_rejection(404, "the model output is not found").is_none());
    // ...and the same sentence inside a JSON envelope, which is the
    // shape that actually matters.
    let body = r#"{"error":{"message":"the model output is not found","type":"not_found_error"}}"#;
    assert!(detect_model_rejection(404, body).is_none());
}

#[test]
fn model_name_prefixed_to_an_unrelated_400_is_not_a_model_rejection() {
    let body = r#"{"error":{"message":"claude-sonnet-5: invalid value for temperature","type":"invalid_request_error"}}"#;
    assert!(detect_model_rejection(400, body).is_none());
}

/// A rejection that mentions the model but whose **subject** is
/// something else — a tool, a parameter, a schema. These must never
/// fire the fallback, because firing it answers the request with a
/// model the operator never chose.
#[test]
fn a_rejection_about_something_else_is_not_a_model_rejection() {
    let bodies = [
        "the web_search tool is not supported for model claude-sonnet-5",
        "claude-sonnet-5 is not supported for the web_search tool",
        "Invalid request for model claude-sonnet-5: images are not supported in this mode",
        "function calling is unsupported by claude-sonnet-5",
        "unknown parameter for claude-sonnet-5",
        "temperature is not supported by claude-sonnet-5",
        "the schema for model claude-sonnet-5 is invalid",
    ];
    for body in bodies {
        let json = format!(r#"{{"error":{{"message":"{body}","type":"invalid_request_error"}}}}"#);
        assert!(
            detect_model_rejection(400, &json).is_none(),
            "must not treat a request-scoped error as a model rejection: {body}"
        );
        // ...and the same body as plain text.
        assert!(
            detect_model_rejection(400, body).is_none(),
            "must not treat a request-scoped error as a model rejection: {body}"
        );
    }
}

#[test]
fn a_non_ascii_message_does_not_panic() {
    // The window arithmetic in an earlier draft sliced at a byte
    // offset that was not a char boundary, which killed the request.
    let body = "模型 claude-sonnet-5 不可用：请求参数不受支持，这是很长的一个句子用来推动偏移量越过字符边界";
    let json = format!(r#"{{"error":{{"message":"{body}","type":"invalid_request_error"}}}}"#);
    for offset in 0..40 {
        let mut padded = body.to_string();
        padded.push_str(&"x".repeat(offset));
        let json =
            format!(r#"{{"error":{{"message":"{padded}","type":"invalid_request_error"}}}}"#);
        let _ = detect_model_rejection(400, &json);
    }
    let _ = detect_model_rejection(400, &json);
}

/// Every request-scoped noun a review turned up. The error is about
/// the request, not the model, so the fallback must not fire — a
/// fallback answers the request with a *different model*, which is
/// silent corruption.
///
/// Each is tried in both shapes: with the word "model" before the
/// name (so the anchored pattern could match) and without it.
#[test]
fn a_request_scoped_error_never_swaps_the_model() {
    let nouns = [
        "web_search",
        "attachment",
        "vector store",
        "assistant",
        "agent",
        "thread",
        "conversation",
        "code",
        "script",
        "shell",
        "command",
        "syntax",
        "regex",
        "encoding",
        "token limit",
        "tokens",
        "context",
    ];
    for noun in nouns {
        for template in [
            "model claude-sonnet-5 is not supported for the __NOUN__",
            "claude-sonnet-5 is not supported for the __NOUN__",
            "model claude-sonnet-5 is not available: __NOUN__ problem",
            "the __NOUN__ is not supported by model claude-sonnet-5",
        ] {
            let body = template.replace("__NOUN__", noun);
            let json =
                format!(r#"{{"error":{{"message":"{body}","type":"invalid_request_error"}}}}"#);
            assert!(
                detect_model_rejection(400, &json).is_none(),
                "must not swap the model for: {body}"
            );
            assert!(
                detect_model_rejection(400, &body).is_none(),
                "must not swap the model for: {body}"
            );
        }
    }
}

/// Prose that happens to contain a digit must not read as a model id.
/// `model 2fa not found` is a two-factor error, not a model rejection.
#[test]
fn digit_prose_is_not_a_model_id() {
    for body in [
        "model 2fa not found",
        "model x1 not found",
        "model 404 not found",
        "model a1 not found",
    ] {
        let json = format!(r#"{{"error":{{"message":"{body}","type":"not_found_error"}}}}"#);
        assert!(
            detect_model_rejection(404, &json).is_none(),
            "digit prose must not read as a model: {body}"
        );
    }
}

/// The absence negatives the gateway actually uses must still fire.
#[test]
fn the_absence_negatives_still_fire() {
    for body in [
        "model \"claude-sonnet-5\" not found",
        "model 'claude-opus-5' not found",
        "model claude-sonnet-5 not found",
        "model claude-sonnet-5 does not exist",
        "model claude-sonnet-5 doesn't exist",
        "model claude-sonnet-5 is deprecated",
    ] {
        let json = format!(r#"{{"error":{{"message":"{body}","type":"not_found_error"}}}}"#);
        assert!(
            detect_model_rejection(404, &json).is_some(),
            "must still fire for: {body}"
        );
    }
}

/// A message where "model" sits immediately before the negative has
/// no room for a different subject, so it is recognised.
#[test]
fn adjacent_model_phrases_still_fire() {
    for body in [
        "Model not found",
        "MODEL NOT SUPPORTED",
        "model not supported",
        "model not available",
        "model unavailable",
        "Unknown model: foo",
        "no such model",
        "The requested model claude-sonnet-5 is not supported for the selected provider.",
    ] {
        assert!(
            detect_model_rejection(400, body).is_some(),
            "must still fire for: {body}"
        );
    }
}

#[test]
fn suggestion_must_look_like_a_model_id() {
    for prose in [
        "model \"x\" not found. Try another model.",
        "model \"x\" not found. Try https://evil.example/x",
        "model \"x\" not found. Try the default.",
        "model \"x\" not found. Try /usr/local/bin",
        "model \"x\" not found. Try user@example.com",
        "model \"x\" not found. Try e.g. something",
    ] {
        let rejection = detect_model_rejection(404, prose);
        assert_eq!(
            rejection.and_then(|r| r.suggested),
            None,
            "prose must not be read as a suggestion: {prose}"
        );
    }
}

#[test]
fn the_envelope_flattens_both_shapes() {
    let nested = parse_error_envelope(REAL_404);
    assert!(nested.is_json);
    assert_eq!(nested.kind, "not_found_error");
    assert_eq!(nested.code, "");
    assert!(nested.message.contains("claude-sonnet-5"));

    let flat = parse_error_envelope(
        r#"{"message":"flat message","type":"invalid_request_error","code":"model_not_found"}"#,
    );
    assert_eq!(flat.message, "flat message");
    assert_eq!(flat.kind, "invalid_request_error");
    assert_eq!(flat.code, "model_not_found");

    let text = parse_error_envelope("not json at all");
    assert!(!text.is_json);
    assert_eq!(text.message, "not json at all");
}

#[test]
fn a_numeric_code_is_kept() {
    let env = parse_error_envelope(r#"{"error":{"message":"m","code":402}}"#);
    assert_eq!(env.code, "402");
}
