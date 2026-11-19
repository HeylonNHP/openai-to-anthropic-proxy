use super::*;
use crate::anthropic::{StopReason, Usage};
use serde_json::json;

fn raw(kind: &str) -> ResponseContentBlock {
    ResponseContentBlock::Raw(json!({"type": kind}))
}

fn text(t: &str) -> ResponseContentBlock {
    ResponseContentBlock::Text { text: t.to_owned() }
}

fn tail(blocks: Vec<ResponseContentBlock>, stop: Option<StopReason>) -> Tail {
    Tail {
        blocks,
        stop_reason: stop,
        stop_sequence: None,
        usage: Usage::default(),
    }
}

/// `(event type, index)` of every event, for compact assertions.
fn shape(events: &[StreamEvent]) -> Vec<(String, Option<u32>)> {
    events
        .iter()
        .map(|e| {
            let v = serde_json::to_value(e).unwrap();
            (
                v["type"].as_str().unwrap().to_owned(),
                v["index"].as_u64().map(|i| u32::try_from(i).unwrap()),
            )
        })
        .collect()
}

fn state(rx: UnboundedReceiver<LoopEvent>, ping_after: Duration) -> BodyState {
    BodyState {
        rx,
        next_index: 0,
        done: false,
        ping_after,
    }
}

fn chunk_text(chunk: &Bytes) -> String {
    String::from_utf8_lossy(chunk).into_owned()
}

#[test]
fn blocks_are_numbered_contiguously_across_calls() {
    let mut next = 0;
    let first = block_stream_events(&mut next, vec![raw("server_tool_use"), raw("result")]);
    let second = block_stream_events(&mut next, vec![text("answer")]);

    assert_eq!(next, 3);
    let indices: Vec<u32> = shape(&first)
        .iter()
        .chain(shape(&second).iter())
        .filter(|(kind, _)| kind == "content_block_start")
        .map(|(_, i)| i.unwrap())
        .collect();
    assert_eq!(indices, [0, 1, 2]);
}

#[test]
fn no_blocks_means_no_events_and_no_index_used() {
    let mut next = 4;
    assert!(block_stream_events(&mut next, Vec::new()).is_empty());
    assert_eq!(next, 4);
}

#[test]
fn the_tail_sends_unseen_blocks_then_closes_the_message() {
    let mut next = 2;
    let events = tail_stream_events(
        &mut next,
        tail(vec![text("last")], Some(StopReason::EndTurn)),
    );
    let kinds: Vec<String> = shape(&events).into_iter().map(|(k, _)| k).collect();
    assert_eq!(
        kinds,
        [
            "content_block_start",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop"
        ]
    );
    assert_eq!(shape(&events)[0].1, Some(2), "numbering continues");
}

#[test]
fn an_empty_tail_still_closes_the_message_with_its_stop_reason() {
    let mut next = 3;
    let events = tail_stream_events(&mut next, tail(Vec::new(), Some(StopReason::PauseTurn)));
    assert_eq!(events.len(), 2);
    let delta = serde_json::to_value(&events[0]).unwrap();
    assert_eq!(delta["delta"]["stop_reason"], "pause_turn");
    assert_eq!(
        serde_json::to_value(&events[1]).unwrap()["type"],
        "message_stop"
    );
}

#[test]
fn a_failure_after_the_start_is_an_sse_error_event() {
    let event = serde_json::to_value(error_event(&AppError::Internal("boom".into()))).unwrap();
    assert_eq!(event["type"], "error");
    assert_eq!(event["error"]["type"], "api_error");
    assert_eq!(event["error"]["message"], "boom");
}

#[tokio::test]
async fn the_body_forwards_blocks_then_finishes_and_ends() {
    let (tx, rx) = mpsc::unbounded_channel();
    tx.send(LoopEvent::Blocks(vec![raw("server_tool_use")]))
        .unwrap();
    tx.send(LoopEvent::Finished(tail(
        vec![text("done")],
        Some(StopReason::EndTurn),
    )))
    .unwrap();

    let (first, st) = next_chunk(state(rx, Duration::from_secs(30)))
        .await
        .unwrap();
    assert!(chunk_text(&first).contains("event: content_block_start"));
    assert_eq!(st.next_index, 1);

    let (last, st) = next_chunk(st).await.unwrap();
    let last = chunk_text(&last);
    assert!(
        last.contains("done") && last.contains("event: message_stop"),
        "{last}"
    );
    assert!(st.done);
    assert!(
        next_chunk(st).await.is_none(),
        "nothing follows message_stop"
    );
}

#[tokio::test]
async fn a_quiet_stream_is_kept_alive_with_pings() {
    let (_tx, rx) = mpsc::unbounded_channel::<LoopEvent>();
    let (chunk, st) = next_chunk(state(rx, Duration::from_millis(20)))
        .await
        .unwrap();
    assert!(
        chunk_text(&chunk).starts_with("event: ping"),
        "{}",
        chunk_text(&chunk)
    );
    assert!(!st.done, "a ping does not end the stream");
}

#[tokio::test]
async fn a_failure_event_ends_the_stream_with_an_error() {
    let (tx, rx) = mpsc::unbounded_channel();
    tx.send(LoopEvent::Failed(AppError::Internal("late failure".into())))
        .unwrap();
    let (chunk, st) = next_chunk(state(rx, Duration::from_secs(30)))
        .await
        .unwrap();
    assert!(chunk_text(&chunk).contains("event: error"));
    assert!(next_chunk(st).await.is_none());
}

#[tokio::test]
async fn a_loop_that_vanishes_ends_the_stream_with_an_error_not_a_hang() {
    let (tx, rx) = mpsc::unbounded_channel::<LoopEvent>();
    drop(tx);
    let (chunk, st) = next_chunk(state(rx, Duration::from_secs(30)))
        .await
        .unwrap();
    assert!(chunk_text(&chunk).contains("event: error"));
    assert!(next_chunk(st).await.is_none());
}

#[tokio::test]
async fn events_that_do_not_belong_after_the_start_are_ignored() {
    let (tx, rx) = mpsc::unbounded_channel();
    tx.send(LoopEvent::Started(crate::anthropic::Message {
        id: "m".into(),
        r#type: "message",
        role: "assistant",
        content: Vec::new(),
        model: "m".into(),
        stop_reason: None,
        stop_sequence: None,
        usage: Usage::default(),
    }))
    .unwrap();
    tx.send(LoopEvent::Blocks(vec![text("x")])).unwrap();
    let (chunk, _) = next_chunk(state(rx, Duration::from_secs(30)))
        .await
        .unwrap();
    assert!(chunk_text(&chunk).contains("event: content_block_start"));
}
