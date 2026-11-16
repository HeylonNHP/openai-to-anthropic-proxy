use super::*;

#[test]
fn plain_sink_does_not_block_when_no_receiver() {
    let sink = OutputSink::plain();
    sink.emit(LogLine {
        text: "hello".into(),
        kind: LogKind::Info,
    });
}

#[test]
fn tui_sink_drops_when_receiver_gone() {
    let (sink, rx) = OutputSink::tui();
    drop(rx);
    // Should not panic.
    sink.emit(LogLine {
        text: "still works".into(),
        kind: LogKind::Info,
    });
}
