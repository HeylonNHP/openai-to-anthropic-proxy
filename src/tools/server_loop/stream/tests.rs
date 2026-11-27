use super::*;
use crate::anthropic::{StopReason, Usage};
use futures_util::StreamExt;
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

fn chunk_text(chunk: &Bytes) -> String {
    String::from_utf8_lossy(chunk).into_owned()
}

/// The next encoded chunk of a `loop_events` stream.
async fn next_bytes(stream: &mut (impl Stream<Item = Bytes> + Unpin)) -> Option<String> {
    stream.next().await.map(|chunk| chunk_text(&chunk))
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
    tx.send(LoopEvent::Blocks(vec![text("answer")])).unwrap();
    tx.send(LoopEvent::Finished(tail(
        vec![text("done")],
        Some(StopReason::EndTurn),
    )))
    .unwrap();

    let mut stream = Box::pin(loop_events(rx));

    let first = next_bytes(&mut stream).await.unwrap();
    assert!(first.contains("event: content_block_start"), "{first}");

    // Numbering carries across chunks: the second block is index 1, the
    // tail's block index 2.
    let second = next_bytes(&mut stream).await.unwrap();
    assert!(second.contains(r#""index":1"#), "{second}");

    let last = next_bytes(&mut stream).await.unwrap();
    assert!(
        last.contains("done") && last.contains("event: message_stop"),
        "{last}"
    );
    assert!(
        last.contains(r#""index":2"#),
        "the tail's block continues the numbering: {last}"
    );
    assert!(
        next_bytes(&mut stream).await.is_none(),
        "nothing follows message_stop"
    );
}

/// The keep-alive now lives in the shared early-start body; here it wraps
/// the loop exactly as `response` does: a silent loop keeps the client
/// supplied with `ping`s rather than silence.
#[tokio::test(start_paused = true)]
async fn a_quiet_stream_is_kept_alive_with_pings() {
    let (_tx, rx) = mpsc::unbounded_channel::<LoopEvent>();
    let body = crate::early_start::keep_alive_response(
        crate::early_start::placeholder_frame(&placeholder_message("m")),
        loop_events(rx),
    );
    let mut chunks = body.into_body().into_data_stream();

    let first = chunk_text(&chunks.next().await.unwrap().unwrap());
    assert!(first.contains("event: message_start"), "{first}");
    let second = chunk_text(&chunks.next().await.unwrap().unwrap());
    assert!(second.starts_with("event: ping"), "{second}");
}

#[tokio::test]
async fn a_failure_event_ends_the_stream_with_an_error() {
    let (tx, rx) = mpsc::unbounded_channel();
    tx.send(LoopEvent::Failed(AppError::Internal("late failure".into())))
        .unwrap();
    let mut stream = Box::pin(loop_events(rx));
    let chunk = next_bytes(&mut stream).await.unwrap();
    assert!(chunk.contains("event: error"));
    assert!(next_bytes(&mut stream).await.is_none());
}

#[tokio::test]
async fn a_loop_that_vanishes_ends_the_stream_with_an_error_not_a_hang() {
    let (tx, rx) = mpsc::unbounded_channel::<LoopEvent>();
    drop(tx);
    let mut stream = Box::pin(loop_events(rx));
    let chunk = next_bytes(&mut stream).await.unwrap();
    assert!(chunk.contains("event: error"));
    assert!(next_bytes(&mut stream).await.is_none());
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
    let mut stream = Box::pin(loop_events(rx));
    let chunk = next_bytes(&mut stream).await.unwrap();
    assert!(chunk.contains("event: content_block_start"));
}

fn started(id: &str) -> LoopEvent {
    let mut message = placeholder_message("m");
    message.id = id.to_owned();
    LoopEvent::Started(message)
}

/// The next data chunk of a started response's body.
async fn body_chunk(body: &mut axum::body::BodyDataStream) -> String {
    let chunk = body.next().await.expect("a chunk").expect("no body error");
    chunk_text(&chunk)
}

fn streamed_body(streamed: Streamed) -> axum::body::BodyDataStream {
    match streamed {
        Streamed::Response(response) => response.into_body().into_data_stream(),
        Streamed::Fallback(..) => panic!("expected a streaming response"),
    }
}

#[test]
fn the_placeholder_is_an_empty_assistant_message_for_the_requested_model() {
    let message = placeholder_message("claude-sonnet-5-5");
    assert!(message.id.starts_with("msg_"), "{}", message.id);
    assert_eq!(message.model, "claude-sonnet-5-5");
    assert_eq!(message.role, "assistant");
    assert!(message.content.is_empty());
    assert!(message.stop_reason.is_none());
}

#[tokio::test]
async fn a_fast_first_run_starts_the_stream_with_its_own_message() {
    let (tx, rx) = mpsc::unbounded_channel();
    tx.send(started("msg_from_the_run")).unwrap();
    let streamed = start(rx, placeholder_message("m"), Duration::from_secs(30))
        .await
        .unwrap();
    let mut body = streamed_body(streamed);
    let first = body_chunk(&mut body).await;
    assert!(first.contains("msg_from_the_run"), "{first}");
}

#[tokio::test]
async fn a_fast_first_failure_is_still_a_plain_error() {
    let (tx, rx) = mpsc::unbounded_channel();
    tx.send(LoopEvent::Failed(AppError::Internal("quick".into())))
        .unwrap();
    let result = start(rx, placeholder_message("m"), Duration::from_secs(30)).await;
    assert!(matches!(result, Err(AppError::Internal(m)) if m == "quick"));
}

#[tokio::test]
async fn a_slow_first_run_starts_the_stream_early_with_the_placeholder() {
    let (tx, rx) = mpsc::unbounded_channel();
    let placeholder = placeholder_message("m");
    let placeholder_id = placeholder.id.clone();

    // Nothing has arrived when the deadline passes: the response starts.
    let streamed = start(rx, placeholder, Duration::from_millis(10))
        .await
        .unwrap();
    let mut body = streamed_body(streamed);
    let first = body_chunk(&mut body).await;
    assert!(first.contains("event: message_start"), "{first}");
    assert!(first.contains(&placeholder_id), "{first}");

    // The run then finishes: its own message_start is NOT sent again, its
    // blocks and the end follow.
    tx.send(started("msg_from_the_run")).unwrap();
    tx.send(LoopEvent::Finished(tail(
        vec![text("answer")],
        Some(StopReason::EndTurn),
    )))
    .unwrap();
    let rest = body_chunk(&mut body).await;
    assert!(!rest.contains("message_start"), "{rest}");
    assert!(!rest.contains("msg_from_the_run"), "{rest}");
    assert!(
        rest.contains("answer") && rest.contains("event: message_stop"),
        "{rest}"
    );
}

#[tokio::test]
async fn a_slow_first_run_that_then_fails_ends_the_early_stream_with_an_error() {
    let (tx, rx) = mpsc::unbounded_channel();
    let streamed = start(rx, placeholder_message("m"), Duration::from_millis(10))
        .await
        .unwrap();
    let mut body = streamed_body(streamed);
    body_chunk(&mut body).await;

    tx.send(LoopEvent::Failed(AppError::Internal("slow failure".into())))
        .unwrap();
    let rest = body_chunk(&mut body).await;
    assert!(
        rest.contains("event: error") && rest.contains("slow failure"),
        "{rest}"
    );
    assert!(
        body.next().await.is_none(),
        "the stream ends after the error"
    );
}

#[tokio::test]
async fn live_events_are_sent_as_they_are_and_numbering_follows_them() {
    let (tx, rx) = mpsc::unbounded_channel();
    // A run streamed live used blocks 0 and 1; the next block the response
    // numbers itself must be 2.
    tx.send(LoopEvent::Live(
        vec![
            StreamEvent::ContentBlockStop { index: 0 },
            StreamEvent::ContentBlockStop { index: 1 },
        ],
        2,
    ))
    .unwrap();
    tx.send(LoopEvent::Blocks(vec![text("after")])).unwrap();

    let mut stream = Box::pin(loop_events(rx));

    let live = next_bytes(&mut stream).await.unwrap();
    assert_eq!(live.matches("event: content_block_stop").count(), 2);

    let blocks = next_bytes(&mut stream).await.unwrap();
    assert!(
        blocks.contains(r#""index":2"#),
        "numbering followed the live run: {blocks}"
    );
}
