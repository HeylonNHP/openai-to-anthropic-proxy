//! Start a streaming response before the upstream has answered.
//!
//! A streaming client that hears nothing for minutes aborts the turn (see
//! the keep-alive note on [`crate::proxy::PING_INTERVAL`]), and the upstream
//! answer can be that far away: the request may still be queueing in the
//! upstream gate or sleeping through rate-limit backoff, and neither runs
//! under any timeout. So a streaming request whose upstream has not
//! answered within [`START_AFTER`] starts anyway — HTTP 200, an empty
//! placeholder `message_start`, then `ping` events while the wait
//! continues. When the upstream finally answers, its bytes splice in
//! behind the placeholder.
//!
//! The deadline doubles as the error channel: a failure the upstream
//! reports *within* [`START_AFTER`] is still returned as a real HTTP
//! status, which clients retry with backoff. Past the deadline the status
//! line is already on the wire, so a failure can only surface as an
//! in-stream `error` event — a weaker signal. The cut-off therefore stays
//! at one ping cadence: late enough that ordinary failures keep their
//! status, early enough that the client is never silent near its byte
//! watchdog.

use std::pin::Pin;
use std::time::Duration;

use axum::body::Body;
use axum::http::{StatusCode, header};
use axum::response::Response;
use bytes::Bytes;
use futures_util::{Stream, StreamExt};

use crate::anthropic::{Message, StreamEvent};
use crate::tools::advisor::sse::start_event;

/// How long a streaming request may wait for the upstream's answer before
/// the response starts anyway with a placeholder `message_start`. One ping
/// cadence: after committing, the client hears a `ping` at least this
/// often, and before committing it is silent for at most this long — both
/// far inside the client's byte watchdog.
pub const START_AFTER: Duration = crate::proxy::PING_INTERVAL;

/// The `message_start` sent when the stream starts before the upstream has
/// answered: an empty assistant message for the model the client asked
/// for. Its usage is zero; the closing `message_delta` carries the turn's
/// real usage.
pub fn placeholder_message(model: &str) -> Message {
    let id = crate::tools::web_fetch::protocol::new_id();
    Message {
        id: format!("msg_{}", id.trim_start_matches("srvtoolu_")),
        r#type: "message",
        role: "assistant",
        content: Vec::new(),
        model: model.to_owned(),
        stop_reason: None,
        stop_sequence: None,
        usage: crate::anthropic::Usage::default(),
    }
}

/// The wire frame for a placeholder `message_start`.
pub fn placeholder_frame(message: &Message) -> Bytes {
    crate::proxy::encode_sse_events(&[start_event(message)])
}

/// Cancels a spawned task when dropped.
///
/// The upstream call runs in its own task so the response can start while it
/// is still waiting. A plain `tokio::spawn` detaches that task: if the client
/// disconnects meanwhile, it would finish an upstream request nobody will
/// read, holding a concurrency slot and spending upstream capacity. Holding
/// this guard for exactly as long as someone is waiting on the task restores
/// the old behaviour, where dropping the handler cancelled the request.
/// Aborting a task that has already finished is a no-op.
pub struct AbortOnDrop(tokio::task::AbortHandle);

impl AbortOnDrop {
    /// Spawn `future` and tie its lifetime to the returned guard.
    pub fn spawn<F>(future: F) -> Self
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        Self(tokio::spawn(future).abort_handle())
    }
}

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// A committed streaming response: `first` (the placeholder frame) goes
/// out immediately — with the headers, since axum writes them with the
/// first chunk — then every chunk `source` yields, with a `ping` whenever
/// `source` stays silent for a full ping interval. The body ends when
/// `source` does.
pub fn keep_alive_response<S>(first: Bytes, source: S) -> Response
where
    S: Stream<Item = Bytes> + Send + 'static,
{
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(
            keep_alive_stream(first, source).map(Ok::<Bytes, std::io::Error>),
        ))
        .expect("static headers are valid")
}

/// The body stream behind [`keep_alive_response`]: the first frame, then
/// `source` interleaved with `ping`s while it is quiet. A fresh sleep is
/// started on every pass, so any chunk from `source` resets the cadence.
fn keep_alive_stream<S>(first: Bytes, source: S) -> impl Stream<Item = Bytes> + Send
where
    S: Stream<Item = Bytes> + Send + 'static,
{
    let state = KeepAlive {
        first: Some(first),
        source: Box::pin(source),
    };
    futures_util::stream::unfold(state, next_chunk)
}

struct KeepAlive<S> {
    /// The placeholder frame, sent before the source is polled at all.
    first: Option<Bytes>,
    source: Pin<Box<S>>,
}

async fn next_chunk<S>(mut state: KeepAlive<S>) -> Option<(Bytes, KeepAlive<S>)>
where
    S: Stream<Item = Bytes>,
{
    if let Some(first) = state.first.take() {
        return Some((first, state));
    }
    let chunk = tokio::select! {
        item = state.source.next() => match item {
            Some(chunk) => chunk,
            // The splice finished: end the body (after the frames the
            // source itself closed with).
            None => return None,
        },
        () = tokio::time::sleep(crate::proxy::PING_INTERVAL) => {
            crate::proxy::encode_sse_events(&[StreamEvent::Ping {}])
        }
    };
    Some((chunk, state))
}

#[cfg(test)]
mod tests;
