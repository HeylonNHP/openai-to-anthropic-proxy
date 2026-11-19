//! Streaming a server-tool turn to the client as it happens.
//!
//! Each executor run is still fetched whole (a model's function call cannot be
//! served until it is complete), but the CLIENT does not have to wait for the
//! turn to end. As each round finishes, the blocks it produced (the model's
//! text, `server_tool_use`, the result) are sent at once, `ping` events keep
//! the connection alive while a later run is in flight, and the closing
//! `message_delta` follows the last block:
//!
//! ```text
//! first run done ─▶ message_start
//! each round     ─▶ content_block_start / delta / stop …
//! (idle)         ─▶ ping, ping …
//! last run done  ─▶ final blocks, message_delta, message_stop
//! ```
//!
//! The response only starts once the FIRST executor run has succeeded, so a
//! failure there is still an ordinary HTTP error with its proper status (which
//! clients retry on), exactly as before. After that a failure cannot change
//! the status any more and is sent as an SSE `error` event. In practice a
//! failed re-run does not even get that far: it degrades to the work done so
//! far (see [`crate::tools::server_loop::run`]).
//!
//! The loop runs in its own task and talks to the response body over a
//! channel, so a slow client never stalls it and a vanished client stops it
//! (the loop notices the closed channel before its next upstream call).

// `tools` is a private module, so these crate-visible items look redundant to
// clippy; they must be `pub(crate)` because they name `proxy::Attempt`.
#![allow(clippy::redundant_pub_crate)]

use std::io;
use std::time::Duration;

use axum::body::Body;
use axum::http::{StatusCode, header};
use axum::response::Response;
use bytes::Bytes;
use futures_util::StreamExt;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::anthropic::{Message, ResponseContentBlock, StreamEvent};
use crate::error::AppError;
use crate::proxy::{Attempt, encode_sse_events};
use crate::tools::advisor::sse::{block_events, end_events, start_event};
use crate::tools::server_loop::handler::ServerTool;
use crate::tools::server_loop::render::{Tail, finish_streaming};
use crate::tools::server_loop::run::{LoopEvent, SessionOutcome, drive};

/// How long the stream may be silent before a `ping` is sent.
const PING_INTERVAL: Duration = Duration::from_secs(10);

/// What [`respond`] made of the request.
pub(crate) enum Streamed {
    /// The streaming response.
    Response(Response),
    /// The upstream streamed although the request was buffered, before any
    /// work was done: render its stream the ordinary way.
    Fallback(Box<Attempt>, reqwest::Response),
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
    let (tx, mut rx) = mpsc::unbounded_channel();
    tokio::spawn(run_turn(attempt, tools, tx));

    match rx.recv().await {
        Some(LoopEvent::Started(message)) => Ok(Streamed::Response(response(message, rx))),
        Some(LoopEvent::Failed(err)) => Err(err),
        Some(LoopEvent::Fallback(attempt, raw)) => Ok(Streamed::Fallback(attempt, raw)),
        Some(LoopEvent::Blocks(_) | LoopEvent::Finished(_)) | None => Err(AppError::Internal(
            "the server-tool turn ended without starting".to_owned(),
        )),
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

/// The SSE response: `message_start`, then whatever the loop sends.
fn response(message: Message, rx: UnboundedReceiver<LoopEvent>) -> Response {
    let first = encode_sse_events(&[start_event(&message)]);
    let state = BodyState {
        rx,
        next_index: 0,
        done: false,
        ping_after: PING_INTERVAL,
    };
    let rest = futures_util::stream::unfold(state, next_chunk);
    let body = futures_util::stream::once(std::future::ready(Ok::<Bytes, io::Error>(first)))
        .chain(rest.map(Ok::<Bytes, io::Error>));
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(body))
        .expect("static headers are valid")
}

struct BodyState {
    rx: UnboundedReceiver<LoopEvent>,
    /// The index of the next content block.
    next_index: u32,
    done: bool,
    /// How long the stream may be silent before a `ping`.
    ping_after: Duration,
}

/// The next chunk of the body: the loop's next event as SSE, or a `ping` if it
/// stays quiet. `None` ends the stream.
async fn next_chunk(mut state: BodyState) -> Option<(Bytes, BodyState)> {
    if state.done {
        return None;
    }
    let events = loop {
        tokio::select! {
            event = state.rx.recv() => match event {
                Some(LoopEvent::Blocks(blocks)) => {
                    break block_stream_events(&mut state.next_index, blocks);
                }
                Some(LoopEvent::Finished(tail)) => {
                    state.done = true;
                    break tail_stream_events(&mut state.next_index, tail);
                }
                Some(LoopEvent::Failed(err)) => {
                    state.done = true;
                    break vec![error_event(&err)];
                }
                // Neither is sent after the start; ignore rather than guess.
                Some(LoopEvent::Started(_) | LoopEvent::Fallback(..)) => {}
                None => {
                    state.done = true;
                    break vec![error_event(&AppError::Internal(
                        "the server-tool turn ended unexpectedly".to_owned(),
                    ))];
                }
            },
            () = tokio::time::sleep(state.ping_after) => break vec![StreamEvent::Ping {}],
        }
    };
    Some((encode_sse_events(&events), state))
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

/// The events that close the stream: the blocks not yet sent, then the
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
