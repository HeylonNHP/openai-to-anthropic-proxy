//! `POST /v1/messages/count_tokens`: how many input tokens a request would use.
//!
//! Claude Code calls this to size its context. The count comes from the
//! upstream's own token-counting endpoint (`POST {upstream_path}/input_tokens`,
//! OpenAI's `/v1/responses/input_tokens`) when it has one, because only the
//! upstream knows its model's tokenizer. A gateway without that endpoint, or a
//! failed call, falls back to a local estimate: Claude Code would only fall
//! back to a cruder one of its own, so an error buys nothing.
//!
//! The request is translated with the same code as `/v1/messages`, so the
//! count reflects exactly what that request would send.

use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use serde_json::{Value, json};

use crate::anthropic::CreateMessageRequest;
use crate::error::AppError;
use crate::model_rejection::detect_model_rejection;
use crate::proxy::{AppState, build_outbound, resolve_upstream_model, send_upstream_once};
use crate::responses::{InputTokensRequest, InputTokensResponse};

/// Characters per token, for every place the proxy converts between text and
/// tokens without a tokenizer. A rough rule of thumb for English.
pub const CHARS_PER_TOKEN: usize = 4;

/// Tokens assumed for one base64 image. Counting its characters would
/// overstate a screenshot by two orders of magnitude.
const IMAGE_TOKENS: u64 = 1_600;

/// A count should never stall the client: past this, estimate instead.
const COUNT_TIMEOUT: Duration = Duration::from_secs(10);

/// Statuses that mean "this upstream has no such endpoint".
const UNSUPPORTED_STATUSES: [u16; 3] = [404, 405, 501];

pub async fn handle_count_tokens(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    let req = parse_count_request(&body)?;
    let mappings = state.mappings.load_live().clone();
    let model = resolve_upstream_model(&state, &mappings, &req.model);
    let (outbound, _registry, _replay) = build_outbound(&req, &state.config, &model, None, false)?;
    let count_body = InputTokensRequest::from(&outbound);

    let (tokens, source) = count_upstream(&state, &headers, &count_body)
        .await
        .map_or_else(
            || (estimate_tokens(&count_body), "estimate"),
            |tokens| (tokens, "upstream"),
        );
    tracing::info!(model = %model, tokens, source, "count_tokens");
    Ok(Json(json!({ "input_tokens": tokens })))
}

/// Parse a count request. Its body is the messages body minus `max_tokens`,
/// which `CreateMessageRequest` requires, so a placeholder is supplied. It
/// never reaches the upstream: [`InputTokensRequest`] has no output cap.
fn parse_count_request(body: &[u8]) -> Result<CreateMessageRequest, AppError> {
    let mut value: Value = serde_json::from_slice(body)
        .map_err(|e| AppError::BadRequest(format!("invalid request body: {e}")))?;
    if let Some(map) = value.as_object_mut() {
        map.entry("max_tokens").or_insert_with(|| json!(1));
    }
    serde_json::from_value(value)
        .map_err(|e| AppError::BadRequest(format!("invalid request body: {e}")))
}

/// Ask the upstream for the count. `None` means "estimate instead", and the
/// reason is logged.
async fn count_upstream(
    state: &AppState,
    headers: &HeaderMap,
    body: &InputTokensRequest<'_>,
) -> Option<u64> {
    if state.count_endpoint_unsupported.load(Ordering::Relaxed) {
        return None;
    }
    let url = format!(
        "{}/input_tokens",
        crate::proxy::upstream_url(&state.config).trim_end_matches('/')
    );
    let resp = match send_upstream_once(state, &url, headers, body, Some(COUNT_TIMEOUT)).await {
        Ok(resp) => resp,
        Err(error) => {
            tracing::warn!(%error, "token count request failed; estimating");
            return None;
        }
    };
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if status.is_success() {
        return match serde_json::from_str::<InputTokensResponse>(&text) {
            Ok(parsed) => Some(parsed.input_tokens),
            Err(error) => {
                tracing::warn!(%error, "unreadable token count response; estimating");
                None
            }
        };
    }
    // A 404 can also be "no such model": that says nothing about whether the
    // endpoint exists, so it must not switch counting off.
    if UNSUPPORTED_STATUSES.contains(&status.as_u16())
        && detect_model_rejection(status.as_u16(), &text).is_none()
    {
        if !state
            .count_endpoint_unsupported
            .swap(true, Ordering::Relaxed)
        {
            tracing::warn!(
                %status,
                "the upstream has no token-counting endpoint; estimating from now on"
            );
        }
    } else {
        tracing::warn!(%status, body = %text, "token count rejected; estimating");
    }
    None
}

/// Estimate the input tokens of a counting request without a tokenizer:
/// every string value and object key at [`CHARS_PER_TOKEN`] characters per
/// token, rounded up, and each base64 image at a flat [`IMAGE_TOKENS`].
fn estimate_tokens(body: &impl serde::Serialize) -> u64 {
    let value = serde_json::to_value(body).unwrap_or(Value::Null);
    let (chars, images) = measure(&value);
    chars.div_ceil(CHARS_PER_TOKEN as u64) + images * IMAGE_TOKENS
}

/// `(characters, base64 images)` in a JSON value.
fn measure(value: &Value) -> (u64, u64) {
    match value {
        Value::String(s) if is_base64_data_uri(s) => (0, 1),
        Value::String(s) => (s.chars().count() as u64, 0),
        Value::Array(items) => items.iter().map(measure).fold((0, 0), add),
        Value::Object(map) => map
            .iter()
            .map(|(key, v)| add(measure(v), (key.chars().count() as u64, 0)))
            .fold((0, 0), add),
        _ => (0, 0),
    }
}

fn add(a: (u64, u64), b: (u64, u64)) -> (u64, u64) {
    (a.0 + b.0, a.1 + b.1)
}

fn is_base64_data_uri(s: &str) -> bool {
    s.starts_with("data:") && s.contains(";base64,")
}

#[cfg(test)]
mod tests;
