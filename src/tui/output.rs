//! Central output for request log lines.
//!
//! The proxy handler used to call `println!` directly, which works when
//! the proxy is the only thing on stdout but corrupts the TUI when the
//! console is rendering. The TUI takes ownership of stdout (alternate
//! screen + raw mode), so request log lines must be routed through a
//! queue that the TUI drains and renders inside its own frame.
//!
//! The TUI is opt-in: when the operator runs the proxy with
//! `--no-tui` (or pipes stdin), the `Output::Plain` variant writes
//! directly to stdout, preserving the original behavior byte-for-byte.

use std::sync::mpsc;
use std::sync::{Arc, Mutex};

/// One line of output destined for the TUI's log pane.
#[derive(Debug, Clone)]
pub struct LogLine {
    /// Already-formatted text (no trailing newline).
    pub text: String,
    /// Logical category so the TUI can style the line (inbound, response,
    /// warning, error).
    pub kind: LogKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogKind {
    Inbound,
    Response,
    Warning,
    Error,
    Info,
}

/// Where request log lines go.
///
/// Cloning is cheap (the inner state is `Arc`'d). The proxy holds one
/// `OutputSink` per request, the TUI holds the receiver.
#[derive(Clone)]
pub struct OutputSink {
    inner: Arc<OutputInner>,
}

enum OutputInner {
    /// TUI is active. Lines are queued and rendered by the TUI loop.
    Tui {
        tx: mpsc::Sender<LogLine>,
    },
    /// TUI is off. Lines go to stdout, exactly as they did before.
    Plain {
        /// Serialize stdout writes so two requests can't interleave.
        /// `parking_lot` would be a touch faster but the standard
        /// `Mutex` keeps the dep graph unchanged.
        stdout: Arc<Mutex<std::io::Stdout>>,
    },
}

impl OutputSink {
    /// Build a TUI-mode sink. Returns the sink and a receiver the TUI
    /// should drain.
    pub fn tui() -> (Self, mpsc::Receiver<LogLine>) {
        let (tx, rx) = mpsc::channel();
        (
            Self {
                inner: Arc::new(OutputInner::Tui { tx }),
            },
            rx,
        )
    }

    /// Build a plain (no-TUI) sink.
    pub fn plain() -> Self {
        Self {
            inner: Arc::new(OutputInner::Plain {
                stdout: Arc::new(Mutex::new(std::io::stdout())),
            }),
        }
    }

    /// Send a log line. In TUI mode, blocks if the channel is full; in
    /// plain mode, writes the line to stdout.
    pub fn emit(&self, line: LogLine) {
        match &*self.inner {
            OutputInner::Tui { tx } => {
                // If the TUI task has shut down, drop the line silently
                // rather than panicking on the request hot path.
                let _ = tx.send(line);
            }
            OutputInner::Plain { stdout } => {
                use std::io::Write as _;
                let mut out = stdout.lock().unwrap();
                let _ = writeln!(out, "{}", line.text);
                let _ = out.flush();
            }
        }
    }
}

#[cfg(test)]
mod tests {
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
}
