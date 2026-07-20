//! axum router and request handlers.
//!
//! Wires together: read Anthropic request → translate to Responses →
//! forward to upstream → translate response (or stream) back → ship to
//! Claude Code.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use bytes::Bytes;
use eventsource_stream::{EventStream, Eventsource};
use futures_util::{Stream, StreamExt};

use crate::anthropic::{CreateMessageRequest, StreamEvent};
use crate::config::Config;
use crate::error::AppError;
use crate::responses;
use crate::stream::StreamTranslator;
use crate::translate;
use eventsource_stream::Event as SseEvent;
use eventsource_stream::EventStreamError;
use serde_json::Value;

/// Shared state passed to every handler.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub client: reqwest::Client,
}

// Per-request stash of the JSON body we sent upstream. Used only on
// the error path: when the upstream rejects, we log the sent body
// alongside the error so postmortem debugging is one log line away.
//
// Lives in a `task_local!` (not a file) so two concurrent requests
// can't trample each other's bodies — `task_local!` storage is scoped
// to the current task, and each axum request is its own task.
tokio::task_local! {
    static LAST_SENT_BODY: String;
}

/// Build the axum router.
pub fn router(config: Arc<Config>, client: reqwest::Client) -> Router {
    let state = AppState { config, client };
    Router::new()
        .route("/v1/messages", post(handle_messages))
        .route("/healthz", get(handle_health))
        .with_state(state)
}

// ─── Handlers ─────────────────────────────────────────────────────────────

async fn handle_health() -> &'static str {
    "ok"
}

async fn handle_messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, AppError> {
    let req: CreateMessageRequest = serde_json::from_slice(&body)
        .map_err(|e| AppError::BadRequest(format!("invalid request body: {e}")))?;

    // Resolve the inbound model name to the upstream model name BEFORE
    // building the request — so the reasoning-effort lookup uses the
    // *resolved* name (e.g. an aliased `claude-sonnet-5` request picks
    // up the reasoning entry for `gpt-5.4-mini`, not the entry for
    // `claude-sonnet-5`). If no alias is configured, the name passes
    // through unchanged.
    let upstream_model = state.config.upstream_model_for(&req.model);
    let reasoning_effort = state.config.reasoning_for_model(&upstream_model);
    let mut outbound = translate::anthropic_to_responses(&req, reasoning_effort)
        .map_err(|e| AppError::BadRequest(format!("translation error: {e}")))?;
    // The translator copies the inbound model name verbatim; rewrite
    // it here so the upstream sees the alias-resolved name.
    outbound.model = upstream_model;

    // Serialize once, log a short summary, and stash the full body for
    // the error path. The body is the single most useful artifact when
    // debugging an upstream rejection, so we keep it in a task_local!
    // scoped to this handler — see LAST_SENT_BODY above.
    let body_json = serde_json::to_string(&outbound)
        .map_err(|e| AppError::Internal(format!("serialize outbound body: {e}")))?;

    tracing::info!(
        inbound_model = %req.model,
        upstream_model = %outbound.model,
        stream = outbound.stream,
        tools = outbound.tools.as_ref().map_or(0, Vec::len),
        reasoning_effort = ?outbound.reasoning.as_ref().map(|r| r.effort.clone()),
        "→ upstream"
    );

    let start = Instant::now();

    // Print a user-readable request summary to the terminal.
    let tool_count = outbound.tools.as_ref().map_or(0, Vec::len);
    let msg_count = req.messages.len();
    let stream_label = if outbound.stream {
        "  |  stream"
    } else {
        ""
    };
    let reasoning = outbound
        .reasoning
        .as_ref()
        .map(|r| format!("  |  reasoning: {}", r.effort))
        .unwrap_or_default();
    println!(
        "  → {} → {}  |  {} tools  |  {} messages{}{}",
        req.model, outbound.model, tool_count, msg_count, stream_label, reasoning
    );

    LAST_SENT_BODY
        .scope(
            body_json,
            handle_messages_inner(state, headers, req, outbound, start),
        )
        .await
}

async fn handle_messages_inner(
    state: AppState,
    headers: HeaderMap,
    req: CreateMessageRequest,
    outbound: responses::ResponsesRequest,
    start: Instant,
) -> Result<Response, AppError> {
    let url = format!(
        "{}{}",
        state.config.upstream_base_url.trim_end_matches('/'),
        state.config.upstream_path
    );

    // Send the first attempt. The non-streaming path may issue a
    // single retry with the configured `default_model` if the
    // upstream rejects the requested model; the streaming path
    // can't safely retry once the response stream has started (see
    // TODO at the end of this file).
    let mut outbound = outbound;
    let mut attempt = 1u8;
    let upstream_resp = loop {
        let mut upstream_req = state
            .client
            .post(&url)
            .header(header::CONTENT_TYPE, "application/json")
            .bearer_auth(&state.config.upstream_api_key)
            .json(&outbound);

        // Forward a few select headers if the client set them (e.g. tracing
        // identifiers). We don't blindly forward — that risks leaking
        // credentials or breaking the upstream contract.
        for key in ["x-request-id", "anthropic-version"] {
            if let Some(v) = headers.get(key) {
                upstream_req = upstream_req.header(key, v.clone());
            }
        }

        let resp = upstream_req.send().await?;
        let status = resp.status();
        if status.is_success() {
            break resp;
        }
        let body = resp.text().await.unwrap_or_default();
        // The first call to the upstream for a given request, sent
        // through this same task — pulled from the task-local, not
        // from disk, so concurrent requests don't trample each other.
        let sent = LAST_SENT_BODY.try_with(|b| b.clone()).unwrap_or_default();
        tracing::warn!(%status, body = %body, "← upstream error");
        tracing::warn!(
            target: "upstream_payload",
            %status,
            sent_body = %sent,
            "← upstream error (full payload)"
        );

        // Decide whether to retry with the default model. The retry
        // happens before any response bytes are written to the client
        // (streaming or not), so it is safe for both paths. Only one
        // retry; the loop ends after attempt 2 regardless.
        let should_retry = attempt == 1
            && is_model_not_supported(status, &body)
            && state
                .config
                .default_model()
                .is_some_and(|fb| fb != outbound.model);

        if should_retry {
            let fallback = state.config.default_model().unwrap();
            let fallback_reasoning = state.config.reasoning_for_model(fallback);
            tracing::warn!(
                inbound_model = %outbound.model,
                fallback_model = %fallback,
                "upstream rejected model; falling back to default_model"
            );
            outbound.model = fallback.to_owned();
            outbound.reasoning = fallback_reasoning.map(|effort| responses::ReasoningConfig {
                effort,
                ..responses::ReasoningConfig::default()
            });
            attempt = 2;
            continue;
        }

        return Err(map_upstream_error(status, &body));
    };

    let status = upstream_resp.status();
    debug_assert!(status.is_success(), "loop above only breaks on success");

    if outbound.stream {
        let content_type = upstream_resp
            .headers()
            .get(header::CONTENT_TYPE)
            .cloned()
            .unwrap_or_else(|| HeaderValue::from_static("text/event-stream"));
        let byte_stream = upstream_resp.bytes_stream();
        let sse: EventStream<_> = byte_stream.eventsource();

        let msg_id = synthetic_message_id(&req);
        let model = outbound.model.clone();
        let translator_stream = TranslatorStream::new(
            sse,
            msg_id,
            model,
            start,
        );

        let body = Body::from_stream(translator_stream);
        let response = Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, content_type)
            .header(header::CACHE_CONTROL, "no-cache")
            .body(body)
            .map_err(|e| AppError::Internal(format!("build streaming response: {e}")))?;
        Ok(response)
    } else {
        let bytes = upstream_resp.bytes().await?;
        let upstream_resp_body: responses::ResponsesResponse = serde_json::from_slice(&bytes)
            .map_err(|e| AppError::Upstream {
                status: StatusCode::BAD_GATEWAY,
                body: format!("upstream returned non-JSON body: {e}"),
            })?;
        let anth = translate::responses_to_anthropic(&upstream_resp_body);
        let elapsed = start.elapsed();
        let usage = &upstream_resp_body.usage;
        let in_tokens = usage.as_ref().map_or(0, |u| u.input_tokens);
        let out_tokens = usage.as_ref().map_or(0, |u| u.output_tokens);
        let thinking = usage
            .as_ref()
            .and_then(|u| u.output_tokens_details.as_ref())
            .map_or(0, |d| d.reasoning_tokens);
        let cache_read = usage
            .as_ref()
            .and_then(|u| u.input_tokens_details.as_ref())
            .map_or(0, |d| d.cached_tokens);
        let cache_write = usage
            .as_ref()
            .and_then(|u| u.input_tokens_details.as_ref())
            .map_or(0, |d| d.cache_write_tokens);
        let thinking_str = if thinking > 0 {
            format!("  |  {} thinking", thinking)
        } else {
            String::new()
        };
        println!(
            "  ← {}  |  {:.2}s  |  {} in  |  {} out{}  |  cache: {}r {}w",
            upstream_resp_body.status,
            elapsed.as_secs_f64(),
            in_tokens,
            out_tokens,
            thinking_str,
            cache_read,
            cache_write,
        );
        Ok(Json(anth).into_response())
    }
}

/// Print a response stats line from a `StreamStats` (used by the streaming path).
fn print_stream_stats(stats: &crate::stream::StreamStats, elapsed: std::time::Duration) {
    let usage = &stats.usage;
    let in_tokens = usage.as_ref().map_or(0, |u| u.input_tokens);
    let out_tokens = usage.as_ref().map_or(0, |u| u.output_tokens);
    let thinking = usage.as_ref().map_or(0, |u| u.thinking_tokens);
    let cache_read = stats.cache_read_input_tokens;
    let cache_write = stats.cache_creation_input_tokens;
    let stop_reason = stats
        .stop_reason
        .as_ref()
        .map_or("completed", |s| match s {
            crate::anthropic::StopReason::EndTurn => "completed",
            crate::anthropic::StopReason::MaxTokens => "max_tokens",
            crate::anthropic::StopReason::StopSequence => "stop_sequence",
            crate::anthropic::StopReason::ToolUse => "tool_use",
            crate::anthropic::StopReason::Refusal => "refusal",
            crate::anthropic::StopReason::PauseTurn => "paused",
        });
    let thinking_str = if thinking > 0 {
        format!("  |  {} thinking", thinking)
    } else {
        String::new()
    };
    println!(
        "  ← {}  |  {:.2}s  |  {} in  |  {} out{}  |  cache: {}r {}w",
        stop_reason,
        elapsed.as_secs_f64(),
        in_tokens,
        out_tokens,
        thinking_str,
        cache_read,
        cache_write,
    );
}

/// Heuristic for "the upstream doesn't know this model". Matches on
/// (a) status in `[400, 404]` and (b) either a structured error code
/// (`code: "model_not_found"` / `model_not_supported`) or a body
/// containing one of the well-known phrases. Conservative by design:
/// false negatives are fine (the operator sees the original error),
/// false positives would mask unrelated 400s.
fn is_model_not_supported(status: StatusCode, body: &str) -> bool {
    if !matches!(status.as_u16(), 400 | 404) {
        return false;
    }

    // Structured codes first: upstreams may return a generic message
    // text while putting the real reason in `code` (flat Responses
    // envelope) or `error.code` (Chat Completions envelope).
    if let Some(code) = extract_model_error_code(body)
        && matches!(code.as_str(), "model_not_found" | "model_not_supported")
    {
        return true;
    }

    const PHRASES: &[&str] = &[
        "model_not_found",
        "model not found",
        "model not supported",
        "not supported for the selected provider",
        "unknown model",
    ];
    let lower = body.to_ascii_lowercase();
    PHRASES.iter().any(|p| lower.contains(p))
}

/// Extract the `code` field from a structured error envelope. Returns
/// `None` if the body isn't JSON or has no recognized code shape.
fn extract_model_error_code(body: &str) -> Option<String> {
    // Responses API errors are flat: { "code": "...", "message": "..." }
    if let Ok(err) = serde_json::from_str::<responses::ResponsesError>(body)
        && let Some(Value::String(s)) = err.code
    {
        return Some(s);
    }
    // Chat Completions style errors are wrapped: { "error": { "code": "...", "message": "..." } }
    if let Ok(v) = serde_json::from_str::<Value>(body)
        && let Some(code) = v
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(|c| c.as_str())
    {
        return Some(code.to_owned());
    }
    None
}

// ─── Streaming bridge ─────────────────────────────────────────────────────

/// Bridges the upstream SSE event stream with our `StreamTranslator` and
/// yields Anthropic-formatted SSE bytes ready to write to the client.
///
/// This is its own struct (not a closure chain) because the translator
/// carries state across events — the closure-in-`filter_map` shape
/// can't share that state cleanly.
struct TranslatorStream<S>
where
    S: Stream<Item = Result<SseEvent, EventStreamError<reqwest::Error>>> + Unpin,
{
    inner: S,
    translator: Option<StreamTranslator>,
    /// When the request started, for computing elapsed time.
    start: Instant,
    /// Captured stats after the translator is consumed, so `Drop` can print them.
    stats: Option<crate::stream::StreamStats>,
}

impl<S> TranslatorStream<S>
where
    S: Stream<Item = Result<SseEvent, EventStreamError<reqwest::Error>>> + Unpin,
{
    fn new(
        inner: S,
        msg_id: String,
        model: String,
        start: Instant,
    ) -> Self {
        Self {
            inner,
            translator: Some(StreamTranslator::new(msg_id, model)),
            start,
            stats: None,
        }
    }

    /// Take the translator, capture its stats, and return the events.
    fn take_and_report(&mut self) -> Option<Vec<StreamEvent>> {
        let t = self.translator.take()?;
        self.stats = Some(t.stats());
        let events = t.finish();
        if events.is_empty() { None } else { Some(events) }
    }

    /// Take the translator for an error path, capture stats, return events.
    fn take_and_report_error(
        &mut self,
        kind: impl Into<String>,
        message: impl Into<String>,
    ) -> Vec<StreamEvent> {
        let t = self.translator.take().expect("translator already taken");
        self.stats = Some(t.stats());
        t.emit_error(kind, message)
    }
}

impl<S> Drop for TranslatorStream<S>
where
    S: Stream<Item = Result<SseEvent, EventStreamError<reqwest::Error>>> + Unpin,
{
    fn drop(&mut self) {
        if let Some(stats) = self.stats.as_ref() {
            let elapsed = self.start.elapsed();
            print_stream_stats(stats, elapsed);
        }
    }
}

impl<S> Stream for TranslatorStream<S>
where
    S: Stream<Item = Result<SseEvent, EventStreamError<reqwest::Error>>> + Unpin,
{
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // SAFETY: we never move the inner stream out of `self`.
        let this = self.get_mut();
        loop {
            // The translator is `None` after a clean close (terminal
            // event, error event, or a truncated stream). Any further
            // event from the inner stream is a stray — drop it and end
            // the outer stream.
            if this.translator.is_none() {
                match this.inner.poll_next_unpin(cx) {
                    Poll::Ready(Some(_)) => continue,
                    _ => return Poll::Ready(None),
                }
            }
            match this.inner.poll_next_unpin(cx) {
                Poll::Ready(Some(Ok(msg))) => {
                    if msg.data.trim() == "[DONE]" {
                        // The Responses API does not use a `[DONE]`
                        // sentinel; its terminal event is
                        // `response.completed` / `incomplete` / `failed`,
                        // which the translator handles directly. If we
                        // see `[DONE]` here, it's a stray from a
                        // non-Responses upstream — flush the translator
                        // and end the stream.
                        let events = this.take_and_report().unwrap_or_default();
                        if events.is_empty() {
                            return Poll::Ready(None);
                        }
                        return Poll::Ready(Some(Ok(encode_sse_events(&events))));
                    }
                    if msg.data.is_empty() {
                        continue;
                    }
                    let event: responses::ResponsesStreamEvent =
                        match serde_json::from_str(&msg.data) {
                            Ok(e) => e,
                            Err(e) => {
                                tracing::warn!(
                                    error = %e,
                                    event_kind = %msg.event,
                                    "dropping malformed upstream SSE event"
                                );
                                continue;
                            }
                        };
                    let Some(translator) = this.translator.as_mut() else {
                        return Poll::Ready(None);
                    };
                    // Detect `response.failed` and route to
                    // `emit_error` so the bridge surfaces a clean
                    // error event rather than a half-streamed message.
                    if matches!(event, responses::ResponsesStreamEvent::Failed { .. }) {
                        // Best-effort message extraction; the
                        // translator will read it from the response
                        // envelope.
                        let (kind, message) = match &event {
                            responses::ResponsesStreamEvent::Failed { response } => {
                                let msg = response
                                    .error
                                    .as_ref()
                                    .map(|e| e.message.clone())
                                    .unwrap_or_else(|| "upstream response failed".to_owned());
                                ("upstream_error", msg)
                            }
                            _ => unreachable!(),
                        };
                        let events = this.take_and_report_error(kind, message);
                        return Poll::Ready(Some(Ok(encode_sse_events(&events))));
                    }
                    let events = translator.feed_event(&event);
                    if events.is_empty() {
                        continue;
                    }
                    return Poll::Ready(Some(Ok(encode_sse_events(&events))));
                }
                Poll::Ready(Some(Err(e))) => {
                    // Surface a clean close: emit an error event and end.
                    tracing::warn!(error = %e, "upstream SSE error; closing stream");
                    let events = this.take_and_report_error("api_error", format!("upstream SSE error: {e}"));
                    return Poll::Ready(Some(Ok(encode_sse_events(&events))));
                }
                Poll::Ready(None) => {
                    // Upstream closed without a terminal event — flush
                    // whatever we have. (Real Responses upstreams emit
                    // a `response.completed` / `incomplete` before
                    // closing; this branch handles the rare truncated
                    // case.)
                    if let Some(events) = this.take_and_report() {
                        return Poll::Ready(Some(Ok(encode_sse_events(&events))));
                    }
                    return Poll::Ready(None);
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

fn encode_sse_events(events: &[StreamEvent]) -> Bytes {
    let mut out = Vec::new();
    for event in events {
        let kind = event_kind(event);
        let data = match serde_json::to_string(event) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "failed to serialize stream event");
                continue;
            }
        };
        out.extend_from_slice(format!("event: {kind}\ndata: {data}\n\n").as_bytes());
    }
    Bytes::from(out)
}

fn event_kind(event: &StreamEvent) -> &'static str {
    match event {
        StreamEvent::MessageStart { .. } => "message_start",
        StreamEvent::ContentBlockStart { .. } => "content_block_start",
        StreamEvent::ContentBlockDelta { .. } => "content_block_delta",
        StreamEvent::ContentBlockStop { .. } => "content_block_stop",
        StreamEvent::MessageDelta { .. } => "message_delta",
        StreamEvent::MessageStop {} => "message_stop",
        StreamEvent::Ping {} => "ping",
        StreamEvent::Error { .. } => "error",
    }
}

// ─── Upstream error mapping ───────────────────────────────────────────────

fn map_upstream_error(status: StatusCode, body: &str) -> AppError {
    if let Ok(env) = serde_json::from_str::<responses::ResponsesError>(body) {
        // Responses errors are flat: `{ "message": "...", "type": "...", ... }`.
        // The Responses API doesn't wrap in an `error: {...}` envelope like
        // Chat Completions did, so we just read the fields directly.
        let kind = if env.kind.is_empty() {
            "upstream_error"
        } else {
            env.kind.as_str()
        };
        AppError::Upstream {
            status,
            body: format!("{kind}: {}", env.message),
        }
    } else {
        AppError::Upstream {
            status,
            body: body.to_string(),
        }
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────

// TODO(model-fallback-streaming): a streaming "model not supported"
// rejection arriving after we've already shipped message_start to the
// client is not retryable with the current design. Today the
// streaming branch skips the fallback loop and surfaces the 4xx
// directly. Revisit if it shows up in practice; the right fix is
// probably "buffer the upstream body before opening our response,
// inspect status, then start streaming the (possibly fallback) body".

/// Synthesize an Anthropic-shaped message id for streaming responses.
/// Uses nanoseconds since the UNIX epoch, hex-encoded, plus a sanitized
/// model fragment. Not a UUID, but unique enough for our purposes.
/// The translator replaces this with the real `resp_...` id once
/// `response.created` arrives.
fn synthetic_message_id(req: &CreateMessageRequest) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("msg_{:x}_{}", nanos, sanitize_model(&req.model))
}

fn sanitize_model(model: &str) -> String {
    model
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthetic_message_id_uses_msg_prefix() {
        let req = CreateMessageRequest {
            model: "gpt-4o".into(),
            max_tokens: 16,
            messages: vec![],
            system: None,
            tools: None,
            tool_choice: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: Some(true),
            metadata: None,
        };
        let id = synthetic_message_id(&req);
        assert!(id.starts_with("msg_"));
        assert!(id.contains("gpt-4o"));
    }

    #[test]
    fn sanitize_model_keeps_safe_chars() {
        assert_eq!(sanitize_model("gpt-4o"), "gpt-4o");
        assert_eq!(sanitize_model("claude/opus 4.8"), "claude_opus_4_8");
    }

    #[test]
    fn is_model_not_supported_matches_known_phrases() {
        // Phrases the airia / OpenAI / Anthropic envelopes use when
        // a model is unknown. The matcher is case-insensitive.
        let cases = [
            (
                StatusCode::BAD_REQUEST,
                r#"{"error":{"code":"model_not_found","message":"..."}}"#,
            ),
            (StatusCode::NOT_FOUND, "Model not found"),
            (
                StatusCode::BAD_REQUEST,
                "The requested model X is not supported for the selected provider.",
            ),
            (StatusCode::BAD_REQUEST, "Unknown model: foo"),
            (StatusCode::BAD_REQUEST, "MODEL NOT SUPPORTED"),
        ];
        for (status, body) in cases {
            assert!(
                is_model_not_supported(status, body),
                "expected true for status={status} body={body:?}",
            );
        }
    }

    #[test]
    fn is_model_not_supported_rejects_unrelated_errors() {
        // 500s, 429s, and 400s about other things (rate limits,
        // bad params, missing API key) must not be classified as
        // model-not-supported — the fallback path would mask the
        // real problem.
        let cases = [
            (StatusCode::INTERNAL_SERVER_ERROR, "upstream exploded"),
            (StatusCode::TOO_MANY_REQUESTS, "rate limit exceeded"),
            (StatusCode::UNAUTHORIZED, "invalid api key"),
            (StatusCode::BAD_REQUEST, "max_tokens is too high"),
            (StatusCode::SERVICE_UNAVAILABLE, "overloaded"),
        ];
        for (status, body) in cases {
            assert!(
                !is_model_not_supported(status, body),
                "expected false for status={status} body={body:?}",
            );
        }
    }

    #[test]
    fn is_model_not_supported_detects_flat_responses_code() {
        // Responses API errors are flat: { "code": "...", ... }.
        // We must trigger fallback on model_not_supported / model_not_found
        // even when the message text is generic.
        let body = r#"{"message":"Bad request","type":"invalid_request_error","code":"model_not_supported"}"#;
        assert!(is_model_not_supported(StatusCode::BAD_REQUEST, body));
    }

    #[test]
    fn is_model_not_supported_detects_wrapped_chat_completions_code() {
        // Chat Completions-style errors wrap the code inside `error`.
        let body = r#"{"error":{"message":"Not found","type":"invalid_request_error","code":"model_not_found"}}"#;
        assert!(is_model_not_supported(StatusCode::BAD_REQUEST, body));
    }

    #[test]
    fn extract_model_error_code_returns_none_for_non_json() {
        assert_eq!(extract_model_error_code("not json"), None);
    }

    #[test]
    fn extract_model_error_code_prefers_flat_code() {
        let body = r#"{"code":"model_not_supported","message":"Bad request"}"#;
        assert_eq!(
            extract_model_error_code(body).as_deref(),
            Some("model_not_supported")
        );
    }

    #[test]
    fn extract_model_error_code_falls_back_to_wrapped_code() {
        let body = r#"{"error":{"code":"model_not_found","message":"Not found"}}"#;
        assert_eq!(
            extract_model_error_code(body).as_deref(),
            Some("model_not_found")
        );
    }
}
