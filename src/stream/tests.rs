use super::*;
use crate::responses::{
    IncompleteDetails, InputTokensDetails, OutputContentPart, OutputItem, ResponsesResponse,
    ResponsesUsage,
};
use serde_json::json;

fn msg_id() -> String {
    "msg_test".into()
}

fn model() -> String {
    "test-model".into()
}

fn base_response(status: &str) -> ResponsesResponse {
    ResponsesResponse {
        id: "resp_test".into(),
        object: "response".into(),
        created_at: 0,
        status: status.into(),
        model: model(),
        output: vec![],
        usage: Some(ResponsesUsage {
            input_tokens: 10,
            output_tokens: 5,
            total_tokens: 15,
            input_tokens_details: None,
            output_tokens_details: None,
        }),
        error: None,
        incomplete_details: None,
    }
}

fn created_event() -> ResponsesStreamEvent {
    ResponsesStreamEvent::Created {
        response: base_response("in_progress"),
    }
}

fn completed_event(status: &str) -> ResponsesStreamEvent {
    ResponsesStreamEvent::Completed {
        response: base_response(status),
    }
}

fn completed_event_with_output(status: &str, output: Vec<OutputItem>) -> ResponsesStreamEvent {
    let mut r = base_response(status);
    r.output = output;
    ResponsesStreamEvent::Completed { response: r }
}

fn incomplete_event(reason: &str) -> ResponsesStreamEvent {
    let mut r = base_response("incomplete");
    r.incomplete_details = Some(IncompleteDetails {
        reason: reason.into(),
    });
    ResponsesStreamEvent::Incomplete { response: r }
}

fn message_item_added(output_index: u32) -> ResponsesStreamEvent {
    ResponsesStreamEvent::OutputItemAdded {
        output_index,
        item: OutputItem::Message {
            id: Some("msg_x".into()),
            status: Some("in_progress".into()),
            role: "assistant".into(),
            content: vec![],
        },
    }
}

/// A terminal `output_item.done` for a Message whose output_text part
/// carries the given `url_citation` annotations (the shape Airia emits
/// on the completed message).
fn message_item_done_with_annotations(
    output_index: u32,
    annotations: serde_json::Value,
) -> ResponsesStreamEvent {
    ResponsesStreamEvent::OutputItemDone {
        output_index,
        item: OutputItem::Message {
            id: Some(format!("msg_{output_index}")),
            status: Some("completed".into()),
            role: "assistant".into(),
            content: vec![OutputContentPart::OutputText {
                text: "answer".into(),
                annotations: annotations.as_array().cloned().unwrap_or_default(),
            }],
        },
    }
}

fn function_call_added(output_index: u32, call_id: &str, name: &str) -> ResponsesStreamEvent {
    ResponsesStreamEvent::OutputItemAdded {
        output_index,
        item: OutputItem::FunctionCall {
            id: Some(format!("fc_{output_index}")),
            status: Some("in_progress".into()),
            call_id: call_id.into(),
            name: name.into(),
            arguments: String::new(),
        },
    }
}

fn function_call_args_delta(output_index: u32, delta: &str) -> ResponsesStreamEvent {
    ResponsesStreamEvent::FunctionCallArgumentsDelta {
        item_id: format!("fc_{output_index}"),
        output_index,
        delta: delta.into(),
    }
}

fn function_call_args_done(output_index: u32, arguments: &str) -> ResponsesStreamEvent {
    ResponsesStreamEvent::FunctionCallArgumentsDone {
        item_id: format!("fc_{output_index}"),
        output_index,
        arguments: arguments.into(),
    }
}

fn reasoning_item_added(output_index: u32) -> ResponsesStreamEvent {
    ResponsesStreamEvent::OutputItemAdded {
        output_index,
        item: OutputItem::Reasoning {
            id: Some(format!("rs_{output_index}")),
            summary: vec![],
            encrypted_content: String::new(),
        },
    }
}

fn reasoning_summary_delta(output_index: u32, delta: &str) -> ResponsesStreamEvent {
    ResponsesStreamEvent::ReasoningSummaryTextDelta {
        item_id: format!("rs_{output_index}"),
        output_index,
        summary_index: 0,
        delta: delta.into(),
    }
}

fn reasoning_item_done(output_index: u32, encrypted: &str) -> ResponsesStreamEvent {
    ResponsesStreamEvent::OutputItemDone {
        output_index,
        item: OutputItem::Reasoning {
            id: Some(format!("rs_{output_index}")),
            summary: vec![],
            encrypted_content: encrypted.into(),
        },
    }
}

fn output_text_delta(output_index: u32, text: &str) -> ResponsesStreamEvent {
    ResponsesStreamEvent::OutputTextDelta {
        item_id: "msg_x".into(),
        output_index,
        content_index: 0,
        delta: text.into(),
    }
}

fn refusal_delta(output_index: u32, text: &str) -> ResponsesStreamEvent {
    ResponsesStreamEvent::RefusalDelta {
        item_id: "msg_x".into(),
        output_index,
        content_index: 0,
        delta: text.into(),
    }
}

#[test]
fn text_stream_emits_message_start_then_text_deltas_then_stop() {
    let mut t = StreamTranslator::new(msg_id(), model(), crate::repair::ToolSchemaRegistry::new());
    t.feed_event(&created_event());
    t.feed_event(&message_item_added(0));
    let evs = t.feed_event(&output_text_delta(0, "Hello"));
    assert_eq!(evs.len(), 1);
    assert!(matches!(
        evs[0],
        StreamEvent::ContentBlockDelta {
            delta: ContentDelta::TextDelta { .. },
            ..
        }
    ));
    let evs = t.feed_event(&output_text_delta(0, " world"));
    assert_eq!(evs.len(), 1);
    assert!(matches!(evs[0], StreamEvent::ContentBlockDelta { .. }));

    let evs = t.feed_event(&completed_event("completed"));
    // content_block_stop (text)
    assert_eq!(evs.len(), 1);
    assert!(matches!(evs[0], StreamEvent::ContentBlockStop { index: 0 }));

    let closing = t.finish();
    assert_eq!(closing.len(), 2);
    match &closing[0] {
        StreamEvent::MessageDelta { delta, usage } => {
            assert_eq!(delta.stop_reason, Some(StopReason::EndTurn));
            let u = usage.as_ref().unwrap();
            assert_eq!(u.input_tokens, 10);
            assert_eq!(u.output_tokens, 5);
        }
        _ => panic!("expected message_delta"),
    }
    assert!(matches!(closing[1], StreamEvent::MessageStop {}));
}

#[test]
fn tool_call_stream_emits_tool_use_block() {
    let mut t = StreamTranslator::new(msg_id(), model(), crate::repair::ToolSchemaRegistry::new());
    t.feed_event(&created_event());

    let evs = t.feed_event(&function_call_added(0, "call_1", "get_weather"));
    // content_block_start (tool)
    assert_eq!(evs.len(), 1);
    match &evs[0] {
        StreamEvent::ContentBlockStart {
            index,
            content_block,
        } => {
            assert_eq!(*index, 0);
            match content_block {
                ContentBlockKind::ToolUse { id, name, .. } => {
                    assert_eq!(id, "call_1");
                    assert_eq!(name, "get_weather");
                }
                _ => panic!("expected tool_use"),
            }
        }
        _ => panic!("expected content_block_start"),
    }

    // Args deltas are BUFFERED, not streamed through: the complete
    // blob must be repairable before it is shipped downstream.
    let evs = t.feed_event(&function_call_args_delta(0, r#"{"loc"#));
    assert!(evs.is_empty());
    let evs = t.feed_event(&function_call_args_delta(0, r#"ation":"SF"}"#));
    assert!(evs.is_empty());

    // The terminal done event flushes the repaired arguments as a
    // single `input_json_delta`.
    let evs = t.feed_event(&function_call_args_done(0, r#"{"location":"SF"}"#));
    assert_eq!(evs.len(), 1);
    match &evs[0] {
        StreamEvent::ContentBlockDelta {
            index,
            delta: ContentDelta::InputJsonDelta { partial_json },
        } => {
            assert_eq!(*index, 0);
            assert_eq!(partial_json, r#"{"location":"SF"}"#);
        }
        _ => panic!("expected input_json_delta"),
    }

    // The terminal Completed event should carry the function call
    // in its output so the translator can detect has_tool_use and
    // set stop_reason = ToolUse.
    let evs = t.feed_event(&completed_event_with_output(
        "completed",
        vec![OutputItem::FunctionCall {
            id: Some("fc_0".into()),
            status: Some("completed".into()),
            call_id: "call_1".into(),
            name: "get_weather".into(),
            arguments: r#"{"location":"SF"}"#.into(),
        }],
    ));
    // content_block_stop (tool)
    assert_eq!(evs.len(), 1);
    assert!(matches!(evs[0], StreamEvent::ContentBlockStop { index: 0 }));

    let closing = t.finish();
    match &closing[0] {
        StreamEvent::MessageDelta { delta, .. } => {
            // has_tool_use → stop_reason = ToolUse
            assert_eq!(delta.stop_reason, Some(StopReason::ToolUse));
        }
        _ => panic!("expected message_delta"),
    }
}

#[test]
fn text_then_tool_call_closes_text_first() {
    let mut t = StreamTranslator::new(msg_id(), model(), crate::repair::ToolSchemaRegistry::new());
    t.feed_event(&created_event());
    t.feed_event(&message_item_added(0));
    t.feed_event(&output_text_delta(0, "Let me check."));

    // Open a tool use. Expect: text-stop, tool-start.
    let evs = t.feed_event(&function_call_added(0, "call_1", "get_weather"));
    assert_eq!(evs.len(), 2);
    assert!(matches!(evs[0], StreamEvent::ContentBlockStop { index: 0 }));
    assert!(matches!(
        evs[1],
        StreamEvent::ContentBlockStart { index: 1, .. }
    ));

    // Args delta is buffered; nothing is emitted yet.
    let evs = t.feed_event(&function_call_args_delta(0, "{}"));
    assert!(evs.is_empty());

    // Closing the block flushes the (repaired) args delta on index 1
    // and then stops the block.
    let evs = t.feed_event(&completed_event("completed"));
    assert_eq!(evs.len(), 2);
    match &evs[0] {
        StreamEvent::ContentBlockDelta { index, .. } => assert_eq!(*index, 1),
        _ => panic!(),
    }
    assert!(matches!(evs[1], StreamEvent::ContentBlockStop { index: 1 }));

    let closing = t.finish();
    assert!(matches!(closing[0], StreamEvent::MessageDelta { .. }));
    assert!(matches!(closing[1], StreamEvent::MessageStop {}));
}

#[test]
fn parallel_tool_calls_close_in_output_index_order() {
    let mut t = StreamTranslator::new(msg_id(), model(), crate::repair::ToolSchemaRegistry::new());
    t.feed_event(&created_event());
    t.feed_event(&function_call_added(0, "call_a", "tool_a"));
    let _ = t.feed_event(&function_call_added(1, "call_b", "tool_b"));

    // Both blocks are open at output_index 0 and 1, Anthropic
    // indices 0 and 1. Closing flushes each block's repaired args
    // delta followed by its stop, in BTreeMap (output_index) order.
    let evs = t.feed_event(&completed_event("completed"));
    assert_eq!(evs.len(), 4);
    assert!(matches!(
        evs[0],
        StreamEvent::ContentBlockDelta { index: 0, .. }
    ));
    assert!(matches!(evs[1], StreamEvent::ContentBlockStop { index: 0 }));
    assert!(matches!(
        evs[2],
        StreamEvent::ContentBlockDelta { index: 1, .. }
    ));
    assert!(matches!(evs[3], StreamEvent::ContentBlockStop { index: 1 }));
}

#[test]
fn reasoning_item_emits_thinking_block() {
    let mut t = StreamTranslator::new(msg_id(), model(), crate::repair::ToolSchemaRegistry::new());
    t.feed_event(&created_event());

    let evs = t.feed_event(&reasoning_item_added(0));
    assert_eq!(evs.len(), 1);
    match &evs[0] {
        StreamEvent::ContentBlockStart {
            index,
            content_block,
        } => {
            assert_eq!(*index, 0);
            assert!(matches!(content_block, ContentBlockKind::Thinking { .. }));
        }
        _ => panic!("expected content_block_start Thinking"),
    }

    let evs = t.feed_event(&reasoning_summary_delta(0, "step 1 "));
    assert_eq!(evs.len(), 1);
    match &evs[0] {
        StreamEvent::ContentBlockDelta { delta, .. } => {
            assert!(matches!(
                delta,
                ContentDelta::ThinkingDelta { thinking } if thinking == "step 1 "
            ));
        }
        _ => panic!("expected thinking_delta"),
    }

    let evs = t.feed_event(&reasoning_summary_delta(0, "step 2"));
    assert_eq!(evs.len(), 1);
    assert!(matches!(evs[0], StreamEvent::ContentBlockDelta { .. }));

    // Reasoning item done carries the signature; the translator
    // records it but does not emit an immediate delta. The
    // signature is emitted later, when the block is closed (by a
    // `Completed` event, a downstream item, or the stream end).
    let evs = t.feed_event(&reasoning_item_done(0, "opaque-sig"));
    assert!(evs.is_empty(), "no events for reasoning done");

    // When the stream ends with a Completed event, the open
    // thinking block is closed via `capture_terminal` →
    // `finalize` → `close_open_thinking_blocks`. The close emits
    // a `SignatureDelta` carrying the captured signature,
    // followed by a `ContentBlockStop`. Without the signature on
    // the wire, the Anthropic client can't verify the thinking
    // block on the next turn. `finish()` then emits the
    // MessageDelta + MessageStop that close the message.
    let mut closing = t.feed_event(&completed_event("completed"));
    closing.extend(t.finish());
    assert!(
        matches!(
            closing[0],
            StreamEvent::ContentBlockDelta {
                delta: ContentDelta::SignatureDelta { .. },
                ..
            }
        ),
        "expected SignatureDelta first, got {:?}",
        closing[0]
    );
    assert!(matches!(
        closing[1],
        StreamEvent::ContentBlockStop { index: 0 }
    ));
    // MessageDelta + MessageStop
    assert!(matches!(closing[2], StreamEvent::MessageDelta { .. }));
    assert!(matches!(closing[3], StreamEvent::MessageStop {}));
}

#[test]
fn empty_signature_skips_signature_delta() {
    // Some upstreams send a `done` event with an empty
    // `encrypted_content`. The translator must close the block
    // with a plain `ContentBlockStop` and no SignatureDelta —
    // emitting an empty `SignatureDelta { signature: "" }` would
    // be a confusing no-op on the wire.
    let mut t = StreamTranslator::new(msg_id(), model(), crate::repair::ToolSchemaRegistry::new());
    t.feed_event(&created_event());
    let evs = t.feed_event(&reasoning_item_added(0));
    assert!(matches!(evs[0], StreamEvent::ContentBlockStart { .. }));

    let evs = t.feed_event(&reasoning_item_done(0, ""));
    assert!(evs.is_empty());

    // The block closes on Completed. With an empty signature,
    // the close sequence is just `ContentBlockStop` — no
    // `SignatureDelta` should precede it.
    let closing = t.feed_event(&completed_event("completed"));
    let stop_pos = closing
        .iter()
        .position(|e| matches!(e, StreamEvent::ContentBlockStop { index: 0 }))
        .expect("expected ContentBlockStop");
    assert_eq!(
        stop_pos, 0,
        "expected ContentBlockStop to be first, got {closing:?}"
    );
    if stop_pos > 0 {
        assert!(
            !matches!(
                closing[stop_pos - 1],
                StreamEvent::ContentBlockDelta {
                    delta: ContentDelta::SignatureDelta { .. },
                    ..
                }
            ),
            "expected no SignatureDelta when signature is empty, got {closing:?}"
        );
    }
}

#[test]
fn late_reasoning_item_after_text_closes_text_first() {
    let mut t = StreamTranslator::new(msg_id(), model(), crate::repair::ToolSchemaRegistry::new());
    t.feed_event(&created_event());
    t.feed_event(&message_item_added(0));
    t.feed_event(&output_text_delta(0, "intro"));

    // Reasoning item after the text block.
    let evs = t.feed_event(&reasoning_item_added(1));
    // text-stop, thinking-start.
    assert_eq!(evs.len(), 2);
    assert!(matches!(evs[0], StreamEvent::ContentBlockStop { index: 0 }));
    match &evs[1] {
        StreamEvent::ContentBlockStart {
            index,
            content_block,
        } => {
            assert_eq!(*index, 1);
            assert!(matches!(content_block, ContentBlockKind::Thinking { .. }));
        }
        _ => panic!("expected content_block_start Thinking"),
    }
}

#[test]
fn incomplete_response_max_tokens_maps_to_max_tokens() {
    let mut t = StreamTranslator::new(msg_id(), model(), crate::repair::ToolSchemaRegistry::new());
    t.feed_event(&created_event());
    t.feed_event(&message_item_added(0));
    t.feed_event(&output_text_delta(0, "a bit"));
    let evs = t.feed_event(&incomplete_event("max_output_tokens"));
    // content_block_stop(text)
    assert_eq!(evs.len(), 1);
    assert!(matches!(evs[0], StreamEvent::ContentBlockStop { index: 0 }));
    let closing = t.finish();
    match &closing[0] {
        StreamEvent::MessageDelta { delta, .. } => {
            assert_eq!(delta.stop_reason, Some(StopReason::MaxTokens));
        }
        _ => panic!("expected message_delta"),
    }
}

#[test]
fn incomplete_response_content_filter_maps_to_refusal() {
    // A safety filter stopped the generation: Anthropic reports the same
    // thing as `refusal`.
    let mut t = StreamTranslator::new(msg_id(), model(), crate::repair::ToolSchemaRegistry::new());
    t.feed_event(&created_event());
    t.feed_event(&message_item_added(0));
    t.feed_event(&output_text_delta(0, "a bit"));
    t.feed_event(&incomplete_event("content_filter"));
    let closing = t.finish();
    match &closing[0] {
        StreamEvent::MessageDelta { delta, .. } => {
            assert_eq!(delta.stop_reason, Some(StopReason::Refusal));
        }
        _ => panic!("expected message_delta"),
    }
}

#[test]
fn a_refusal_event_reports_the_refusal_stop_reason() {
    // The upstream declined mid-stream and the terminal response does not
    // repeat it: the wire event alone must be enough.
    let mut t = StreamTranslator::new(msg_id(), model(), crate::repair::ToolSchemaRegistry::new());
    t.feed_event(&created_event());
    t.feed_event(&message_item_added(0));
    t.feed_event(&refusal_delta(0, "I won't do that"));
    t.feed_event(&completed_event("completed"));
    let closing = t.finish();
    match &closing[0] {
        StreamEvent::MessageDelta { delta, .. } => {
            assert_eq!(delta.stop_reason, Some(StopReason::Refusal));
        }
        _ => panic!("expected message_delta"),
    }
}

#[test]
fn a_refusal_event_is_marked_the_same_way_the_buffered_path_marks_it() {
    let mut t = StreamTranslator::new(msg_id(), model(), crate::repair::ToolSchemaRegistry::new());
    t.feed_event(&created_event());
    t.feed_event(&message_item_added(0));
    let evs = t.feed_event(&refusal_delta(0, "I won't do that"));
    let text: String = evs
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ContentBlockDelta {
                delta: ContentDelta::TextDelta { text },
                ..
            } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "[refusal] I won't do that");
}

#[test]
fn a_refusal_is_marked_once_per_part() {
    let mut t = StreamTranslator::new(msg_id(), model(), crate::repair::ToolSchemaRegistry::new());
    t.feed_event(&created_event());
    t.feed_event(&message_item_added(0));
    t.feed_event(&refusal_delta(0, "I won't "));
    let evs = t.feed_event(&refusal_delta(0, "do that"));
    let text: String = evs
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ContentBlockDelta {
                delta: ContentDelta::TextDelta { text },
                ..
            } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "do that");
}

#[test]
fn multiple_streaming_web_searches_get_unique_ids_and_accurate_usage() {
    let mut t = StreamTranslator::new(msg_id(), model(), crate::repair::ToolSchemaRegistry::new());
    t.feed_event(&created_event());

    let first = t.feed_event(&ResponsesStreamEvent::OutputItemAdded {
        output_index: 0,
        item: OutputItem::WebSearchCall {
            id: Some("ws_01".into()),
            status: Some("in_progress".into()),
        },
    });
    let second = t.feed_event(&ResponsesStreamEvent::OutputItemAdded {
        output_index: 1,
        item: OutputItem::WebSearchCall {
            id: Some("ws_02".into()),
            status: Some("in_progress".into()),
        },
    });

    let ids: Vec<&str> = first
        .iter()
        .chain(&second)
        .filter_map(|event| match event {
            StreamEvent::ContentBlockStart {
                content_block: ContentBlockKind::ServerToolUse { id, .. },
                ..
            } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(ids.len(), 2, "expected one server_tool_use per search");
    assert_ne!(ids[0], ids[1], "ids must be unique");
    for id in &ids {
        assert!(
            crate::server_tool_id::is_server_tool_id(id),
            "server_tool_use id must match Anthropic's pattern: {id}"
        );
    }

    t.feed_event(&completed_event("completed"));
    let closing = t.finish();
    let usage = closing
        .iter()
        .find_map(|event| match event {
            StreamEvent::MessageDelta { usage, .. } => usage.as_ref(),
            _ => None,
        })
        .expect("terminal usage");
    assert_eq!(
        usage
            .server_tool_use
            .as_ref()
            .expect("server search usage")
            .web_search_requests,
        2
    );
}
#[test]
fn failed_response_surfaces_error() {
    let mut t = StreamTranslator::new(msg_id(), model(), crate::repair::ToolSchemaRegistry::new());
    t.feed_event(&created_event());
    let mut r = base_response("failed");
    r.error = Some(crate::responses::ResponsesError {
        message: "upstream rejected".into(),
        kind: "server_error".into(),
        code: None,
        param: None,
    });
    let evs = t.feed_event(&ResponsesStreamEvent::Failed { response: r });
    // The translator calls emit_error internally; we get back the
    // events. The exact shape depends on whether message_start
    // fired — it has, so just the error + stop.
    assert!(evs.iter().any(|e| matches!(e, StreamEvent::Error { .. })));
    assert!(evs.iter().any(|e| matches!(e, StreamEvent::MessageStop {})));
}

#[test]
fn failed_response_closes_open_text_block_before_error() {
    // Regression: a Failed event arriving while a text block is
    // open used to push `Error` + `MessageStop` without a
    // matching `content_block_stop`, leaving the Anthropic client
    // with a half-streamed response. The translator must now
    // close the block first.
    let mut t = StreamTranslator::new(msg_id(), model(), crate::repair::ToolSchemaRegistry::new());
    t.feed_event(&created_event());
    t.feed_event(&message_item_added(0));
    t.feed_event(&output_text_delta(0, "partial output"));

    let mut r = base_response("failed");
    r.error = Some(crate::responses::ResponsesError {
        message: "context overflow".into(),
        kind: "server_error".into(),
        code: None,
        param: None,
    });
    let evs = t.feed_event(&ResponsesStreamEvent::Failed { response: r });

    // Find the indices of content_block_stop, error, and message_stop.
    let stop_idx = evs
        .iter()
        .position(|e| matches!(e, StreamEvent::ContentBlockStop { .. }));
    let err_idx = evs
        .iter()
        .position(|e| matches!(e, StreamEvent::Error { .. }));
    let msg_stop_idx = evs
        .iter()
        .position(|e| matches!(e, StreamEvent::MessageStop {}));
    let stop_idx = stop_idx.expect("expected a ContentBlockStop");
    let err_idx = err_idx.expect("expected an Error event");
    let msg_stop_idx = msg_stop_idx.expect("expected a MessageStop");
    assert!(
        stop_idx < err_idx,
        "ContentBlockStop must precede Error, got events={evs:?}"
    );
    assert!(
        err_idx < msg_stop_idx,
        "Error must precede MessageStop, got events={evs:?}"
    );
}

#[test]
fn top_level_error_closes_open_tool_block_before_error() {
    // Same regression as above, but via a FunctionCall instead of
    // a text block, and triggered by the top-level `Error` SSE
    // event (not `Failed`).
    let mut t = StreamTranslator::new(msg_id(), model(), crate::repair::ToolSchemaRegistry::new());
    t.feed_event(&created_event());
    // Open a tool_use block.
    let tool_added = ResponsesStreamEvent::OutputItemAdded {
        output_index: 0,
        item: OutputItem::FunctionCall {
            id: Some("call_x".into()),
            call_id: "call_x".into(),
            name: "do_thing".into(),
            arguments: String::new(),
            status: None,
        },
    };
    t.feed_event(&tool_added);
    t.feed_event(&ResponsesStreamEvent::Error {
        message: "upstream closed".into(),
        code: None,
        param: None,
    });

    // Walk the events returned by the error feed (the bridge
    // calls `take_and_report_error`, which uses `emit_error`
    // under the hood). The order must be:
    //   1. content_block_stop (closing the open tool block)
    //   2. error
    //   3. message_stop
    let evs = t.emit_error("upstream_error", "upstream closed");
    let stop_idx = evs
        .iter()
        .position(|e| matches!(e, StreamEvent::ContentBlockStop { .. }));
    let err_idx = evs
        .iter()
        .position(|e| matches!(e, StreamEvent::Error { .. }));
    let msg_stop_idx = evs
        .iter()
        .position(|e| matches!(e, StreamEvent::MessageStop {}));
    assert!(
        stop_idx < err_idx,
        "ContentBlockStop must precede Error in emit_error, got events={evs:?}"
    );
    assert!(
        err_idx < msg_stop_idx,
        "Error must precede MessageStop in emit_error, got events={evs:?}"
    );
}

#[test]
fn usage_in_completed_event_is_captured() {
    let mut t = StreamTranslator::new(msg_id(), model(), crate::repair::ToolSchemaRegistry::new());
    t.feed_event(&created_event());
    t.feed_event(&message_item_added(0));
    t.feed_event(&output_text_delta(0, "x"));
    let _ = t.feed_event(&completed_event("completed"));
    let closing = t.finish();
    match &closing[0] {
        StreamEvent::MessageDelta { usage, .. } => {
            let u = usage.as_ref().unwrap();
            assert_eq!(u.input_tokens, 10);
            assert_eq!(u.output_tokens, 5);
        }
        _ => panic!("expected message_delta"),
    }
}

#[test]
fn completed_event_usage_includes_cache_fields() {
    let mut t = StreamTranslator::new(msg_id(), model(), crate::repair::ToolSchemaRegistry::new());
    t.feed_event(&created_event());
    t.feed_event(&message_item_added(0));
    t.feed_event(&output_text_delta(0, "x"));

    let mut r = base_response("completed");
    r.usage = Some(ResponsesUsage {
        input_tokens: 100,
        output_tokens: 50,
        total_tokens: 150,
        input_tokens_details: Some(InputTokensDetails {
            cached_tokens: 20,
            cache_write_tokens: 7,
        }),
        output_tokens_details: None,
    });
    let _ = t.feed_event(&ResponsesStreamEvent::Completed { response: r });

    let closing = t.finish();
    match &closing[0] {
        StreamEvent::MessageDelta { usage, .. } => {
            let u = usage.as_ref().unwrap();
            // 100 gross - 20 cached - 7 written.
            assert_eq!(u.input_tokens, 73);
            assert_eq!(u.output_tokens, 50);
            assert_eq!(u.cache_read_input_tokens, 20);
            assert_eq!(u.cache_creation_input_tokens, 7);
        }
        _ => panic!("expected message_delta"),
    }
}

#[test]
fn finish_without_start_emits_message_start_first() {
    let t = StreamTranslator::new(msg_id(), model(), crate::repair::ToolSchemaRegistry::new());
    let evs = t.finish();
    assert_eq!(evs.len(), 3);
    assert!(matches!(evs[0], StreamEvent::MessageStart { .. }));
    assert!(matches!(evs[1], StreamEvent::MessageDelta { .. }));
    assert!(matches!(evs[2], StreamEvent::MessageStop {}));
}

#[test]
fn feed_after_finish_is_noop() {
    let mut t = StreamTranslator::new(msg_id(), model(), crate::repair::ToolSchemaRegistry::new());
    t.feed_event(&created_event());
    t.feed_event(&message_item_added(0));
    let _ = t.feed_event(&completed_event("completed"));
    // After this, t is finished. A subsequent event produces no events.
    let evs = t.feed_event(&output_text_delta(0, "ignored"));
    assert!(evs.is_empty());
}

#[test]
fn created_event_replaces_msg_id_and_model() {
    let mut t = StreamTranslator::new(
        "synthetic".into(),
        "inbound".into(),
        crate::repair::ToolSchemaRegistry::new(),
    );
    let ev = ResponsesStreamEvent::Created {
        response: ResponsesResponse {
            id: "resp_real".into(),
            object: "response".into(),
            created_at: 0,
            status: "in_progress".into(),
            model: "gpt-5.6-luna".into(),
            output: vec![],
            usage: None,
            error: None,
            incomplete_details: None,
        },
    };
    let evs = t.feed_event(&ev);
    // message_start with the upstream id (prefixed msg_) and model.
    match &evs[0] {
        StreamEvent::MessageStart { message } => {
            assert_eq!(message.id, "msg_resp_real");
            assert_eq!(message.model, "gpt-5.6-luna");
        }
        _ => panic!("expected message_start"),
    }
}

#[test]
fn created_event_usage_surfaces_in_message_start() {
    let mut t = StreamTranslator::new(
        "synthetic".into(),
        "inbound".into(),
        crate::repair::ToolSchemaRegistry::new(),
    );
    let ev = ResponsesStreamEvent::Created {
        response: ResponsesResponse {
            id: "resp_real".into(),
            object: "response".into(),
            created_at: 0,
            status: "in_progress".into(),
            model: "gpt-5.6-luna".into(),
            output: vec![],
            usage: Some(ResponsesUsage {
                input_tokens: 42,
                output_tokens: 5,
                total_tokens: 47,
                input_tokens_details: Some(InputTokensDetails {
                    cached_tokens: 30,
                    cache_write_tokens: 12,
                }),
                output_tokens_details: None,
            }),
            error: None,
            incomplete_details: None,
        },
    };
    let evs = t.feed_event(&ev);
    match &evs[0] {
        StreamEvent::MessageStart { message } => {
            // 42 gross, all of it cache read (30) or write (12): none is plain input.
            assert_eq!(message.usage.input_tokens, 0);
            assert_eq!(message.usage.cache_read_input_tokens, 30);
            assert_eq!(message.usage.cache_creation_input_tokens, 12);
            assert_eq!(message.usage.output_tokens, 0);
            assert_eq!(message.usage.thinking_tokens, 0);
        }
        _ => panic!("expected message_start"),
    }
}

#[test]
fn dangling_args_delta_is_dropped() {
    let mut t = StreamTranslator::new(msg_id(), model(), crate::repair::ToolSchemaRegistry::new());
    t.feed_event(&created_event());
    // No tool use opened yet.
    let evs = t.feed_event(&function_call_args_delta(0, "ignored"));
    assert!(evs.is_empty());
}

#[test]
fn dangling_thinking_delta_is_dropped() {
    let mut t = StreamTranslator::new(msg_id(), model(), crate::repair::ToolSchemaRegistry::new());
    t.feed_event(&created_event());
    let evs = t.feed_event(&reasoning_summary_delta(0, "ignored"));
    assert!(evs.is_empty());
}

#[test]
fn empty_text_delta_is_dropped() {
    let mut t = StreamTranslator::new(msg_id(), model(), crate::repair::ToolSchemaRegistry::new());
    t.feed_event(&created_event());
    t.feed_event(&message_item_added(0));
    let evs = t.feed_event(&output_text_delta(0, ""));
    assert!(evs.is_empty());
}

#[test]
fn empty_args_delta_is_dropped() {
    let mut t = StreamTranslator::new(msg_id(), model(), crate::repair::ToolSchemaRegistry::new());
    t.feed_event(&created_event());
    t.feed_event(&function_call_added(0, "call_1", "tool"));
    let evs = t.feed_event(&function_call_args_delta(0, ""));
    assert!(evs.is_empty());
}

#[test]
fn unknown_event_is_dropped() {
    let mut t = StreamTranslator::new(msg_id(), model(), crate::repair::ToolSchemaRegistry::new());
    let evs = t.feed_event(&ResponsesStreamEvent::Unknown);
    assert!(evs.is_empty());
}

#[test]
fn top_level_error_event_routes_through_emit_error() {
    let mut t = StreamTranslator::new(msg_id(), model(), crate::repair::ToolSchemaRegistry::new());
    t.feed_event(&created_event());
    let evs = t.feed_event(&ResponsesStreamEvent::Error {
        code: Some("server_error".into()),
        message: "boom".into(),
        param: None,
    });
    assert!(evs.iter().any(|e| matches!(e, StreamEvent::Error { .. })));
    assert!(evs.iter().any(|e| matches!(e, StreamEvent::MessageStop {})));
}

#[test]
fn emit_error_closes_open_content_blocks_before_error() {
    // Regression: an upstream error SSE that arrives after a
    // content_block_start must emit content_block_stop before the
    // error event, otherwise the client sees an unbalanced block.
    let mut t = StreamTranslator::new(msg_id(), model(), crate::repair::ToolSchemaRegistry::new());
    t.feed_event(&created_event());
    t.feed_event(&message_item_added(0));
    let _ = t.feed_event(&output_text_delta(0, "partial"));

    let evs = t.emit_error("api_error", "upstream SSE error");

    // Expected order: content_block_stop, error, message_stop.
    assert!(
        matches!(evs[0], StreamEvent::ContentBlockStop { index: 0 }),
        "expected content_block_stop first, got {:?}",
        evs[0]
    );
    assert!(
        matches!(evs[1], StreamEvent::Error { .. }),
        "expected error event second, got {:?}",
        evs[1]
    );
    assert!(
        matches!(evs[2], StreamEvent::MessageStop {}),
        "expected message_stop third, got {:?}",
        evs[2]
    );
}

#[test]
fn emit_error_after_open_tool_block_closes_it_first() {
    // Same regression, but for an in-flight tool_use block.
    let mut t = StreamTranslator::new(msg_id(), model(), crate::repair::ToolSchemaRegistry::new());
    t.feed_event(&created_event());
    t.feed_event(&function_call_added(0, "call_1", "get_weather"));
    let _ = t.feed_event(&function_call_args_delta(0, r#"{"loc"#));

    let evs = t.emit_error("api_error", "upstream SSE error");

    // content_block_stop for the open tool block must precede error.
    let stop_idx = evs
        .iter()
        .position(|e| matches!(e, StreamEvent::ContentBlockStop { index: 0 }));
    let error_idx = evs
        .iter()
        .position(|e| matches!(e, StreamEvent::Error { .. }));
    assert!(
        stop_idx.is_some() && error_idx.is_some() && stop_idx.unwrap() < error_idx.unwrap(),
        "content_block_stop must precede error, got {:?}",
        evs
    );
}

/// Helper: build a `response.output_item.added` event carrying a
/// `WebSearchCall` output item.
fn web_search_call_added(output_index: u32) -> ResponsesStreamEvent {
    ResponsesStreamEvent::OutputItemAdded {
        output_index,
        item: OutputItem::WebSearchCall {
            id: Some(format!("ws_{output_index}")),
            status: Some("in_progress".into()),
        },
    }
}

/// Regression: a `web_search_call` output item in the stream must
/// emit a `server_tool_use` (start+stop) + `web_search_tool_result`
/// (start+stop) block pair so Claude Code's search counter works.
/// Previously these items deserialized as `Unknown` and were dropped,
/// producing "Did 0 searches".
#[test]
fn web_search_call_emits_server_tool_use_and_result_blocks() {
    let mut t = StreamTranslator::new(msg_id(), model(), crate::repair::ToolSchemaRegistry::new());
    t.feed_event(&created_event());

    // The `web_search_call` item alone emits only the `server_tool_use`
    // block (start+stop). The matching `web_search_tool_result` block is
    // deferred until the message content with its `url_citation`
    // annotations completes (Airia streams those after the call item).
    let evs = t.feed_event(&web_search_call_added(0));
    assert_eq!(evs.len(), 2, "expected server_tool_use start+stop");
    let use_id = match &evs[0] {
        StreamEvent::ContentBlockStart {
            content_block: ContentBlockKind::ServerToolUse { name, id, .. },
            ..
        } => {
            assert_eq!(name, "web_search");
            id.clone()
        }
        other => panic!("expected a server_tool_use block, got {other:?}"),
    };
    assert!(
        crate::server_tool_id::is_server_tool_id(&use_id),
        "server_tool_use id must match Anthropic's pattern: {use_id}"
    );
    assert!(matches!(&evs[1], StreamEvent::ContentBlockStop { .. }));

    // Message completes carrying a url_citation annotation; the
    // terminal Completed event flushes the deferred web_search_tool_result
    // block populated with the real result link.
    let _ = t.feed_event(&message_item_done_with_annotations(
        1,
        json!([{
            "type": "url_citation",
            "url": "https://example.com/weather",
            "title": "Example Weather"
        }]),
    ));

    let evs = t.feed_event(&completed_event("completed"));
    assert!(
        evs.iter().any(|e| matches!(e,
            StreamEvent::ContentBlockStart {
                content_block: ContentBlockKind::WebSearchToolResult { block },
                ..
            } if block.tool_use_id == use_id
              && block.content.len() == 1
              && block.content[0].url == "https://example.com/weather"
              && block.content[0].title == "Example Weather"
        )),
        "expected populated WebSearchToolResult on completion, got {evs:?}"
    );

    // The web_search_count must be incremented so the terminal
    // message_delta carries server_tool_use usage.
    let closing = t.finish();
    match &closing[0] {
        StreamEvent::MessageDelta { usage, .. } => {
            let u = usage.as_ref().expect("usage present");
            let stu = u
                .server_tool_use
                .as_ref()
                .expect("server_tool_use usage should be set");
            assert_eq!(stu.web_search_requests, 1);
        }
        other => panic!("expected message_delta, got {other:?}"),
    }
}

/// Regression: Airia streams the real search citations via
/// `response.output_text.annotation.added` events. The proxy must
/// collect those `url_citation`s and populate the
/// `web_search_tool_result` block so Claude Code renders the result
/// links, instead of the previous empty block that produced
/// "No links found.".
#[test]
fn web_search_citations_from_annotation_events_populate_result_block() {
    let mut t = StreamTranslator::new(msg_id(), model(), crate::repair::ToolSchemaRegistry::new());
    t.feed_event(&created_event());

    // 1. web_search_call item -> server_tool_use block.
    let use_events = t.feed_event(&web_search_call_added(0));
    let use_id = use_events
        .iter()
        .find_map(|e| match e {
            StreamEvent::ContentBlockStart {
                content_block: ContentBlockKind::ServerToolUse { id, .. },
                ..
            } => Some(id.clone()),
            _ => None,
        })
        .expect("expected a server_tool_use block");
    assert!(
        crate::server_tool_id::is_server_tool_id(&use_id),
        "server_tool_use id must match Anthropic's pattern: {use_id}"
    );

    // 2. Message text streams, then url_citation annotations arrive.
    let _ = t.feed_event(&message_item_added(1));
    let _ = t.feed_event(&output_text_delta(1, "Japan's population is 122.85M "));
    let evs = t.feed_event(&ResponsesStreamEvent::OutputTextAnnotationAdded {
        item_id: "msg_1".into(),
        output_index: 1,
        content_index: 0,
        annotation_index: 0,
        annotation: json!({
            "type": "url_citation",
            "url": "https://www.stat.go.jp/",
            "title": "Statistics Bureau of Japan"
        }),
    });
    // Annotation events emit no Anthropic events on their own.
    assert!(evs.is_empty());

    // 3. Terminal Completed flushes the web_search_tool_result block.
    let evs = t.feed_event(&completed_event("completed"));
    // The text block stop must precede the result block start (well-ordered).
    let text_stop = evs
        .iter()
        .position(|e| matches!(e, StreamEvent::ContentBlockStop { index: 1 }));
    let result_start = evs.iter().position(|e| {
        matches!(e,
            StreamEvent::ContentBlockStart {
                content_block: ContentBlockKind::WebSearchToolResult { block },
                ..
            } if block.tool_use_id == use_id
              && block.content.len() == 1
              && block.content[0].url == "https://www.stat.go.jp/"
              && block.content[0].title == "Statistics Bureau of Japan"
        )
    });
    assert!(
        text_stop.is_some() && result_start.is_some() && text_stop.unwrap() < result_start.unwrap(),
        "expected text stop before populated result block, got {evs:?}"
    );
}

#[test]
fn duplicate_web_search_citations_are_emitted_once_in_arrival_order() {
    let mut t = StreamTranslator::new(msg_id(), model(), crate::repair::ToolSchemaRegistry::new());
    t.feed_event(&created_event());
    let _ = t.feed_event(&web_search_call_added(0));
    let _ = t.feed_event(&message_item_added(1));

    let first = json!({
        "type": "url_citation",
        "url": "https://example.com/first",
        "title": "First",
    });
    let second = json!({
        "type": "url_citation",
        "url": "https://example.com/second",
        "title": "Second",
    });
    t.feed_event(&ResponsesStreamEvent::OutputTextAnnotationAdded {
        item_id: "msg_1".into(),
        output_index: 1,
        content_index: 0,
        annotation_index: 0,
        annotation: first.clone(),
    });
    t.feed_event(&ResponsesStreamEvent::OutputTextAnnotationAdded {
        item_id: "msg_1".into(),
        output_index: 1,
        content_index: 0,
        annotation_index: 1,
        annotation: first,
    });

    let _ = t.feed_event(&message_item_done_with_annotations(
        1,
        json!([second, {
            "type": "url_citation",
            "url": "https://example.com/first",
            "title": "First (final annotation)",
        }]),
    ));
    let evs = t.feed_event(&completed_event("completed"));

    let result = evs.iter().find_map(|event| match event {
        StreamEvent::ContentBlockStart {
            content_block: ContentBlockKind::WebSearchToolResult { block },
            ..
        } => Some(block),
        _ => None,
    });
    let result = result.expect("expected web search result block");
    assert_eq!(result.content.len(), 2);
    assert_eq!(result.content[0].url, "https://example.com/first");
    assert_eq!(result.content[0].title, "First");
    assert_eq!(result.content[1].url, "https://example.com/second");
    assert_eq!(result.content[1].title, "Second");
}

/// Regression: a `FunctionCall` whose name is `WebSearch` must emit
/// the web search blocks FIRST, then the normal `tool_use` block for
/// the function call. This covers the case where the model invokes
/// web search as a function tool rather than via a `web_search_call`.
#[test]
fn web_search_function_call_emits_blocks_then_tool_use() {
    let mut t = StreamTranslator::new(msg_id(), model(), crate::repair::ToolSchemaRegistry::new());
    t.feed_event(&created_event());

    // server_tool_use start+stop, then the tool_use block start. The
    // matching web_search_tool_result is deferred until the message
    // content completes.
    let evs = t.feed_event(&function_call_added(0, "call_ws", "WebSearch"));
    assert_eq!(evs.len(), 3, "expected server_tool_use + tool_use start");
    assert!(matches!(
        &evs[0],
        StreamEvent::ContentBlockStart {
            content_block: ContentBlockKind::ServerToolUse { .. },
            ..
        }
    ));
    assert!(matches!(&evs[1], StreamEvent::ContentBlockStop { .. }));
    assert!(matches!(
        &evs[2],
        StreamEvent::ContentBlockStart {
            content_block: ContentBlockKind::ToolUse { name, .. },
            ..
        } if name == "WebSearch"
    ));

    // web_search_count must be incremented.
    let _ = t.feed_event(&completed_event("completed"));
    let closing = t.finish();
    match &closing[0] {
        StreamEvent::MessageDelta { usage, .. } => {
            assert!(
                usage
                    .as_ref()
                    .and_then(|u| u.server_tool_use.as_ref())
                    .is_some()
            );
        }
        other => panic!("expected message_delta, got {other:?}"),
    }
}

/// A `web_search` the client declared as its own function tool is in the
/// registry, and a call to it is an ordinary `tool_use`: no `server_tool_use`
/// block, no search count.
#[test]
fn a_client_web_search_function_call_does_not_emit_search_blocks() {
    let mut registry = crate::repair::ToolSchemaRegistry::new();
    registry.insert("web_search".into(), serde_json::json!({"type": "object"}));
    let mut t = StreamTranslator::new(msg_id(), model(), registry);
    t.feed_event(&created_event());

    let evs = t.feed_event(&function_call_added(0, "call_1", "web_search"));
    assert_eq!(evs.len(), 1, "only the tool_use start: {evs:?}");
    assert!(matches!(
        &evs[0],
        StreamEvent::ContentBlockStart {
            content_block: ContentBlockKind::ToolUse { name, .. },
            ..
        } if name == "web_search"
    ));
}

/// A non-web-search function call (e.g. `get_weather`) must NOT emit
/// the web search blocks — only the normal `tool_use` block. Pin this
/// so the name-based detection doesn't over-trigger.
#[test]
fn non_web_search_function_call_does_not_emit_search_blocks() {
    let mut t = StreamTranslator::new(msg_id(), model(), crate::repair::ToolSchemaRegistry::new());
    t.feed_event(&created_event());

    let evs = t.feed_event(&function_call_added(0, "call_1", "get_weather"));
    // Only the tool_use start block; no web search blocks.
    assert_eq!(evs.len(), 1);
    assert!(matches!(
        &evs[0],
        StreamEvent::ContentBlockStart {
            content_block: ContentBlockKind::ToolUse { .. },
            ..
        }
    ));

    let _ = t.feed_event(&completed_event("completed"));
    let closing = t.finish();
    match &closing[0] {
        StreamEvent::MessageDelta { usage, .. } => {
            // No server_tool_use usage for a plain function call.
            assert!(
                usage
                    .as_ref()
                    .and_then(|u| u.server_tool_use.as_ref())
                    .is_none()
            );
        }
        other => panic!("expected message_delta, got {other:?}"),
    }
}

// ─── continuation mode (one executor run inside a larger message) ──────────

fn continuation(first_index: u32) -> StreamTranslator {
    StreamTranslator::continuation(
        "msg_x".into(),
        "m".into(),
        crate::repair::ToolSchemaRegistry::default(),
        first_index,
        vec!["internal".to_owned()],
    )
}

fn event(value: serde_json::Value) -> ResponsesStreamEvent {
    serde_json::from_value(value).unwrap()
}

#[test]
fn a_continuation_sends_no_message_start_and_starts_at_its_index() {
    let mut t = continuation(5);
    let mut events = t.feed_event(&event(serde_json::json!({
        "type": "response.output_item.added", "output_index": 0,
        "item": {"type": "message", "role": "assistant", "content": []}
    })));
    events.extend(t.feed_event(&event(serde_json::json!({
        "type": "response.output_text.delta", "item_id": "m", "output_index": 0,
        "content_index": 0, "delta": "hi"
    }))));
    let json = serde_json::to_string(&events).unwrap();
    assert!(!json.contains("message_start"), "{json}");
    assert!(
        json.contains(r#""type":"content_block_start","index":5"#),
        "{json}"
    );
    assert_eq!(t.next_index(), 6);
}

#[test]
fn a_hidden_call_gets_no_block_no_index_and_no_warning_events() {
    let mut t = continuation(0);
    let mut events = Vec::new();
    for e in [
        serde_json::json!({"type": "response.output_item.added", "output_index": 0,
            "item": {"type": "function_call", "call_id": "c", "name": "internal", "arguments": ""}}),
        serde_json::json!({"type": "response.function_call_arguments.delta",
            "item_id": "f", "output_index": 0, "delta": "{}"}),
        serde_json::json!({"type": "response.function_call_arguments.done",
            "item_id": "f", "output_index": 0, "arguments": "{}"}),
        serde_json::json!({"type": "response.output_item.done", "output_index": 0,
            "item": {"type": "function_call", "call_id": "c", "name": "internal", "arguments": "{}"}}),
    ] {
        events.extend(t.feed_event(&event(e)));
    }
    events.extend(t.close_blocks());
    assert!(events.is_empty(), "{events:?}");
    assert_eq!(t.next_index(), 0);
}

#[test]
fn closing_a_continuation_closes_open_blocks_once_and_never_ends_the_message() {
    let mut t = continuation(2);
    t.feed_event(&event(serde_json::json!({
        "type": "response.output_item.added", "output_index": 0,
        "item": {"type": "message", "role": "assistant", "content": []}
    })));
    let closing = t.close_blocks();
    let json = serde_json::to_string(&closing).unwrap();
    assert!(
        json.contains(r#""type":"content_block_stop","index":2"#),
        "{json}"
    );
    assert!(
        !json.contains("message_delta") && !json.contains("message_stop"),
        "{json}"
    );
    assert!(t.close_blocks().is_empty(), "idempotent");
}

#[test]
fn a_truncated_tool_call_keeps_the_parameters_that_were_finished() {
    // The upstream ran out of output tokens mid-call. Anthropic's own
    // API keeps the parameters that were finished and drops the one
    // that was still being written, so the client sees the same shape
    // here — not `{}`, which would look like a valid empty call.
    let mut t = StreamTranslator::new(msg_id(), model(), crate::repair::ToolSchemaRegistry::new());
    t.feed_event(&created_event());
    t.feed_event(&function_call_added(0, "call_1", "write_file"));
    t.feed_event(&function_call_args_delta(
        0,
        r#"{"path":"/tmp/x","content":"half"#,
    ));
    let evs = t.feed_event(&function_call_args_done(
        0,
        r#"{"path":"/tmp/x","content":"half"#,
    ));
    assert_eq!(evs.len(), 1);
    match &evs[0] {
        StreamEvent::ContentBlockDelta {
            index,
            delta: ContentDelta::InputJsonDelta { partial_json },
        } => {
            assert_eq!(*index, 0);
            assert_eq!(partial_json, r#"{"path":"/tmp/x"}"#);
        }
        _ => panic!("expected input_json_delta"),
    }

    // The stop reason is what tells the client the call is incomplete.
    let evs = t.feed_event(&incomplete_event("max_output_tokens"));
    let closing = t.finish();
    let mut saw_max_tokens = false;
    for event in evs.iter().chain(closing.iter()) {
        if let StreamEvent::MessageDelta { delta, .. } = event
            && delta.stop_reason == Some(StopReason::MaxTokens)
        {
            saw_max_tokens = true;
        }
    }
    assert!(saw_max_tokens, "expected stop_reason max_tokens");
}

#[test]
fn a_truncated_call_from_a_completed_stream_still_reports_max_tokens() {
    // An upstream that claims the turn is complete but ships truncated
    // arguments is lying: the call is incomplete whatever the status
    // says. `max_tokens` is the honest stop reason, and it is the
    // signal the client keys on to retry with a bigger budget — a
    // `tool_use` stop reason would tell it to run the call.
    let mut t = StreamTranslator::new(msg_id(), model(), crate::repair::ToolSchemaRegistry::new());
    t.feed_event(&created_event());
    t.feed_event(&function_call_added(0, "call_1", "write_file"));
    let evs = t.feed_event(&function_call_args_done(
        0,
        r#"{"path":"/tmp/x","content":"half a sen"#,
    ));
    // The finished parameters survive.
    match &evs[0] {
        StreamEvent::ContentBlockDelta {
            delta: ContentDelta::InputJsonDelta { partial_json },
            ..
        } => assert_eq!(partial_json, r#"{"path":"/tmp/x"}"#),
        _ => panic!("expected input_json_delta"),
    }

    let evs = t.feed_event(&completed_event_with_output(
        "completed",
        vec![OutputItem::FunctionCall {
            id: Some("fc_0".into()),
            status: Some("completed".into()),
            call_id: "call_1".into(),
            name: "write_file".into(),
            arguments: r#"{"path":"/tmp/x","content":"half a sen"#.into(),
        }],
    ));
    assert!(!evs.is_empty());
    let closing = t.finish();
    let mut saw_max_tokens = false;
    for event in evs.iter().chain(closing.iter()) {
        if let StreamEvent::MessageDelta { delta, .. } = event
            && delta.stop_reason == Some(StopReason::MaxTokens)
        {
            saw_max_tokens = true;
        }
    }
    assert!(saw_max_tokens, "expected stop_reason max_tokens");
}
