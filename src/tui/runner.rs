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

#[derive(Debug, PartialEq, Eq)]
enum KeyRead {
    Key(KeyEvent),
    Resize,
    None,
}

/// Run the TUI until the user quits. `tui_rx` delivers log lines from
/// the proxy handler; dropping `tui_tx` (which the TUI bridge owns
/// via a forwarding task) causes the TUI loop to exit on the next
/// drain.
pub async fn run(
    store: std::sync::Arc<super::runtime::MappingsStore>,
    stats: std::sync::Arc<super::stats::SessionStatsStore>,
    capabilities: std::sync::Arc<crate::capabilities::CapabilityStore>,
    config_path: Option<std::path::PathBuf>,
    listen_addr: String,
    upstream_base_url: String,
    mut tui_rx: mpsc::UnboundedReceiver<LogLine>,
) -> std::io::Result<()> {
    let mut stdout = std::io::stdout();
    enable_raw_mode()?;
    execute!(stdout, EnterAlternateScreen)?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let mut app = TuiApp::new_with_stores(
        store,
        stats,
        capabilities,
        config_path,
        listen_addr,
        upstream_base_url,
    );
    // The tick exists for exactly one job: keeping the uptime counter
    // and other coarse "wall-clock" UI states visibly fresh. We
    // previously redrew at 4 Hz which was wasteful — every frame
    // walks the full log buffer (see `recent_request_column_widths`
    // and `style_token_usage_line` in `app.rs`) — and it produced a
    // noticeable per-second CPU hum. 1 s matches the granularity of
    // the uptime display and is the cheapest cadence that still
    // looks "live". The dirty-flag fast path means idle ticks
    // produce no draw at all.
    let tick_rate = Duration::from_secs(1);
    let mut ticker = interval_at(Instant::now() + tick_rate, tick_rate);

    let result: std::io::Result<()> = async {
        loop {
            // Drain any pending log lines first so the next frame shows them.
            while let Ok(line) = tui_rx.try_recv() {
                app.push_log(line);
            }

            // Only redraw when something visible actually changed.
            // `push_log`, `apply_mutation`, `save_to_disk`, scroll
            // selection changes, mode transitions, and the toast
            // clearing all flip the dirty flag. The tick alone is no
            // longer sufficient — see Fix 1 in
            // `.claude/plans/foamy-wondering-pike.md`.
            if app.take_dirty() {
                terminal.draw(|frame| {
                    let area = frame.area();
                    app.render(frame, area);
                })?;
            }

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
                    match maybe_key? {
                        KeyRead::Key(key) => {
                            if app.on_key(key) {
                                break;
                            }
                        }
                        KeyRead::Resize => app.mark_dirty(),
                        KeyRead::None => continue, // spurious wakeup
                    }
                }
                // Periodic redraw so the uptime clock ticks. With the
                // dirty-flag gate above, this branch is only followed
                // by an actual `terminal.draw` when something has
                // changed since the last frame.
                _ = ticker.tick() => app.mark_dirty(),
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
async fn read_key_async() -> std::io::Result<KeyRead> {
    if event::poll(Duration::from_millis(50))? {
        match event::read()? {
            Event::Key(k) => Ok(KeyRead::Key(k)),
            Event::Resize(_, _) => Ok(KeyRead::Resize),
            _ => Ok(KeyRead::None),
        }
    } else {
        // Yield to the runtime so we don't busy-loop.
        tokio::task::yield_now().await;
        Ok(KeyRead::None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resize_is_distinct_from_poll_wakeup() {
        assert_ne!(KeyRead::Resize, KeyRead::None);
    }
}
