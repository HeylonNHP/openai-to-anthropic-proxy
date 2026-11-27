use futures_util::StreamExt;

use super::*;

fn placeholder() -> Bytes {
    placeholder_frame(&placeholder_message("claude-test"))
}

/// The placeholder leads: the client's very first chunk opens the message,
/// before the source has produced anything.
#[tokio::test(start_paused = true)]
async fn the_placeholder_frame_leads() {
    let body = keep_alive_response(placeholder(), futures_util::stream::pending::<Bytes>());
    let mut chunks = body.into_body().into_data_stream();
    let first = chunks.next().await.unwrap().unwrap();
    let first = String::from_utf8_lossy(&first);
    assert!(first.contains("event: message_start"), "{first}");
    // The source is silent, so the next chunk is the keep-alive.
    let second = chunks.next().await.unwrap().unwrap();
    let second = String::from_utf8_lossy(&second);
    assert!(second.contains("event: ping"), "{second}");
    // …and it repeats rather than ending the stream.
    let third = chunks.next().await.unwrap().unwrap();
    let third = String::from_utf8_lossy(&third);
    assert!(third.contains("event: ping"), "{third}");
}

/// A chunk from the source resets the ping cadence: no keep-alive fires
/// while bytes are flowing.
#[tokio::test(start_paused = true)]
async fn source_bytes_reset_the_ping_timer() {
    let source = futures_util::stream::iter(vec![Bytes::from_static(b"data: real\n\n")])
        .chain(futures_util::stream::pending::<Bytes>());
    let body = keep_alive_response(placeholder(), source);
    let mut chunks = body.into_body().into_data_stream();

    let first = chunks.next().await.unwrap().unwrap();
    assert!(String::from_utf8_lossy(&first).contains("event: message_start"));
    let second = chunks.next().await.unwrap().unwrap();
    assert_eq!(&second[..], b"data: real\n\n");
    // The source went quiet again: one ping interval later, a ping.
    let third = chunks.next().await.unwrap().unwrap();
    assert!(String::from_utf8_lossy(&third).contains("event: ping"));
}

/// The body ends with the source — no trailing keep-alives after the
/// splice has closed the message.
#[tokio::test(start_paused = true)]
async fn the_stream_ends_with_the_source() {
    let source = futures_util::stream::iter(vec![Bytes::from_static(b"data: last\n\n")]);
    let body = keep_alive_response(placeholder(), source);
    let body: Vec<Result<Bytes, _>> = body.into_body().into_data_stream().collect().await;
    let rendered: Vec<u8> = body
        .into_iter()
        .flat_map(|chunk| chunk.unwrap_or_default())
        .collect();
    let rendered = String::from_utf8_lossy(&rendered);
    assert!(rendered.contains("event: message_start"));
    assert!(rendered.contains("data: last"));
    assert!(!rendered.contains("event: ping"));
}

/// The placeholder is an empty assistant message with zero usage: the
/// closing `message_delta` is where the real numbers land.
#[test]
fn the_placeholder_message_is_empty() {
    let message = placeholder_message("claude-test");
    assert!(message.content.is_empty());
    assert!(message.stop_reason.is_none());
    assert_eq!(message.usage.input_tokens, 0);
    assert_eq!(message.usage.output_tokens, 0);
    assert!(message.id.starts_with("msg_"));
    assert_eq!(message.model, "claude-test");
}

/// Dropping the guard cancels a task that is still working.
#[tokio::test(start_paused = true)]
async fn dropping_the_guard_cancels_a_running_task() {
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let guard = AbortOnDrop::spawn(async move {
        let _flag = tx;
        // Far longer than the test runs: only cancellation ends it early.
        tokio::time::sleep(Duration::from_secs(3600)).await;
    });
    tokio::task::yield_now().await;

    drop(guard);

    // The task's future was dropped, which drops the sender it held.
    assert!(
        tokio::time::timeout(Duration::from_secs(1), rx)
            .await
            .expect("the task was cancelled promptly")
            .is_err(),
        "the sender is dropped, not sent"
    );
}

/// A task that has already finished is unaffected by the guard going away.
#[tokio::test]
async fn dropping_the_guard_after_the_task_finished_is_harmless() {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let guard = AbortOnDrop::spawn(async move {
        let _ = tx.send(7);
    });
    assert_eq!(rx.await.unwrap(), 7);
    drop(guard);
}
