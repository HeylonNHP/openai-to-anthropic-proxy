//! One executor run, streamed.
//!
//! A streaming client of a server-tool turn used to wait for each executor
//! run to finish before it saw a byte of it, because the loop must inspect the
//! whole run for calls to its own tools. This module reads the run's upstream
//! SSE stream instead, and does both at once:
//!
//! * every event goes through a [continuation translator]
//!   (`StreamTranslator::continuation`) and what it produces goes straight to
//!   the client, so text and reasoning appear as they are generated. Calls to
//!   the loop's own tools are dropped by the translator: the client never
//!   sees them as `tool_use`;
//! * the same events are collected into the [`ResponsesResponse`] the run
//!   amounts to, which the loop then inspects for calls to serve, replays
//!   into the next run's input and sums into the turn's usage.
//!
//! ## What the collected response holds
//!
//! The terminal event's own `output` is used when it has one. Many upstreams
//! send it empty, and the loop needs more than the text: a reasoning item's
//! `encrypted_content` and a call's arguments are what the next run is
//! replayed from. So the items are also built up from `output_item.added`,
//! the deltas, and `output_item.done` (the authoritative copy when it has
//! content).
//!
//! ## However the run ends, its blocks are closed
//!
//! A failure, a vanished upstream or a client that went away all leave the
//! client's stream well-formed: every block that was opened is closed before
//! the error is returned, because a `content_block_start` with no
//! `content_block_stop` breaks a client. What was already streamed stays.

use std::collections::BTreeMap;
use std::fmt::Display;

use eventsource_stream::Event as SseEvent;
use futures_util::{Stream, StreamExt};

use crate::error::AppError;
use crate::repair::ToolSchemaRegistry;
use crate::responses::{
    OutputContentPart, OutputItem, ReasoningSummaryText, ResponsesResponse, ResponsesStreamEvent,
};
use crate::stream::StreamTranslator;
use crate::tools::server_loop::run::Emitter;

/// What a run needs besides its events.
pub(crate) struct RunContext<'a> {
    /// The id the client's message already has.
    pub msg_id: String,
    /// The upstream model name.
    pub model: String,
    /// The client's tool schemas, to repair its tool calls' arguments.
    pub registry: ToolSchemaRegistry,
    /// The loop's own tools, which the client must not see.
    pub hidden: &'a [&'static str],
}

/// Read one run's events, sending the client what they translate to as they
/// arrive, and return the run as a response.
///
/// # Errors
///
/// The upstream sent a failure, the stream broke or ended before a terminal
/// event, or the client went away. The client's blocks are closed first.
pub(crate) async fn consume<S, E>(
    mut events: S,
    out: &mut Emitter<'_>,
    ctx: RunContext<'_>,
) -> Result<ResponsesResponse, AppError>
where
    S: Stream<Item = Result<SseEvent, E>> + Unpin,
    E: Display,
{
    let mut translator = StreamTranslator::continuation(
        ctx.msg_id,
        ctx.model,
        ctx.registry,
        out.next_index(),
        ctx.hidden.iter().map(|name| (*name).to_owned()).collect(),
    );
    let mut items = Items::default();
    let result = read(&mut events, out, &mut translator, &mut items).await;
    // Close whatever is open, on success and failure alike.
    let closing = translator.close_blocks();
    out.send_live(closing, translator.next_index());
    result.map(|mut resp| {
        if resp.output.is_empty() {
            resp.output = items.into_output();
        }
        resp
    })
}

/// The event loop of [`consume`]: the terminal response, or why there is none.
async fn read<S, E>(
    events: &mut S,
    out: &mut Emitter<'_>,
    translator: &mut StreamTranslator,
    items: &mut Items,
) -> Result<ResponsesResponse, AppError>
where
    S: Stream<Item = Result<SseEvent, E>> + Unpin,
    E: Display,
{
    loop {
        let next = tokio::select! {
            next = events.next() => next,
            () = out.closed() => {
                return Err(AppError::Internal(
                    "the client disconnected during a server-tool turn".to_owned(),
                ));
            }
        };
        let message = match next {
            Some(Ok(message)) => message,
            Some(Err(err)) => {
                return Err(AppError::upstream_failure(&format!(
                    "upstream SSE error: {err}"
                )));
            }
            None => {
                return Err(AppError::upstream_failure(
                    "upstream stream ended before a terminal response event",
                ));
            }
        };
        if message.data.is_empty() {
            continue;
        }
        if message.data.trim() == "[DONE]" {
            return Err(AppError::upstream_failure(
                "upstream stream ended before a terminal response event",
            ));
        }
        let event: ResponsesStreamEvent = match serde_json::from_str(&message.data) {
            Ok(event) => event,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    event_kind = %message.event,
                    "dropping malformed upstream SSE event"
                );
                continue;
            }
        };
        match &event {
            ResponsesStreamEvent::Failed { response } => {
                let Some(error) = response.error.as_ref() else {
                    return Err(AppError::upstream_failure("upstream response failed"));
                };
                return Err(AppError::reported_failure(
                    &crate::responses::code_text(error.code.as_ref()),
                    &error.message,
                ));
            }
            ResponsesStreamEvent::Error { message, code, .. } => {
                return Err(AppError::reported_failure(
                    code.as_deref().unwrap_or_default(),
                    message,
                ));
            }
            ResponsesStreamEvent::Completed { response }
            | ResponsesStreamEvent::Incomplete { response } => {
                let produced = translator.feed_event(&event);
                out.send_live(produced, translator.next_index());
                return Ok(response.clone());
            }
            _ => {}
        }
        items.observe(&event);
        let produced = translator.feed_event(&event);
        out.send_live(produced, translator.next_index());
    }
}

/// The output items of a run, built up from its events.
#[derive(Default)]
struct Items {
    by_index: BTreeMap<u32, OutputItem>,
}

impl Items {
    fn observe(&mut self, event: &ResponsesStreamEvent) {
        match event {
            ResponsesStreamEvent::OutputItemAdded { output_index, item } => {
                self.by_index.insert(*output_index, item.clone());
            }
            ResponsesStreamEvent::OutputItemDone { output_index, item } => {
                match self.by_index.get_mut(output_index) {
                    Some(have) => merge_done(have, item),
                    None => {
                        self.by_index.insert(*output_index, item.clone());
                    }
                }
            }
            ResponsesStreamEvent::OutputTextDelta {
                output_index,
                delta,
                ..
            }
            | ResponsesStreamEvent::RefusalDelta {
                output_index,
                delta,
                ..
            } => {
                if let Some(OutputItem::Message { content, .. }) =
                    self.by_index.get_mut(output_index)
                {
                    append_text(content, delta);
                }
            }
            ResponsesStreamEvent::FunctionCallArgumentsDelta {
                output_index,
                delta,
                ..
            } => {
                if let Some(OutputItem::FunctionCall { arguments, .. }) =
                    self.by_index.get_mut(output_index)
                {
                    arguments.push_str(delta);
                }
            }
            ResponsesStreamEvent::FunctionCallArgumentsDone {
                output_index,
                arguments: complete,
                ..
            } => {
                if let Some(OutputItem::FunctionCall { arguments, .. }) =
                    self.by_index.get_mut(output_index)
                    && !complete.is_empty()
                {
                    complete.clone_into(arguments);
                }
            }
            ResponsesStreamEvent::ReasoningSummaryTextDelta {
                output_index,
                delta,
                ..
            } => {
                if let Some(OutputItem::Reasoning { summary, .. }) =
                    self.by_index.get_mut(output_index)
                {
                    match summary.last_mut() {
                        Some(part) => part.text.push_str(delta),
                        None => summary.push(ReasoningSummaryText {
                            text: delta.clone(),
                            kind: Some("summary_text".to_owned()),
                        }),
                    }
                }
            }
            _ => {}
        }
    }

    fn into_output(self) -> Vec<OutputItem> {
        self.by_index.into_values().collect()
    }
}

/// Add streamed text to the message's last text part (or open one).
fn append_text(content: &mut Vec<OutputContentPart>, delta: &str) {
    if let Some(OutputContentPart::OutputText { text, .. }) = content.last_mut() {
        text.push_str(delta);
    } else {
        content.push(OutputContentPart::OutputText {
            text: delta.to_owned(),
            annotations: Vec::new(),
        });
    }
}

/// `output_item.done` is the authoritative copy of an item, but some upstreams
/// send it without the content they streamed. Adopt it, keeping what was
/// accumulated wherever it came empty.
fn merge_done(have: &mut OutputItem, done: &OutputItem) {
    let mut merged = done.clone();
    match (&mut merged, &*have) {
        (
            OutputItem::Message { content, .. },
            OutputItem::Message {
                content: streamed, ..
            },
        ) if !has_text(content) => content.clone_from(streamed),
        (
            OutputItem::FunctionCall { arguments, .. },
            OutputItem::FunctionCall {
                arguments: streamed,
                ..
            },
        ) if arguments.is_empty() => arguments.clone_from(streamed),
        (
            OutputItem::Reasoning { summary, .. },
            OutputItem::Reasoning {
                summary: streamed, ..
            },
        ) if summary.is_empty() => summary.clone_from(streamed),
        _ => {}
    }
    *have = merged;
}

fn has_text(content: &[OutputContentPart]) -> bool {
    content.iter().any(|part| match part {
        OutputContentPart::OutputText { text, .. } => !text.is_empty(),
        OutputContentPart::OutputRefusal { refusal } => !refusal.is_empty(),
        _ => false,
    })
}

#[cfg(test)]
mod tests;
