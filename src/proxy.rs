//! axum router and request handlers.
//!
//! Wires together: read Anthropic request → translate to OpenAI →
//! forward to upstream → translate response (or stream) back → ship to
//! Claude Code.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{SystemTime, UNIX_EPOCH};

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
use crate::openai;
use crate::stream::StreamTranslator;
use crate::translate;
use eventsource_stream::Event as SseEvent;
use eventsource_stream::EventStreamError;

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
    let mut outbound = translate::anthropic_to_openai(&req, reasoning_effort)
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

    tracing::debug!(
        model = %outbound.model,
        stream = outbound.stream,
        tools = outbound.tools.as_ref().map_or(0, Vec::len),
        messages = outbound.messages.len(),
        max_completion_tokens = ?outbound.max_completion_tokens,
        reasoning_effort = ?outbound.reasoning_effort,
        "→ upstream"
    );

    LAST_SENT_BODY
        .scope(
            body_json,
            handle_messages_inner(state, headers, req, outbound),
        )
        .await
}

async fn handle_messages_inner(
    state: AppState,
    headers: HeaderMap,
    req: CreateMessageRequest,
    outbound: openai::ChatCompletionRequest,
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
        tracing::warn!(%status, body = %body, sent_body = %sent, "← upstream error");

        // Decide whether to retry with the default model. Only
        // non-streaming requests get the retry — see TODO at end
        // of file. Only one retry; the loop ends after attempt 2
        // regardless.
        let should_retry = !outbound.stream
            && attempt == 1
            && is_model_not_supported(status, &body)
            && state
                .config
                .default_model()
                .is_some_and(|fb| fb != outbound.model);

        if should_retry {
            let fallback = state.config.default_model().unwrap();
            tracing::warn!(
                inbound_model = %outbound.model,
                fallback_model = %fallback,
                "upstream rejected model; falling back to default_model"
            );
            outbound.model = fallback.to_owned();
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
        let translator_stream = TranslatorStream::new(sse, msg_id, model);

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
        let openai_resp: openai::ChatCompletionResponse =
            serde_json::from_slice(&bytes).map_err(|e| AppError::Upstream {
                status: StatusCode::BAD_GATEWAY,
                body: format!("upstream returned non-JSON body: {e}"),
            })?;
        let anth = translate::openai_to_anthropic(&openai_resp);
        Ok(Json(anth).into_response())
    }
}

/// Cheap heuristic for "the upstream doesn't know this model".
/// Matches on (a) status in `[400, 404]` and (b) body containing
/// one of the well-known phrases from airia / OpenAI / Anthropic
/// style error envelopes. Conservative by design: false negatives
/// are fine (the operator sees the original error), false positives
/// would mask unrelated 400s.
fn is_model_not_supported(status: StatusCode, body: &str) -> bool {
    if !matches!(status.as_u16(), 400 | 404) {
        return false;
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
}

impl<S> TranslatorStream<S>
where
    S: Stream<Item = Result<SseEvent, EventStreamError<reqwest::Error>>> + Unpin,
{
    fn new(inner: S, msg_id: String, model: String) -> Self {
        Self {
            inner,
            translator: Some(StreamTranslator::new(msg_id, model)),
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
            // The translator is `None` after a clean close ([DONE], an
            // error event, or a truncated stream). Any further event
            // from the inner stream is a stray — drop it and end the
            // outer stream. This used to be three separate `expect()`
            // calls, each of which could panic and bring down the
            // worker.
            if this.translator.is_none() {
                match this.inner.poll_next_unpin(cx) {
                    Poll::Ready(Some(_)) => continue,
                    _ => return Poll::Ready(None),
                }
            }
            match this.inner.poll_next_unpin(cx) {
                Poll::Ready(Some(Ok(msg))) => {
                    if msg.data.trim() == "[DONE]" {
                        // Flush the translator and emit the closing events.
                        let Some(translator) = this.translator.take() else {
                            // Already closed; should be unreachable thanks
                            // to the guard above, but stay defensive.
                            return Poll::Ready(None);
                        };
                        let events = translator.finish();
                        if events.is_empty() {
                            return Poll::Ready(None);
                        }
                        return Poll::Ready(Some(Ok(encode_sse_events(&events))));
                    }
                    if msg.data.is_empty() {
                        continue;
                    }
                    let chunk: openai::ChatCompletionChunk = match serde_json::from_str(&msg.data) {
                        Ok(c) => c,
                        Err(e) => {
                            tracing::warn!(error = %e, "dropping malformed upstream SSE chunk");
                            continue;
                        }
                    };
                    let Some(translator) = this.translator.as_mut() else {
                        // Translator was already taken (e.g. the
                        // upstream sent [DONE] then a real event).
                        return Poll::Ready(None);
                    };
                    let events = translator.feed_chunk(&chunk);
                    if events.is_empty() {
                        continue;
                    }
                    return Poll::Ready(Some(Ok(encode_sse_events(&events))));
                }
                Poll::Ready(Some(Err(e))) => {
                    // Surface a clean close: emit message_stop and end.
                    tracing::warn!(error = %e, "upstream SSE error; closing stream");
                    if let Some(t) = this.translator.take() {
                        let events = t.emit_error("api_error", format!("upstream SSE error: {e}"));
                        return Poll::Ready(Some(Ok(encode_sse_events(&events))));
                    }
                    return Poll::Ready(None);
                }
                Poll::Ready(None) => {
                    // Upstream closed without a [DONE] — flush whatever we have.
                    if let Some(t) = this.translator.take() {
                        let events = t.finish();
                        if !events.is_empty() {
                            return Poll::Ready(Some(Ok(encode_sse_events(&events))));
                        }
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
    if let Ok(env) = serde_json::from_str::<openai::ErrorEnvelope>(body) {
        AppError::Upstream {
            status,
            body: format!("{}: {}", env.error.kind, env.error.message),
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
}
