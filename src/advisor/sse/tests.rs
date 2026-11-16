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
