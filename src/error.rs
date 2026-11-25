//! Application error type.
//!
//! `AppError` is the single error type the axum handlers return. It
//! implements `IntoResponse` so handlers can `?`-propagate it directly.
//!
//! The shape that goes back to the client is always an Anthropic-style
//! error envelope: `{ "type": "error", "error": { "type": ..., "message": ... } }`.

use std::time::Duration;

use axum::Json;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::json;
use thiserror::Error;

/// All errors that can come out of a request handler.
#[derive(Debug, Error)]
pub enum AppError {
    /// The client sent a request we couldn't even parse.
    #[error("invalid request: {0}")]
    BadRequest(String),

    /// The client did not provide a valid `X-Proxy-Key` header (or
    /// the configured `proxy_key` is unset and we're being defensive
    /// elsewhere). 401 with an Anthropic-shaped body.
    #[error("unauthorized")]
    Unauthorized,

    /// The inbound body exceeded `max_request_body_bytes`. 413 with an
    /// Anthropic-shaped `request_too_large` body, so clients get a message
    /// they can act on instead of axum's bare plain-text rejection.
    #[error("request too large: {0}")]
    PayloadTooLarge(String),

    /// The prompt does not fit the model's context window. Carries
    /// Anthropic's exact wording (`prompt is too long: N tokens > M maximum`)
    /// so Claude Code recognises it and compacts. Always HTTP 400.
    #[error("{0}")]
    ContextOverflow(String),

    /// The upstream returned a non-success status. The status is preserved
    /// so the client sees the same code Anthropic would have returned, and
    /// so is the upstream's `Retry-After` (on a 429 or 5xx), which Claude
    /// Code uses to pace its own retry.
    #[error("upstream returned {status}: {body}")]
    Upstream {
        status: StatusCode,
        body: String,
        retry_after: Option<Duration>,
    },

    /// The requested resource does not exist — `GET /v1/models/{id}`
    /// for an id no configured model answers to. 404, with Anthropic's
    /// `not_found_error` envelope.
    #[error("{0}")]
    NotFound(String),

    /// Anything else (network failure, decode failure, etc.).
    #[error("internal error: {0}")]
    Internal(String),
}

/// The longest `Retry-After` the proxy passes on. The Anthropic SDK ignores
/// anything over a minute and backs off on its own, and a cap stops an odd
/// upstream from stalling the client.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);

/// The upstream's requested wait, from `retry-after-ms`, else `retry-after`
/// (whole or fractional seconds, or an HTTP date). `None` when absent or
/// unreadable. Capped at [`MAX_RETRY_AFTER`].
#[must_use]
pub fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    let text = |name: &str| headers.get(name)?.to_str().ok().map(str::trim);
    let wait = text("retry-after-ms")
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|ms| ms.is_finite() && *ms >= 0.0)
        .map(|ms| Duration::from_secs_f64(ms / 1000.0))
        .or_else(|| {
            let value = text("retry-after")?;
            value
                .parse::<f64>()
                .ok()
                .filter(|s| s.is_finite() && *s >= 0.0)
                .map(Duration::from_secs_f64)
                .or_else(|| {
                    let date = httpdate::parse_http_date(value).ok()?;
                    Some(
                        date.duration_since(std::time::SystemTime::now())
                            .unwrap_or_default(),
                    )
                })
        })?;
    Some(wait.min(MAX_RETRY_AFTER))
}

/// The Anthropic error `type` for an HTTP status.
const fn error_kind_for_status(status: u16) -> &'static str {
    match status {
        400 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        413 => "request_too_large",
        429 => "rate_limit_error",
        // 502 and 504 stay `api_error`: they are the proxy's own connect and
        // timeout failures, not the upstream saying it is overloaded.
        503 | 529 => "overloaded_error",
        _ => "api_error",
    }
}

/// The Anthropic error `type` for an error code the upstream sent inside a
/// stream, where there is no HTTP status. Unknown codes are `api_error`.
#[must_use]
pub fn stream_error_kind(code: &str, message: &str) -> &'static str {
    let code = code.to_ascii_lowercase();
    let lower = message.to_ascii_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| code.contains(n));
    if has(&["rate_limit", "too_many_requests", "insufficient_quota"]) {
        "rate_limit_error"
    } else if has(&["overloaded", "service_unavailable", "slow_down"]) {
        "overloaded_error"
    } else if has(&["invalid_api_key", "authentication"]) {
        "authentication_error"
    } else if has(&["permission"]) {
        "permission_error"
    } else if has(&["invalid_request", "invalid_prompt"]) {
        "invalid_request_error"
    } else if code.is_empty() && lower.contains("rate limit") {
        "rate_limit_error"
    } else if code.is_empty() && lower.contains("overloaded") {
        "overloaded_error"
    } else {
        "api_error"
    }
}

impl AppError {
    /// An upstream failure with no `Retry-After`.
    #[must_use]
    pub const fn upstream(status: StatusCode, body: String) -> Self {
        Self::Upstream {
            status,
            body,
            retry_after: None,
        }
    }

    /// A generic failure the upstream reported without saying why: 502, with
    /// the message marked as the upstream's.
    #[must_use]
    pub fn upstream_failure(message: &str) -> Self {
        Self::upstream(
            StatusCode::BAD_GATEWAY,
            format!("upstream_error: {message}"),
        )
    }

    /// A failure the upstream reported inside a response or a stream, where
    /// there is no HTTP status.
    ///
    /// A context overflow keeps Anthropic's wording, so Claude Code compacts.
    /// Otherwise the status is the one whose Anthropic error type matches what
    /// the upstream said: 429 for a rate limit, 529 for an overload, and 502
    /// for everything else. Auth, permission and invalid-request codes are
    /// deliberately 502 too: they describe the proxy's own credentials or
    /// request, and a 401 or 400 would send the client the wrong way.
    #[must_use]
    pub fn reported_failure(code: &str, message: &str) -> Self {
        crate::context_overflow::detect_event(code, message).map_or_else(
            || {
                let status = match stream_error_kind(code, message) {
                    "rate_limit_error" => StatusCode::TOO_MANY_REQUESTS,
                    "overloaded_error" => {
                        StatusCode::from_u16(529).unwrap_or(StatusCode::BAD_GATEWAY)
                    }
                    _ => StatusCode::BAD_GATEWAY,
                };
                Self::upstream(status, format!("upstream_error: {message}"))
            },
            |overflow| Self::ContextOverflow(overflow.anthropic_message()),
        )
    }

    /// The error for a buffered upstream reply that came back `200` with
    /// `status: "failed"`, or `None` for any other reply.
    ///
    /// Anthropic answers a failure with an error status, and its clients retry
    /// or compact on it, so a failed reply must not be rendered as a message.
    /// This is the streaming path's mapping, applied to a buffered body.
    #[must_use]
    pub fn from_failed_response(resp: &crate::responses::ResponsesResponse) -> Option<Self> {
        if resp.status != "failed" {
            return None;
        }
        Some(resp.error.as_ref().map_or_else(
            || Self::upstream_failure("upstream response failed"),
            |error| {
                Self::reported_failure(
                    &crate::responses::code_text(error.code.as_ref()),
                    &error.message,
                )
            },
        ))
    }

    /// Attach the upstream's `Retry-After` to an upstream error. Only a 429
    /// or a 5xx carries one; anything else, or any other variant, is returned
    /// unchanged.
    #[must_use]
    pub fn with_retry_after(mut self, wait: Option<Duration>) -> Self {
        if let Self::Upstream {
            status,
            retry_after,
            ..
        } = &mut self
            && (status.as_u16() == 429 || status.is_server_error())
        {
            *retry_after = wait;
        }
        self
    }

    /// Replace every string in the error with the advisor's beta/tool
    /// tokens scrubbed out. Called on the advisor-active path before an
    /// `AppError` is returned to the client: a 400 mentioning
    /// `advisor_20260301` / `advisor-tool-2026-03-01` / `Input tag` latches
    /// the feature off in Claude Code for the whole process.
    /// Only the client-visible bodies are touched; log messages keep their
    /// original text.
    pub(crate) fn scrub_advisor_error(err: Self) -> Self {
        let scrub = crate::tools::advisor::scrub_error_body;
        match err {
            Self::BadRequest(msg) => Self::BadRequest(scrub(&msg)),
            Self::Unauthorized => Self::Unauthorized,
            Self::PayloadTooLarge(msg) => Self::PayloadTooLarge(scrub(&msg)),
            // Nothing advisor- or upstream-shaped can be in this message:
            // it echoes the id the client itself asked for.
            Self::NotFound(msg) => Self::NotFound(msg),
            Self::ContextOverflow(msg) => Self::ContextOverflow(scrub(&msg)),
            Self::Upstream {
                status,
                body,
                retry_after,
            } => Self::Upstream {
                status,
                body: scrub(&body),
                retry_after,
            },
            Self::Internal(msg) => Self::Internal(scrub(&msg)),
        }
    }

    /// The Anthropic error `type` and message for this error.
    fn kind_and_message(&self) -> (&'static str, String) {
        match self {
            Self::BadRequest(msg) => ("invalid_request_error", msg.clone()),
            Self::Unauthorized => (
                "authentication_error",
                "missing or invalid X-Proxy-Key header".to_owned(),
            ),
            Self::PayloadTooLarge(msg) => ("request_too_large", msg.clone()),
            Self::NotFound(msg) => ("not_found_error", msg.clone()),
            Self::ContextOverflow(msg) => (crate::context_overflow::ERROR_KIND, msg.clone()),
            Self::Upstream { status, body, .. } => (
                error_kind_for_status(status.as_u16()),
                format!("upstream returned {status}: {body}"),
            ),
            Self::Internal(msg) => ("api_error", msg.clone()),
        }
    }

    /// The body of an SSE `error` event, for a failure after a stream has
    /// already started (when an HTTP status can no longer be sent).
    pub(crate) fn to_api_error_body(&self) -> crate::anthropic::ApiErrorBody {
        let (kind, message) = self.kind_and_message();
        crate::anthropic::ApiErrorBody {
            r#type: kind.to_owned(),
            message,
        }
    }

    fn anthropic_error_body(&self) -> serde_json::Value {
        let (kind, message) = self.kind_and_message();

        json!({
            "type": "error",
            "error": {
                "type": kind,
                "message": message,
            }
        })
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = match &self {
            Self::BadRequest(_) | Self::ContextOverflow(_) => StatusCode::BAD_REQUEST,
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::PayloadTooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            // 4xx and 5xx upstream statuses are passed through, but if the
            // status itself is informational (1xx) or a success code (2xx/3xx)
            // we collapse to 502 — something is wrong if we got here.
            Self::Upstream { status, .. }
                if status.is_client_error() || status.is_server_error() =>
            {
                *status
            }
            Self::Upstream { .. } => StatusCode::BAD_GATEWAY,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };

        let body = self.anthropic_error_body();
        let mut response = (status, Json(body)).into_response();
        if let Self::Upstream {
            retry_after: Some(wait),
            ..
        } = &self
        {
            let headers = response.headers_mut();
            // Whole seconds, rounded up, so the client never retries early.
            if let Ok(v) = HeaderValue::from_str(&wait.as_secs_f64().ceil().to_string()) {
                headers.insert("retry-after", v);
            }
            if let Ok(v) = HeaderValue::from_str(&wait.as_millis().to_string()) {
                headers.insert("retry-after-ms", v);
            }
        }
        response
    }
}

impl From<reqwest::Error> for AppError {
    fn from(err: reqwest::Error) -> Self {
        if let Some(status) = err.status() {
            return Self::upstream(status, err.to_string());
        }

        let status = if err.is_timeout() {
            StatusCode::GATEWAY_TIMEOUT
        } else {
            StatusCode::BAD_GATEWAY
        };
        Self::upstream(status, err.to_string())
    }
}

impl From<serde_json::Error> for AppError {
    fn from(err: serde_json::Error) -> Self {
        Self::BadRequest(format!("invalid JSON: {err}"))
    }
}

#[cfg(test)]
mod tests;
