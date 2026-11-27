//! Streaming a server-tool turn to the client as it happens.
//!
//! Each executor run is streamed upstream and forwarded as it is generated
//! (see [`crate::tools::server_loop::live`]); between runs the blocks of the
//! served tool calls are sent, `ping` events keep the connection alive while a
//! run is quiet, and the closing `message_delta` follows the last block:
//!
//! ```text
//! upstream answered ─▶ message_start (placeholder)
//! each run          ─▶ content_block_start / delta / stop … (live)
//! each served call  ─▶ server_tool_use, result
//! (idle)            ─▶ ping, ping …
//! last run done     ─▶ message_delta, message_stop
//! ```
//!
//! The response starts once the first executor run has been ANSWERED with a
//! success status (not finished), so a quick failure there is still an ordinary
//! HTTP error with its proper status, which clients retry on. If the upstream
//! has not answered within [`crate::early_start::START_AFTER`] the stream
//! starts anyway with the same placeholder `message_start` and `ping`s — the
//! shared early-start body ([`crate::early_start`]) keeps the client supplied
//! with bytes — so the client is never silent for longer than the ping
//! interval:
//!
//! ```text
//! first run answered within START_AFTER ─▶ message_start (placeholder)
//! still waiting at START_AFTER          ─▶ message_start (placeholder), ping …
//! ```
//!
//! Once the stream has started a failure cannot change the status any more
//! and is sent as an SSE `error` event. In practice a failed re-run does not
//! even get that far: it degrades to the work done so far (see
//! [`crate::tools::server_loop::run`]).
//!
//! The loop runs in its own task and talks to the response body over a
//! channel, so a slow client never stalls it and a vanished client stops it
//! (the loop notices the closed channel before its next upstream call, and a
//! run being read stops at once).

// `tools` is a private module, so these crate-visible items look redundant to
// clippy; they must be `pub(crate)` because they name `proxy::Attempt`.
#![allow(clippy::redundant_pub_crate)]

use std::time::Duration;

use axum::response::Response;
use bytes::Bytes;
use futures_util::Stream;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::anthropic::{Message, ResponseContentBlock, StreamEvent};
use crate::error::AppError;
use crate::proxy::{Attempt, encode_sse_events};
use crate::tools::advisor::sse::{block_events, end_events};
use crate::tools::server_loop::handler::ServerTool;
use crate::tools::server_loop::render::{Tail, finish_streaming};
use crate::tools::server_loop::run::{LoopEvent, SessionOutcome, drive};

// The placeholder message comes from the shared early-start module, under
// the path `run.rs` already names.
pub(crate) use crate::early_start::placeholder_message;

/// What [`respond`] made of the request.
pub(crate) enum Streamed {
    /// The streaming response.
    Response(Response),
    /// The upstream streamed although the request was buffered, before any
    /// work was done: render its stream the ordinary way.
    Fallback(Box<Attempt>, crate::upstream_gate::GatedResponse),
}

/// Run the turn for a streaming client.
///
/// # Errors
///
/// The first executor run failed: an ordinary HTTP error, before any byte of
/// the stream is sent.
pub(crate) async fn respond(
    attempt: Attempt,
    tools: Vec<Box<dyn ServerTool>>,
) -> Result<Streamed, AppError> {
    let placeholder = placeholder_message(&attempt.req.model);
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(run_turn(attempt, tools, tx));
    start(rx, placeholder, crate::early_start::START_AFTER).await
}

/// Wait for the first executor run, but no longer than `start_after`: past
/// that the stream starts with `placeholder` as its `message_start`.
pub(crate) async fn start(
    mut rx: UnboundedReceiver<LoopEvent>,
    placeholder: Message,
    start_after: Duration,
) -> Result<Streamed, AppError> {
    let Ok(first) = tokio::time::timeout(start_after, rx.recv()).await else {
        tracing::debug!("first executor run is slow; starting the stream early");
        return Ok(Streamed::Response(response(placeholder, rx)));
    };
    match first {
        Some(LoopEvent::Started(message)) => Ok(Streamed::Response(response(message, rx))),
        Some(LoopEvent::Failed(err)) => Err(err),
        Some(LoopEvent::Fallback(attempt, raw)) => Ok(Streamed::Fallback(attempt, raw)),
        Some(LoopEvent::Blocks(_) | LoopEvent::Live(..) | LoopEvent::Finished(_)) | None => Err(
            AppError::Internal("the server-tool turn ended without starting".to_owned()),
        ),
    }
}

/// The task that owns the request for the whole turn.
async fn run_turn(
    mut attempt: Attempt,
    tools: Vec<Box<dyn ServerTool>>,
    tx: UnboundedSender<LoopEvent>,
) {
    let event = match drive(&mut attempt, tools, Some(&tx)).await {
        Ok(SessionOutcome::Complete(result)) => {
            LoopEvent::Finished(finish_streaming(&attempt, *result))
        }
        Ok(SessionOutcome::UnexpectedStreaming(raw)) => LoopEvent::Fallback(Box::new(attempt), raw),
        Err(err) => LoopEvent::Failed(err),
    };
    // The receiver is gone when the client disconnected: nothing to tell.
    let _ = tx.send(event);
}

/// The SSE response: `message_start`, then whatever the loop sends — over
/// the shared keep-alive body, which interleaves `ping`s while the loop is
/// quiet.
fn response(message: Message, rx: UnboundedReceiver<LoopEvent>) -> Response {
    let first = crate::early_start::placeholder_frame(&message);
    crate::early_start::keep_alive_response(first, loop_events(rx))
}

/// The loop's events as SSE bytes for the keep-alive body: blocks and live
/// runs as they arrive, the closing tail, failures as an in-stream `error`
/// — and a `Started` discarded, because the placeholder already opened the
/// message. Block numbering lives here (`Live` carries its own index, as it
/// did when the body polled the channel itself).
fn loop_events(rx: UnboundedReceiver<LoopEvent>) -> impl Stream<Item = Bytes> + Send {
    let state = LoopSource {
        rx,
        next_index: 0,
        done: false,
    };
    futures_util::stream::unfold(state, |mut state| async move {
        if state.done {
            return None;
        }
        let events = loop {
            let Some(event) = state.rx.recv().await else {
                state.done = true;
                break vec![error_event(&AppError::Internal(
                    "the server-tool turn ended unexpectedly".to_owned(),
                ))];
            };
            match event {
                LoopEvent::Blocks(blocks) => {
                    break block_stream_events(&mut state.next_index, blocks);
                }
                LoopEvent::Live(events, next_index) => {
                    state.next_index = next_index;
                    break events;
                }
                LoopEvent::Finished(tail) => {
                    state.done = true;
                    break tail_stream_events(&mut state.next_index, tail);
                }
                LoopEvent::Failed(err) => {
                    state.done = true;
                    break vec![error_event(&err)];
                }
                // The stream was started early with a placeholder: the real
                // `message_start` is not sent a second time.
                LoopEvent::Started(_) => {}
                // The stream was started early, and the upstream then
                // answered with a stream although a whole response was
                // asked for. It cannot be spliced into this stream.
                LoopEvent::Fallback(..) => {
                    state.done = true;
                    break vec![error_event(&AppError::Internal(
                        "the upstream streamed a response that was requested whole".to_owned(),
                    ))];
                }
            }
        };
        Some((encode_sse_events(&events), state))
    })
}

/// What the [`loop_events`] unfold carries between chunks.
struct LoopSource {
    rx: UnboundedReceiver<LoopEvent>,
    /// The index the next content block gets.
    next_index: u32,
    /// Set once a terminal event has been yielded: nothing follows it.
    done: bool,
}

/// The events for `blocks`, numbering them from `*next_index`.
pub(crate) fn block_stream_events(
    next_index: &mut u32,
    blocks: Vec<ResponseContentBlock>,
) -> Vec<StreamEvent> {
    let mut events = Vec::new();
    for block in blocks {
        events.extend(block_events(*next_index, block));
        *next_index += 1;
    }
    events
}

/// The events that close the stream: the blocks not sent, then the
/// `message_delta` and `message_stop`.
pub(crate) fn tail_stream_events(next_index: &mut u32, tail: Tail) -> Vec<StreamEvent> {
    let Tail {
        blocks,
        stop_reason,
        stop_sequence,
        usage,
    } = tail;
    let mut events = block_stream_events(next_index, blocks);
    events.extend(end_events(stop_reason, stop_sequence, usage));
    events
}

/// An SSE `error` event for a failure after the stream has started.
pub(crate) fn error_event(err: &AppError) -> StreamEvent {
    StreamEvent::Error {
        error: err.to_api_error_body(),
    }
}

#[cfg(test)]
mod tests;
