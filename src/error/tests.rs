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

fn upstream(status: u16) -> AppError {
    AppError::upstream(StatusCode::from_u16(status).unwrap(), "x".to_owned())
}

#[test]
fn each_upstream_status_keeps_its_code_and_gets_its_anthropic_type() {
    for (status, kind) in [
        (400, "invalid_request_error"),
        (401, "authentication_error"),
        (403, "permission_error"),
        (404, "not_found_error"),
        (413, "request_too_large"),
        (429, "rate_limit_error"),
        (500, "api_error"),
        (502, "api_error"),
        (503, "overloaded_error"),
        (504, "api_error"),
        (529, "overloaded_error"),
    ] {
        let err = upstream(status);
        assert_eq!(
            err.anthropic_error_body()["error"]["type"],
            kind,
            "{status}"
        );
        assert_eq!(err.into_response().status().as_u16(), status);
    }
}

fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (k, v) in pairs {
        map.insert(
            axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
            v.parse().unwrap(),
        );
    }
    map
}

#[test]
fn retry_after_is_read_from_seconds_milliseconds_or_a_date() {
    let secs = |h: &[(&str, &str)]| parse_retry_after(&headers(h));
    assert_eq!(secs(&[("retry-after", "7")]), Some(Duration::from_secs(7)));
    assert_eq!(
        secs(&[("retry-after", "1.5")]),
        Some(Duration::from_millis(1500))
    );
    // Milliseconds win over seconds.
    assert_eq!(
        secs(&[("retry-after", "7"), ("retry-after-ms", "250")]),
        Some(Duration::from_millis(250))
    );
    // A date in the past is "retry now"; a far-future one is capped.
    assert_eq!(
        secs(&[("retry-after", "Wed, 21 Oct 2015 07:28:00 GMT")]),
        Some(Duration::ZERO)
    );
    assert_eq!(
        secs(&[("retry-after", "Fri, 31 Dec 2100 23:59:59 GMT")]),
        Some(MAX_RETRY_AFTER)
    );
    assert_eq!(secs(&[("retry-after", "9999")]), Some(MAX_RETRY_AFTER));
    for bad in ["", "soon", "-3", "NaN", "inf"] {
        assert_eq!(secs(&[("retry-after", bad)]), None, "{bad:?}");
    }
    assert_eq!(secs(&[]), None);
}

#[test]
fn retry_after_is_sent_on_a_429_or_5xx_and_nowhere_else() {
    let wait = Some(Duration::from_millis(1200));
    let response = upstream(429).with_retry_after(wait).into_response();
    assert_eq!(response.headers()["retry-after"], "2");
    assert_eq!(response.headers()["retry-after-ms"], "1200");

    assert!(
        upstream(529)
            .with_retry_after(wait)
            .into_response()
            .headers()
            .contains_key("retry-after")
    );
    for status in [400, 401, 404] {
        let response = upstream(status).with_retry_after(wait).into_response();
        assert!(!response.headers().contains_key("retry-after"), "{status}");
    }
    assert!(
        !upstream(429)
            .into_response()
            .headers()
            .contains_key("retry-after")
    );
}

#[test]
fn scrubbing_keeps_the_retry_after() {
    let err =
        AppError::scrub_advisor_error(upstream(429).with_retry_after(Some(Duration::from_secs(3))));
    assert!(matches!(
        err,
        AppError::Upstream { retry_after: Some(d), .. } if d == Duration::from_secs(3)
    ));
}

#[test]
fn a_stream_error_code_maps_to_the_matching_anthropic_type() {
    for (code, message, kind) in [
        ("rate_limit_exceeded", "", "rate_limit_error"),
        ("insufficient_quota", "", "rate_limit_error"),
        ("server_is_overloaded", "", "overloaded_error"),
        ("service_unavailable", "", "overloaded_error"),
        ("invalid_api_key", "", "authentication_error"),
        ("invalid_request_error", "", "invalid_request_error"),
        ("server_error", "", "api_error"),
        ("", "boom", "api_error"),
        ("", "Rate limit reached", "rate_limit_error"),
        ("", "the model is overloaded", "overloaded_error"),
    ] {
        assert_eq!(stream_error_kind(code, message), kind, "{code}/{message}");
    }
}

// ─── a failed reply that came back as 200 ───────────────────────────────────

fn reply(status: &str, error: Option<serde_json::Value>) -> crate::responses::ResponsesResponse {
    serde_json::from_value(json!({
        "id": "resp_x", "object": "response", "created_at": 1,
        "status": status, "model": "m", "output": [],
        "error": error,
    }))
    .unwrap()
}

fn failed_with(code: &str, message: &str) -> AppError {
    let resp = reply("failed", Some(json!({"code": code, "message": message})));
    AppError::from_failed_response(&resp).expect("a failed reply is an error")
}

fn status_and_kind(err: AppError) -> (StatusCode, String) {
    let kind = err.anthropic_error_body()["error"]["type"]
        .as_str()
        .unwrap()
        .to_owned();
    (err.into_response().status(), kind)
}

#[test]
fn a_reply_that_did_not_fail_is_not_an_error() {
    for status in ["completed", "incomplete", "cancelled", "in_progress"] {
        assert!(
            AppError::from_failed_response(&reply(status, None)).is_none(),
            "{status}"
        );
    }
}

#[test]
fn a_failed_rate_limit_is_a_429() {
    let err = failed_with("rate_limit_exceeded", "slow down");
    assert_eq!(
        status_and_kind(err),
        (StatusCode::TOO_MANY_REQUESTS, "rate_limit_error".to_owned())
    );
}

#[test]
fn a_failed_overload_is_a_529() {
    let err = failed_with("server_overloaded", "busy");
    let (status, kind) = status_and_kind(err);
    assert_eq!(status.as_u16(), 529);
    assert_eq!(kind, "overloaded_error");
}

#[test]
fn a_failed_server_error_is_a_502_api_error() {
    let err = failed_with("server_error", "boom");
    assert_eq!(
        status_and_kind(err),
        (StatusCode::BAD_GATEWAY, "api_error".to_owned())
    );
}

#[test]
fn a_failed_context_overflow_keeps_anthropics_wording_as_a_400() {
    let err = failed_with(
        "context_length_exceeded",
        "The prompt is too long: 10, model maximum context length: 5",
    );
    assert!(matches!(&err, AppError::ContextOverflow(m)
        if m == "prompt is too long: 10 tokens > 5 maximum"));
    assert_eq!(err.into_response().status(), StatusCode::BAD_REQUEST);
}

#[test]
fn a_failed_auth_or_invalid_request_is_a_502_not_the_clients_fault() {
    // The credentials and the request are the proxy's, not the client's:
    // a 401 or 400 would send the client the wrong way.
    for code in [
        "invalid_api_key",
        "permission_denied",
        "invalid_request_error",
    ] {
        let (status, kind) = status_and_kind(failed_with(code, "nope"));
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{code}");
        assert_eq!(kind, "api_error", "{code}");
    }
}

#[test]
fn a_failed_reply_with_no_error_object_is_a_generic_502() {
    let err = AppError::from_failed_response(&reply("failed", None)).unwrap();
    let body = err.anthropic_error_body();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("upstream response failed")
    );
    assert_eq!(err.into_response().status(), StatusCode::BAD_GATEWAY);
}

#[test]
fn a_missing_model_is_a_404_not_found_error() {
    let err = AppError::NotFound("model with id \"nope\" not found".to_owned());
    let body = err.anthropic_error_body();
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "not_found_error");
    assert_eq!(body["error"]["message"], "model with id \"nope\" not found");
    assert_eq!(err.into_response().status(), StatusCode::NOT_FOUND);
}
