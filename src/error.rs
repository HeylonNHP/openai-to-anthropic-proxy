//! Application error type.
//!
//! `AppError` is the single error type the axum handlers return. It
//! implements `IntoResponse` so handlers can `?`-propagate it directly.
//!
//! The shape that goes back to the client is always an Anthropic-style
//! error envelope: `{ "type": "error", "error": { "type": ..., "message": ... } }`.

use axum::Json;
use axum::http::StatusCode;
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

    /// The upstream returned a non-success status. The status is preserved
    /// so the client sees the same code Anthropic would have returned.
    #[error("upstream returned {status}: {body}")]
    Upstream { status: StatusCode, body: String },

    /// Anything else (network failure, decode failure, etc.).
    #[error("internal error: {0}")]
    Internal(String),
}

impl AppError {
    /// Replace every string in the error with the advisor's beta/tool
    /// tokens scrubbed out. Called on the advisor-active path before an
    /// `AppError` is returned to the client: a 400 mentioning
    /// `advisor_20260301` / `advisor-tool-2026-03-01` / `Input tag` latches
    /// the feature off in Claude Code for the whole process (§18.4).
    /// Only the client-visible bodies are touched; log messages keep their
    /// original text.
    pub(crate) fn scrub_advisor_error(err: Self) -> Self {
        let scrub = crate::tools::advisor::scrub_error_body;
        match err {
            Self::BadRequest(msg) => Self::BadRequest(scrub(&msg)),
            Self::Unauthorized => Self::Unauthorized,
            Self::PayloadTooLarge(msg) => Self::PayloadTooLarge(scrub(&msg)),
            Self::Upstream { status, body } => Self::Upstream {
                status,
                body: scrub(&body),
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
            Self::Upstream { status, body } => {
                let kind = match status.as_u16() {
                    401 | 403 => "authentication_error",
                    404 => "not_found_error",
                    429 => "rate_limit_error",
                    400 => "invalid_request_error",
                    _ => "api_error",
                };
                (kind, format!("upstream returned {status}: {body}"))
            }
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
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::PayloadTooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
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
        (status, Json(body)).into_response()
    }
}

impl From<reqwest::Error> for AppError {
    fn from(err: reqwest::Error) -> Self {
        if let Some(status) = err.status() {
            return Self::Upstream {
                status,
                body: err.to_string(),
            };
        }

        let status = if err.is_timeout() {
            StatusCode::GATEWAY_TIMEOUT
        } else {
            StatusCode::BAD_GATEWAY
        };
        Self::Upstream {
            status,
            body: err.to_string(),
        }
    }
}

impl From<serde_json::Error> for AppError {
    fn from(err: serde_json::Error) -> Self {
        Self::BadRequest(format!("invalid JSON: {err}"))
    }
}
