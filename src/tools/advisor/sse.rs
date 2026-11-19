//! Buffered `Message` → synthetic Anthropic SSE events (§11.3 of
//! `ADVISOR_TOOL_RESEARCH.md`).
//!
//! When the advisor is enabled, the proxy will force the *upstream* request
//! to `stream: false`, run the whole consultation chain through the existing
//! non-streaming pipeline, then convert the final [`Message`] into one SSE
//! sequence. That avoids splicing a second upstream response into
//! `TranslatorStream` (high risk to every non-advisor streaming request) and
//! matches Anthropic's own behaviour: **the advisor sub-inference does not
//! stream**. The executor's stream pauses and the result arrives whole.
//!
//! This is a pure adapter: no I/O, no timers, no `[DONE]` sentinel, and no
//! early `message_start` (there is no idle gap in a buffered response, so a
//! keepalive state machine is unnecessary).

use serde_json::Value;

use crate::anthropic::{
    ContentBlockKind, ContentDelta, Message, MessageDeltaPayload, ResponseContentBlock, StreamEvent,
};

/// Turn one complete, buffered [`Message`] into a well-formed streaming
/// event sequence.
///
/// Invariants:
/// - exactly one `message_start` (with an empty `content[]` and the input
///   usage) and one `message_stop`;
/// - one `content_block_start` / `content_block_stop` pair per content
///   block, with contiguous indices starting at 0;
/// - text, thinking and tool_use blocks get their delta between start and
///   stop; thinking emits `signature_delta` immediately before its stop;
/// - opaque blocks (`Raw`, `server_tool_use`, `web_search_tool_result`)
///   arrive whole in `content_block_start` with **no delta** (this is how
///   the advisor result arrives);
/// - one final `message_delta` carrying `stop_reason`, `stop_sequence` and
///   the full usage (including `iterations[]`);
/// - no `ping`, no `[DONE]`.
#[must_use]
pub fn message_to_events(msg: Message) -> Vec<StreamEvent> {
    let Message {
        id,
        r#type,
        role,
        content,
        model,
        stop_reason,
        stop_sequence,
        usage,
    } = msg;

    let mut events = Vec::with_capacity(content.len() * 3 + 3);
    events.push(start_event(&Message {
        id,
        r#type,
        role,
        content: Vec::new(),
        model,
        stop_reason: None,
        stop_sequence: None,
        usage: usage.clone(),
    }));
    for (i, block) in content.into_iter().enumerate() {
        events.extend(block_events(u32::try_from(i).unwrap_or(u32::MAX), block));
    }
    events.extend(end_events(stop_reason, stop_sequence, usage));
    events
}

/// The `message_start` event for `msg` (its `content` is ignored: a message
/// always starts empty).
///
/// House convention (`src/stream.rs` `build_message_start`): the start event
/// carries input tokens but no output tokens yet. `iterations[]` belongs on
/// the terminal delta only (§11.5), so a client that reads both events cannot
/// double-count advisor tokens.
#[must_use]
pub fn start_event(msg: &Message) -> StreamEvent {
    let mut start_usage = msg.usage.clone();
    start_usage.output_tokens = 0;
    start_usage.iterations = None;
    StreamEvent::MessageStart {
        message: Message {
            id: msg.id.clone(),
            r#type: msg.r#type,
            role: msg.role,
            content: Vec::new(),
            model: msg.model.clone(),
            stop_reason: None,
            stop_sequence: None,
            usage: start_usage,
        },
    }
}

/// The closing events: one `message_delta` carrying `stop_reason`,
/// `stop_sequence` and the full usage (including `iterations[]`), then
/// `message_stop`.
#[must_use]
pub fn end_events(
    stop_reason: Option<crate::anthropic::StopReason>,
    stop_sequence: Option<String>,
    usage: crate::anthropic::Usage,
) -> Vec<StreamEvent> {
    vec![
        StreamEvent::MessageDelta {
            delta: MessageDeltaPayload {
                stop_reason,
                stop_sequence,
            },
            usage: Some(usage),
        },
        StreamEvent::MessageStop {},
    ]
}

/// The events for ONE content block at `index`: a start, its delta (if it
/// has one) and a stop.
#[must_use]
pub fn block_events(index: u32, block: ResponseContentBlock) -> Vec<StreamEvent> {
    let mut events = Vec::with_capacity(3);
    match block {
        ResponseContentBlock::Text { text } => {
            events.push(StreamEvent::ContentBlockStart {
                index,
                content_block: ContentBlockKind::Text {
                    text: String::new(),
                },
            });
            events.push(StreamEvent::ContentBlockDelta {
                index,
                delta: ContentDelta::TextDelta { text },
            });
            events.push(StreamEvent::ContentBlockStop { index });
        }
        ResponseContentBlock::ToolUse { id, name, input } => {
            // `content_block_start` carries the empty shell; the arguments
            // arrive as one `input_json_delta`, exactly as
            // `StreamTranslator::emit_repaired_args` emits them.
            let partial_json = serde_json::to_string(&input).unwrap_or_else(|_| "{}".to_owned());
            events.push(StreamEvent::ContentBlockStart {
                index,
                content_block: ContentBlockKind::ToolUse {
                    id,
                    name,
                    input: Value::Object(Default::default()),
                },
            });
            events.push(StreamEvent::ContentBlockDelta {
                index,
                delta: ContentDelta::InputJsonDelta { partial_json },
            });
            events.push(StreamEvent::ContentBlockStop { index });
        }
        ResponseContentBlock::Thinking {
            thinking,
            signature,
        } => {
            events.push(StreamEvent::ContentBlockStart {
                index,
                content_block: ContentBlockKind::Thinking {
                    thinking: String::new(),
                    signature: String::new(),
                },
            });
            events.push(StreamEvent::ContentBlockDelta {
                index,
                delta: ContentDelta::ThinkingDelta { thinking },
            });
            // `src/stream.rs:878-903`: the signature MUST land immediately
            // before `content_block_stop`, otherwise the client cannot verify
            // the thinking block on the next turn.
            if !signature.is_empty() {
                events.push(StreamEvent::ContentBlockDelta {
                    index,
                    delta: ContentDelta::SignatureDelta { signature },
                });
            }
            events.push(StreamEvent::ContentBlockStop { index });
        }
        ResponseContentBlock::Raw(v) => {
            // The tools' own blocks. `untagged` on the target enum emits the
            // Value verbatim (§11.6), so this is a plain move.
            events.push(StreamEvent::ContentBlockStart {
                index,
                content_block: ContentBlockKind::Raw(v),
            });
            events.push(StreamEvent::ContentBlockStop { index });
        }
        other => {
            // Pre-existing opaque blocks (`server_tool_use`,
            // `web_search_tool_result`) also arrive whole: the real streaming
            // translator emits start+stop with no delta for them
            // (`emit_server_tool_use` / `emit_pending_web_search_results`).
            let v = serde_json::to_value(&other).unwrap_or(Value::Null);
            events.push(StreamEvent::ContentBlockStart {
                index,
                content_block: ContentBlockKind::Raw(v),
            });
            events.push(StreamEvent::ContentBlockStop { index });
        }
    }
    events
}

#[cfg(test)]
mod tests;
