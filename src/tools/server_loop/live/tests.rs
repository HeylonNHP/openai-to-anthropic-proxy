use super::*;
use crate::tools::server_loop::run::{Emitter, LoopEvent};
use serde_json::{Value, json};
use tokio::sync::mpsc::{self, UnboundedReceiver};

const HIDDEN: &[&str] = &["proxy_tool"];

fn envelope(status: &str, output: Value) -> Value {
    json!({
        "id": "resp_1", "object": "response", "created_at": 1, "status": status,
        "model": "m", "output": output,
        "usage": {"input_tokens": 3, "output_tokens": 4, "total_tokens": 7},
    })
}

/// A scripted upstream stream of `(type, fields)` events.
fn sse(events: Vec<(&str, Value)>) -> Vec<Result<SseEvent, String>> {
    events
        .into_iter()
        .map(|(kind, mut data)| {
            data["type"] = json!(kind);
            Ok(SseEvent {
                event: kind.to_owned(),
                data: data.to_string(),
                ..SseEvent::default()
            })
        })
        .collect()
}

fn created() -> (&'static str, Value) {
    (
        "response.created",
        json!({"response": envelope("in_progress", json!([]))}),
    )
}

fn completed(output: Value) -> (&'static str, Value) {
    (
        "response.completed",
        json!({"response": envelope("completed", output)}),
    )
}

fn message_added(index: u32) -> (&'static str, Value) {
    (
        "response.output_item.added",
        json!({"output_index": index, "item": {
            "type": "message", "role": "assistant", "content": [], "status": "in_progress"}}),
    )
}

fn text_delta(index: u32, delta: &str) -> (&'static str, Value) {
    (
        "response.output_text.delta",
        json!({"item_id": "m", "output_index": index, "content_index": 0, "delta": delta}),
    )
}

fn call_added(index: u32, name: &str, call_id: &str) -> (&'static str, Value) {
    (
        "response.output_item.added",
        json!({"output_index": index, "item": {
            "type": "function_call", "call_id": call_id, "name": name, "arguments": ""}}),
    )
}

fn args_delta(index: u32, delta: &str) -> (&'static str, Value) {
    (
        "response.function_call_arguments.delta",
        json!({"item_id": "f", "output_index": index, "delta": delta}),
    )
}

fn context() -> RunContext<'static> {
    RunContext {
        msg_id: "msg_1".to_owned(),
        model: "m".to_owned(),
        registry: ToolSchemaRegistry::default(),
        hidden: HIDDEN,
    }
}

/// Everything the loop sent the client, flattened to Anthropic events, and
/// the content index it ended on.
fn drain(rx: &mut UnboundedReceiver<LoopEvent>) -> (Vec<Value>, u32) {
    let mut events = Vec::new();
    let mut next = 0;
    while let Ok(event) = rx.try_recv() {
        if let LoopEvent::Live(batch, index) = event {
            next = index;
            events.extend(batch.iter().map(|e| serde_json::to_value(e).unwrap()));
        }
    }
    (events, next)
}

fn kinds(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .map(|e| {
            let kind = e["type"].as_str().unwrap();
            e["index"]
                .as_u64()
                .map_or_else(|| kind.to_owned(), |i| format!("{kind}:{i}"))
        })
        .collect()
}

#[tokio::test]
async fn text_is_forwarded_and_the_hidden_call_is_not() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut out = Emitter::new(Some(&tx));
    let stream = futures_util::stream::iter(sse(vec![
        created(),
        message_added(0),
        text_delta(0, "Hello "),
        text_delta(0, "there"),
        call_added(1, "proxy_tool", "call_1"),
        args_delta(1, r#"{"q":"#),
        args_delta(1, r#""x"}"#),
        completed(json!([])),
    ]));

    let resp = consume(stream, &mut out, context()).await.unwrap();

    let (events, next) = drain(&mut rx);
    assert_eq!(
        kinds(&events),
        [
            "content_block_start:0",
            "content_block_delta:0",
            "content_block_delta:0",
            "content_block_stop:0"
        ],
        "the hidden call produced nothing, and no message_start/stop was sent"
    );
    assert_eq!(next, 1);
    assert_eq!(out.next_index(), 1);

    // The run, as the loop sees it: the text and the call with its arguments,
    // although the terminal event's own output was empty.
    assert_eq!(resp.output.len(), 2);
    match &resp.output[0] {
        OutputItem::Message { content, .. } => match &content[0] {
            OutputContentPart::OutputText { text, .. } => assert_eq!(text, "Hello there"),
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
    match &resp.output[1] {
        OutputItem::FunctionCall {
            name,
            call_id,
            arguments,
            ..
        } => {
            assert_eq!((name.as_str(), call_id.as_str()), ("proxy_tool", "call_1"));
            assert_eq!(arguments, r#"{"q":"x"}"#);
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_second_run_continues_the_numbering() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut out = Emitter::new(Some(&tx));
    for text in ["one", "two"] {
        let stream = futures_util::stream::iter(sse(vec![
            created(),
            message_added(0),
            text_delta(0, text),
            completed(json!([])),
        ]));
        consume(stream, &mut out, context()).await.unwrap();
    }
    let (events, next) = drain(&mut rx);
    let starts: Vec<u64> = events
        .iter()
        .filter(|e| e["type"] == "content_block_start")
        .map(|e| e["index"].as_u64().unwrap())
        .collect();
    assert_eq!(starts, [0, 1]);
    assert_eq!(next, 2);
}

#[tokio::test]
async fn a_client_call_is_streamed_with_its_arguments() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut out = Emitter::new(Some(&tx));
    let stream = futures_util::stream::iter(sse(vec![
        created(),
        call_added(0, "Read", "call_r"),
        args_delta(0, r#"{"path":"a"}"#),
        completed(json!([])),
    ]));
    consume(stream, &mut out, context()).await.unwrap();
    let (events, _) = drain(&mut rx);
    let text = serde_json::to_string(&events).unwrap();
    assert!(text.contains(r#""name":"Read""#), "{text}");
    assert!(text.contains(r#"{\"path\":\"a\"}"#), "{text}");
    assert_eq!(
        kinds(&events).last().map(String::as_str),
        Some("content_block_stop:0")
    );
}

#[tokio::test]
async fn done_items_supply_what_the_stream_did_not() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut out = Emitter::new(Some(&tx));
    let stream = futures_util::stream::iter(sse(vec![
        created(),
        (
            "response.output_item.added",
            json!({"output_index": 0, "item": {"type": "reasoning", "id": "rs_1", "summary": []}}),
        ),
        (
            "response.reasoning_summary_text.delta",
            json!({"item_id": "rs_1", "output_index": 0, "summary_index": 0, "delta": "thinking"}),
        ),
        (
            "response.output_item.done",
            json!({"output_index": 0, "item": {
                "type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "SECRET"}}),
        ),
        call_added(1, "proxy_tool", "call_1"),
        args_delta(1, r#"{"a":1}"#),
        (
            "response.output_item.done",
            json!({"output_index": 1, "item": {
                "type": "function_call", "id": "fc", "call_id": "call_1",
                "name": "proxy_tool", "arguments": ""}}),
        ),
        completed(json!([])),
    ]));
    let resp = consume(stream, &mut out, context()).await.unwrap();

    match &resp.output[0] {
        OutputItem::Reasoning {
            summary,
            encrypted_content,
            ..
        } => {
            assert_eq!(encrypted_content, "SECRET", "the done item's blob is kept");
            assert_eq!(summary[0].text, "thinking", "the streamed summary is kept");
        }
        other => panic!("{other:?}"),
    }
    match &resp.output[1] {
        OutputItem::FunctionCall { arguments, .. } => assert_eq!(arguments, r#"{"a":1}"#),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn the_terminal_output_is_used_when_it_has_content() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut out = Emitter::new(Some(&tx));
    let stream = futures_util::stream::iter(sse(vec![
        created(),
        completed(json!([{"type": "message", "role": "assistant", "content": [
            {"type": "output_text", "text": "whole", "annotations": []}]}])),
    ]));
    let resp = consume(stream, &mut out, context()).await.unwrap();
    assert_eq!(resp.output.len(), 1);
}

#[tokio::test]
async fn a_stream_that_ends_without_a_terminal_event_closes_its_blocks() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut out = Emitter::new(Some(&tx));
    let stream = futures_util::stream::iter(sse(vec![
        created(),
        message_added(0),
        text_delta(0, "half"),
    ]));

    let err = consume(stream, &mut out, context()).await.unwrap_err();

    assert!(err.to_string().contains("terminal"), "{err}");
    let (events, next) = drain(&mut rx);
    assert_eq!(
        kinds(&events),
        [
            "content_block_start:0",
            "content_block_delta:0",
            "content_block_stop:0"
        ]
    );
    assert_eq!(next, 1, "the next block does not reuse the closed index");
}

#[tokio::test]
async fn a_failed_response_closes_its_blocks_and_fails_the_run() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut out = Emitter::new(Some(&tx));
    let mut failed = envelope("failed", json!([]));
    failed["error"] = json!({"code": "boom", "message": "kaput"});
    let stream = futures_util::stream::iter(sse(vec![
        created(),
        message_added(0),
        text_delta(0, "half"),
        ("response.failed", json!({"response": failed})),
    ]));

    let err = consume(stream, &mut out, context()).await.unwrap_err();

    assert!(err.to_string().contains("kaput"), "{err}");
    let (events, _) = drain(&mut rx);
    assert_eq!(kinds(&events).last().unwrap(), "content_block_stop:0");
    assert!(
        !kinds(&events)
            .iter()
            .any(|k| k == "message_stop" || k == "error"),
        "the loop, not this run, decides how the message ends: {events:?}"
    );
}

#[tokio::test]
async fn a_broken_stream_is_an_upstream_error() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut out = Emitter::new(Some(&tx));
    let stream = futures_util::stream::iter(vec![Err::<SseEvent, String>("reset".to_owned())]);
    let err = consume(stream, &mut out, context()).await.unwrap_err();
    assert!(err.to_string().contains("reset"), "{err}");
}

#[tokio::test]
async fn malformed_events_are_skipped() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut out = Emitter::new(Some(&tx));
    let mut events = sse(vec![created()]);
    events.push(Ok(SseEvent {
        data: "{not json".to_owned(),
        ..SseEvent::default()
    }));
    events.extend(sse(vec![completed(json!([]))]));
    consume(futures_util::stream::iter(events), &mut out, context())
        .await
        .unwrap();
}

#[tokio::test]
async fn a_vanished_client_stops_the_run() {
    let (tx, rx) = mpsc::unbounded_channel();
    let mut out = Emitter::new(Some(&tx));
    drop(rx);
    // An upstream that never finishes.
    let stream = futures_util::stream::pending::<Result<SseEvent, String>>();
    let err = consume(stream, &mut out, context()).await.unwrap_err();
    assert!(err.to_string().contains("disconnected"), "{err}");
}
