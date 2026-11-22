use super::*;

#[test]
fn a_context_overflow_is_a_400_in_anthropics_exact_form() {
    let err = AppError::ContextOverflow("prompt is too long: 5 tokens > 3 maximum".to_owned());
    assert_eq!(
        err.anthropic_error_body(),
        json!({
            "type": "error",
            "error": {
                "type": "invalid_request_error",
                "message": "prompt is too long: 5 tokens > 3 maximum",
            }
        })
    );
    assert_eq!(err.into_response().status(), StatusCode::BAD_REQUEST);
}

#[test]
fn scrubbing_keeps_the_context_overflow_variant() {
    let err =
        AppError::scrub_advisor_error(AppError::ContextOverflow("prompt is too long".to_owned()));
    assert!(matches!(err, AppError::ContextOverflow(m) if m == "prompt is too long"));
}
