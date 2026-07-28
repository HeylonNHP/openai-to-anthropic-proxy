//! TUI event loop.
//!
//! Owns the terminal (raw mode + alternate screen) for the lifetime of
//! the program. Drains the `OutputSink` log channel and the crossterm
//! event stream via `tokio::select!`, and redraws on each event.

use std::time::Duration;

use crossterm::event::{self, Event, KeyEvent};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;
use tokio::time::{Instant, interval_at};

use super::app::TuiApp;
use super::output::LogLine;

/// Run the TUI until the user quits. `tui_rx` delivers log lines from
/// the proxy handler; dropping `tui_tx` (which the TUI bridge owns
/// via a forwarding task) causes the TUI loop to exit on the next
/// drain.
pub async fn run(
    store: std::sync::Arc<super::runtime::MappingsStore>,
    config_path: Option<std::path::PathBuf>,
    mut tui_rx: mpsc::UnboundedReceiver<LogLine>,
) -> std::io::Result<()> {
    let mut stdout = std::io::stdout();
    enable_raw_mode()?;
    execute!(stdout, EnterAlternateScreen)?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let mut app = TuiApp::new(store, config_path);
    let tick_rate = Duration::from_millis(250);
    let mut ticker = interval_at(Instant::now() + tick_rate, tick_rate);

    let result: std::io::Result<()> = async {
        loop {
            // Drain any pending log lines first so the next frame shows them.
            while let Ok(line) = tui_rx.try_recv() {
                app.push_log(line);
            }

            terminal.draw(|frame| {
                let area = frame.area();
                app.render(frame, area);
            })?;

            tokio::select! {
                biased;
                // New log lines from the proxy handler.
                maybe_line = tui_rx.recv() => {
                    match maybe_line {
                        Some(line) => app.push_log(line),
                        // Sender closed; the proxy has dropped the sink.
                        None => break,
                    }
                }
                // Keyboard input.
                maybe_key = read_key_async() => {
                    let key = match maybe_key? {
                        Some(k) => k,
                        None => continue, // spurious wakeup
                    };
                    if app.on_key(key) {
                        break;
                    }
                }
                // Periodic redraw so the uptime clock ticks and any
                // external state changes (e.g. another task mutating
                // the store) become visible.
                _ = ticker.tick() => {}
            }
        }
        Ok(())
    }
    .await;

    // Always restore the terminal, even on error.
    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen).ok();
    terminal.show_cursor().ok();
    result
}

/// Async wrapper around `crossterm::event::poll` + `read`. Polling
/// the synchronous API on a Tokio worker thread is fine here because
/// the TUI loop is single-threaded and the call returns quickly.
async fn read_key_async() -> std::io::Result<Option<KeyEvent>> {
    if event::poll(Duration::from_millis(50))? {
        match event::read()? {
            Event::Key(k) => Ok(Some(k)),
            // Resize events: redraw, don't return a key.
            Event::Resize(_, _) => Ok(None),
            _ => Ok(None),
        }
    } else {
        // Yield to the runtime so we don't busy-loop.
        tokio::task::yield_now().await;
        Ok(None)
    }
}
