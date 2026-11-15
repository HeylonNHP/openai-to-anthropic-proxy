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

    // House convention (`src/stream.rs` `build_message_start`): the start
    // event carries input tokens but no output tokens yet. `iterations[]`
    // belongs on the terminal delta only (§11.5), so a client that reads
    // both events cannot double-count advisor tokens.
    let mut start_usage = usage.clone();
    start_usage.output_tokens = 0;
    start_usage.iterations = None;

    let mut events = Vec::with_capacity(content.len() * 3 + 3);
    events.push(StreamEvent::MessageStart {
        message: Message {
            id,
            r#type,
            role,
            content: Vec::new(),
            model,
            stop_reason: None,
            stop_sequence: None,
            usage: start_usage,
        },
    });

    for (i, block) in content.into_iter().enumerate() {
        let index = u32::try_from(i).unwrap_or(u32::MAX);
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
                // `content_block_start` carries the empty shell; the
                // arguments arrive as one `input_json_delta`, exactly as
                // `StreamTranslator::emit_repaired_args` emits them.
                let partial_json =
                    serde_json::to_string(&input).unwrap_or_else(|_| "{}".to_owned());
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
                // `src/stream.rs:878-903`: the signature MUST land
                // immediately before `content_block_stop`, otherwise the
                // client cannot verify the thinking block on the next turn.
                if !signature.is_empty() {
                    events.push(StreamEvent::ContentBlockDelta {
                        index,
                        delta: ContentDelta::SignatureDelta { signature },
                    });
                }
                events.push(StreamEvent::ContentBlockStop { index });
            }
            ResponseContentBlock::Raw(v) => {
                // The advisor's own blocks. `untagged` on the target enum
                // emits the Value verbatim (§11.6), so this is a plain move.
                events.push(StreamEvent::ContentBlockStart {
                    index,
                    content_block: ContentBlockKind::Raw(v),
                });
                events.push(StreamEvent::ContentBlockStop { index });
            }
            other => {
                // Pre-existing opaque blocks (`server_tool_use`,
                // `web_search_tool_result`) also arrive whole: the real
                // streaming translator emits start+stop with no delta for
                // them (`emit_server_tool_use` / `emit_pending_web_search_results`).
                let v = serde_json::to_value(&other).unwrap_or(Value::Null);
                events.push(StreamEvent::ContentBlockStart {
                    index,
                    content_block: ContentBlockKind::Raw(v),
                });
                events.push(StreamEvent::ContentBlockStop { index });
            }
        }
    }

    events.push(StreamEvent::MessageDelta {
        delta: MessageDeltaPayload {
            stop_reason,
            stop_sequence,
        },
        usage: Some(usage),
    });
    events.push(StreamEvent::MessageStop {});
    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::advisor::protocol;
    use crate::anthropic::{StopReason, Usage};
    use serde_json::json;

    fn usage() -> Usage {
        Usage {
            input_tokens: 11,
            output_tokens: 5,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 2,
            cache_creation_input_tokens_5m: 0,
            cache_creation_input_tokens_1h: 0,
            thinking_tokens: 0,
            server_tool_use: None,
            iterations: None,
        }
    }

    fn message(content: Vec<ResponseContentBlock>) -> Message {
        Message {
            id: "msg_test".into(),
            r#type: "message",
            role: "assistant",
            content,
            model: "claude-sonnet-5".into(),
            stop_reason: Some(StopReason::EndTurn),
            stop_sequence: None,
            usage: usage(),
        }
    }

    /// Compact discriminant for exact sequence assertions.
    fn variant(ev: &StreamEvent) -> &'static str {
        match ev {
            StreamEvent::MessageStart { .. } => "message_start",
            StreamEvent::ContentBlockStart { .. } => "content_block_start",
            StreamEvent::ContentBlockDelta { .. } => "content_block_delta",
            StreamEvent::ContentBlockStop { .. } => "content_block_stop",
            StreamEvent::MessageDelta { .. } => "message_delta",
            StreamEvent::MessageStop { .. } => "message_stop",
            StreamEvent::Ping {} => "ping",
            StreamEvent::Error { .. } => "error",
        }
    }

    fn variants(events: &[StreamEvent]) -> Vec<&'static str> {
        events.iter().map(variant).collect()
    }

    fn indices(events: &[StreamEvent]) -> Vec<u32> {
        events
            .iter()
            .filter_map(|ev| match ev {
                StreamEvent::ContentBlockStart { index, .. }
                | StreamEvent::ContentBlockDelta { index, .. }
                | StreamEvent::ContentBlockStop { index } => Some(*index),
                _ => None,
            })
            .collect()
    }

    fn assert_terminal_sequence(events: &[StreamEvent]) {
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, StreamEvent::MessageStart { .. }))
                .count(),
            1,
            "exactly one message_start"
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, StreamEvent::MessageStop {}))
                .count(),
            1,
            "exactly one message_stop"
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, StreamEvent::MessageDelta { .. }))
                .count(),
            1,
            "exactly one message_delta"
        );
        assert!(matches!(
            events.first(),
            Some(StreamEvent::MessageStart { .. })
        ));
        assert!(matches!(events.last(), Some(StreamEvent::MessageStop {})));
        assert!(
            !events.iter().any(|e| matches!(e, StreamEvent::Ping {})),
            "a buffered response has no idle gap and must not emit ping"
        );
        // No `[DONE]` sentinel: it is not part of the Anthropic SSE event
        // set, and the proxy's encoder never writes one.
        let encoded = serde_json::to_string(events).unwrap();
        assert!(!encoded.contains("[DONE]"), "{encoded}");
    }

    #[test]
    fn plain_text_emits_one_start_delta_stop_then_terminal_events() {
        let events = message_to_events(message(vec![ResponseContentBlock::Text {
            text: "Hello there!".into(),
        }]));
        assert_eq!(
            variants(&events),
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        assert_eq!(indices(&events), vec![0, 0, 0]);
        assert_terminal_sequence(&events);

        match &events[1] {
            StreamEvent::ContentBlockStart { content_block, .. } => assert_eq!(
                serde_json::to_value(content_block).unwrap(),
                json!({"type": "text", "text": ""}),
                "the start event carries the empty shell, as streaming does"
            ),
            _ => unreachable!(),
        }
        match &events[2] {
            StreamEvent::ContentBlockDelta { delta, .. } => assert_eq!(
                serde_json::to_value(delta).unwrap(),
                json!({"type": "text_delta", "text": "Hello there!"})
            ),
            _ => unreachable!(),
        }
        match &events[0] {
            StreamEvent::MessageStart { message } => {
                assert!(message.content.is_empty());
                assert_eq!(message.usage.input_tokens, 11);
                assert_eq!(message.usage.output_tokens, 0);
            }
            _ => unreachable!(),
        }
        match &events[4] {
            StreamEvent::MessageDelta { delta, usage } => {
                assert_eq!(delta.stop_reason, Some(StopReason::EndTurn));
                assert_eq!(delta.stop_sequence, None);
                assert_eq!(usage.as_ref().unwrap().output_tokens, 5);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn text_plus_raw_advisor_pair_has_contiguous_indices_and_no_raw_deltas() {
        let id = protocol::new_id();
        let use_block = protocol::server_tool_use(&id);
        let result_block = protocol::advisor_result(&id, "Use a channel.");
        let events = message_to_events(message(vec![
            ResponseContentBlock::Text {
                text: "Let me consult the advisor.".into(),
            },
            ResponseContentBlock::Raw(use_block.clone()),
            ResponseContentBlock::Raw(result_block.clone()),
            ResponseContentBlock::Text {
                text: "Here's the implementation.".into(),
            },
        ]));
        assert_eq!(
            variants(&events),
            vec![
                "message_start",
                // index 0: text
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                // index 1: server_tool_use (whole, no delta)
                "content_block_start",
                "content_block_stop",
                // index 2: advisor_tool_result (whole, no delta)
                "content_block_start",
                "content_block_stop",
                // index 3: text
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        assert_eq!(indices(&events), vec![0, 0, 0, 1, 1, 2, 2, 3, 3, 3]);
        assert_terminal_sequence(&events);

        // The two Raw blocks are emitted verbatim, with their own `type`.
        for (pos, expected) in [(4, &use_block), (6, &result_block)] {
            match &events[pos] {
                StreamEvent::ContentBlockStart { content_block, .. } => {
                    assert_eq!(serde_json::to_value(content_block).unwrap(), *expected);
                    assert!(
                        matches!(content_block, ContentBlockKind::Raw(_)),
                        "raw blocks must use the generic extension point"
                    );
                }
                _ => panic!("expected content_block_start at {pos}"),
            }
        }
        // No delta for either Raw block.
        assert!(
            !events
                .iter()
                .any(|ev| matches!(ev, StreamEvent::ContentBlockDelta { index: 1 | 2, .. })),
            "an opaque advisor block must arrive whole"
        );
        // The id pair is intact end-to-end.
        let encoded = serde_json::to_string(&events).unwrap();
        assert!(encoded.contains(&id));
        assert!(encoded.contains("server_tool_use"));
        assert!(encoded.contains("advisor_tool_result"));
        assert!(encoded.contains("advisor_result"));
    }

    #[test]
    fn thinking_block_puts_signature_delta_immediately_before_stop() {
        let events = message_to_events(message(vec![ResponseContentBlock::Thinking {
            thinking: "thinking out loud".into(),
            signature: "opaque-sig-blob".into(),
        }]));
        assert_eq!(
            variants(&events),
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        assert_eq!(indices(&events), vec![0, 0, 0, 0]);
        assert_terminal_sequence(&events);
        assert_eq!(
            serde_json::to_value(match &events[1] {
                StreamEvent::ContentBlockStart { content_block, .. } => content_block,
                _ => unreachable!(),
            })
            .unwrap(),
            json!({"type": "thinking", "thinking": "", "signature": ""})
        );
        match &events[2] {
            StreamEvent::ContentBlockDelta { delta, .. } => assert_eq!(
                serde_json::to_value(delta).unwrap(),
                json!({"type": "thinking_delta", "thinking": "thinking out loud"})
            ),
            _ => unreachable!(),
        }
        match &events[3] {
            StreamEvent::ContentBlockDelta { delta, .. } => assert_eq!(
                serde_json::to_value(delta).unwrap(),
                json!({"type": "signature_delta", "signature": "opaque-sig-blob"})
            ),
            _ => unreachable!(),
        }
        assert!(matches!(
            events[4],
            StreamEvent::ContentBlockStop { index: 0 }
        ));
    }

    #[test]
    fn empty_thinking_signature_emits_no_signature_delta() {
        // Matches `src/stream.rs`'s guard: only a non-empty signature is
        // emitted, and it still sits immediately before the stop.
        let events = message_to_events(message(vec![ResponseContentBlock::Thinking {
            thinking: "brief".into(),
            signature: String::new(),
        }]));
        assert_eq!(
            variants(&events),
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        assert_eq!(indices(&events), vec![0, 0, 0]);
    }

    #[test]
    fn tool_use_block_emits_one_input_json_delta() {
        let events = message_to_events(message(vec![ResponseContentBlock::ToolUse {
            id: "call_1".into(),
            name: "get_weather".into(),
            input: json!({"location": "SF"}),
        }]));
        assert_eq!(
            variants(&events),
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        assert_eq!(indices(&events), vec![0, 0, 0]);
        assert_terminal_sequence(&events);
        match &events[1] {
            StreamEvent::ContentBlockStart { content_block, .. } => assert_eq!(
                serde_json::to_value(content_block).unwrap(),
                json!({"type": "tool_use", "id": "call_1", "name": "get_weather", "input": {}})
            ),
            _ => unreachable!(),
        }
        match &events[2] {
            StreamEvent::ContentBlockDelta { delta, .. } => assert_eq!(
                serde_json::to_value(delta).unwrap(),
                json!({"type": "input_json_delta", "partial_json": r#"{"location":"SF"}"#})
            ),
            _ => unreachable!(),
        }
    }

    #[test]
    fn stop_sequence_and_iterations_reach_the_final_delta_only() {
        let mut msg = message(vec![ResponseContentBlock::Text {
            text: "done".into(),
        }]);
        msg.stop_sequence = Some("STOP".into());
        msg.usage.iterations = Some(vec![
            json!({"type": "message", "input_tokens": 11, "output_tokens": 5}),
            json!({"type": "advisor_message", "model": "beam", "input_tokens": 3, "output_tokens": 2}),
        ]);
        let events = message_to_events(msg);
        assert_terminal_sequence(&events);
        match &events[0] {
            StreamEvent::MessageStart { message } => {
                assert!(message.usage.iterations.is_none(), "no double billing");
                assert_eq!(message.usage.output_tokens, 0);
            }
            _ => unreachable!(),
        }
        match events
            .iter()
            .find(|e| matches!(e, StreamEvent::MessageDelta { .. }))
            .unwrap()
        {
            StreamEvent::MessageDelta { delta, usage } => {
                assert_eq!(delta.stop_sequence.as_deref(), Some("STOP"));
                assert_eq!(
                    usage.as_ref().unwrap().iterations.as_ref().unwrap().len(),
                    2
                );
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn empty_content_still_emits_a_complete_message() {
        let events = message_to_events(message(Vec::new()));
        assert_eq!(
            variants(&events),
            vec!["message_start", "message_delta", "message_stop"]
        );
        assert!(indices(&events).is_empty());
        assert_terminal_sequence(&events);
    }
}
