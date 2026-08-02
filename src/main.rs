//! Proxy server entrypoint.
//!
//! Loads configuration, builds a shared reqwest client and an axum
//! router, then binds the configured listen address.
//!
//! Two modes of operation:
//!
//! - **Default (TUI on)**: A full-screen TUI owns the terminal and
//!   shows the live model-routing table. The axum server runs in a
//!   background Tokio task. Press `q` (or `Ctrl-C`) in the TUI to
//!   shut the proxy down gracefully.
//! - **No-TUI (`--no-tui`)**: The proxy runs as a plain foreground
//!   process and writes request log lines to stdout. Useful for
//!   piping, CI, and running under a process manager that already
//!   captures output.
//!
//! In both modes, `tracing` events are silent by default; set
//! `log_to_disk = true` in `proxy.json` (or `LOG_TO_DISK=1`) to
//! capture them in `target/logs/proxy.log` for postmortem inspection.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use openai_to_anthropic_proxy::{
    Config, MappingsStore, OutputSink, RuntimeMappings, SessionStatsStore,
};
use tokio::net::TcpListener;
use tokio::signal;
use tokio::sync::{mpsc, watch};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

#[tokio::main]
async fn main() -> Result<()> {
    // Parse a single optional `--no-tui` flag from argv. Anything else
    // (env vars, subcommands) is intentionally unsupported to keep the
    // surface area small; configuration lives in `proxy.json`.
    let no_tui = std::env::args().any(|a| a == "--no-tui");

    let config = Config::load().context("load configuration")?;
    let _log_guard = init_tracing(config.log_to_disk);

    // Runtime mappings: start from whatever the static config has, and
    // also seed the on-disk snapshot to the same value. A TUI save
    // marks them equal; a TUI edit that hasn't been saved yet diverges
    // them and shows `*` markers.
    let store = MappingsStore::seeded(Arc::new(RuntimeMappings::from_config(&config)));
    let store_arc = Arc::new(store);
    let stats = Arc::new(SessionStatsStore::new());

    if config.proxy_key.is_none() {
        // Loud, single-line warning, written to stderr so it surfaces
        // even when the TUI is on (the TUI starts *after* this).
        eprintln!(
            "WARNING: proxy_key is not set; /v1/messages accepts requests from any client. \
             Set `proxy_key` in proxy.json or PROXY_KEY env to require authentication."
        );
    }

    let client = build_upstream_client(&config)?;

    let listener = TcpListener::bind(config.listen_addr)
        .await
        .with_context(|| format!("bind {}", config.listen_addr))?;

    eprintln!(
        "Proxy listening on {} -> {}{}",
        config.listen_addr,
        config.upstream_base_url.trim_end_matches('/'),
        config.upstream_path
    );

    // The server runs until either the graceful-shutdown signal fires
    // (Ctrl-C / SIGTERM / console close) or the TUI asks to quit.
    //
    // We use a `watch::channel` as the "TUI asked to quit" signal.
    // After the TUI returns, `main()` drops `shutdown_tx`; the
    // `watch::Receiver` then resolves its `changed()` future, which
    // `shutdown_signal` races against the OS signals. This makes
    // `q` in the TUI shut the server down without requiring the
    // user to press Ctrl-C a second time.
    let (shutdown_tx, shutdown_rx) = watch::channel(());
    let server_shutdown = shutdown_signal(shutdown_rx);

    if no_tui {
        // No TUI: build a plain router that writes request lines to
        // stdout, preserving the original behavior.
        let app = openai_to_anthropic_proxy::proxy::router_with_stats(
            Arc::new(config.clone()),
            store_arc.clone(),
            client,
            OutputSink::plain(),
            stats.clone(),
        );
        axum::serve(listener, app)
            .with_graceful_shutdown(server_shutdown)
            .await
            .context("axum server error")?;
        return Ok(());
    }

    // TUI mode: build the TUI's log channel, wire the router to
    // feed it, run the server in a background task, and drive the
    // TUI on the main task.
    let (tui_tx, tui_rx) = mpsc::unbounded_channel::<openai_to_anthropic_proxy::tui::LogLine>();
    let tui_sink = TuiBridge::new(tui_tx.clone());
    let app = openai_to_anthropic_proxy::proxy::router_with_stats(
        Arc::new(config.clone()),
        store_arc.clone(),
        client,
        tui_sink.into_sink(),
        stats.clone(),
    );
    let server = axum::serve(listener, app).with_graceful_shutdown(server_shutdown);
    let server_task = tokio::spawn(async move {
        if let Err(e) = server.await {
            eprintln!("axum server error: {e:?}");
        }
    });

    // Run the TUI. It owns the terminal until the user quits.
    let config_path = Some(PathBuf::from("proxy.json"));
    let tui_result = openai_to_anthropic_proxy::tui::runner::run(
        store_arc.clone(),
        stats,
        config_path,
        config.listen_addr.to_string(),
        config.upstream_base_url.clone(),
        tui_rx,
    )
    .await;

    // TUI exited: signal shutdown and wait for the server to drain.
    drop(tui_tx);
    // Dropping `shutdown_tx` causes `shutdown_signal`'s
    // `watch::Receiver::changed()` future to resolve with
    // `Err(RecvError)`, which fires the `select!` branch that
    // `axum::serve(...).with_graceful_shutdown` is waiting on.
    // The server then drains in-flight requests and returns.
    drop(shutdown_tx);
    let _ = server_task.await;

    tui_result.context("TUI runtime error")?;
    Ok(())
}

fn build_upstream_client(config: &Config) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(config.request_timeout)
        .build()
        .context("build reqwest client")
}

/// Bridge between the `tokio::sync::mpsc` channel the TUI listens on
/// and the `std::sync::mpsc` channel that `OutputSink` uses.
///
/// `OutputSink` is intentionally built on `std::sync::mpsc` so a
/// request handler can `emit` from a synchronous `Drop` impl (the
/// streaming translator prints from `Drop`). A Tokio-aware variant
/// would force `emit` to be `async`, which is a non-starter for
/// `Drop`. This bridge keeps the public API ergonomic while
/// crossing the runtime boundary.
#[derive(Clone)]
struct TuiBridge {
    tx: mpsc::UnboundedSender<openai_to_anthropic_proxy::tui::LogLine>,
}

impl TuiBridge {
    fn new(tx: mpsc::UnboundedSender<openai_to_anthropic_proxy::tui::LogLine>) -> Self {
        Self { tx }
    }

    /// Build an `OutputSink` that forwards every `emit` call to the
    /// TUI's channel. We can't use `OutputSink::tui()` directly
    /// because that uses `std::sync::mpsc`, and we need a Tokio
    /// channel so the TUI loop can `select!` on it.
    fn into_sink(self) -> OutputSink {
        // OutputSink exposes only `emit(&self, ...)`, so we wrap the
        // bridge in a closure and use OutputSink::tui's receiver to
        // build a channel, then drain it on a background task that
        // forwards into the Tokio channel.
        //
        // In practice this is one extra hop per log line, which is
        // negligible compared to the network I/O of a real request.
        let (sink, std_rx) = OutputSink::tui();
        let tx = self.tx;
        std::thread::spawn(move || {
            while let Ok(line) = std_rx.recv() {
                // If the TUI has shut down, stop forwarding.
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        sink
    }
}

/// Initialize tracing. When `log_to_disk` is `true` (opt-in),
/// structured events go to a rotating file at
/// `target/logs/proxy.log`. When `false` (the default), events are
/// dropped — they reach neither the terminal nor a file, and only
/// the explicit `println!` / `eprintln!` lines in this binary are
/// visible to the operator.
fn init_tracing(log_to_disk: bool) -> WorkerGuard {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,openai_to_anthropic_proxy=debug"));

    if log_to_disk {
        let log_dir = std::path::Path::new("target").join("logs");
        let file_appender = tracing_appender::rolling::daily(&log_dir, "proxy.log");
        let (file_writer, guard) = tracing_appender::non_blocking(file_appender);

        let file_layer = fmt::layer()
            .with_writer(file_writer)
            .with_target(true)
            .with_ansi(false)
            .with_level(true);

        tracing_subscriber::registry()
            .with(filter)
            .with(file_layer)
            .init();

        guard
    } else {
        let (sink_writer, guard) = tracing_appender::non_blocking(std::io::sink());
        let sink_layer = fmt::layer()
            .with_writer(sink_writer)
            .with_target(true)
            .with_ansi(false)
            .with_level(true);
        tracing_subscriber::registry()
            .with(filter)
            .with(sink_layer)
            .init();
        guard
    }
}

/// Resolves when **any** of:
///   - the user hits Ctrl-C, sends SIGTERM (Unix), or sends a
///     console-close event (Windows);
///   - the `watch::Sender` held by `main()` is dropped, which
///     happens immediately after the TUI's `run()` returns.
///
/// The TUI installs its own Ctrl-C handler because raw mode
/// suppresses delivery of SIGINT to the process; Ctrl-C is delivered
/// to the TUI as a `KeyEvent` instead. Pressing `q` (or Ctrl-C)
/// inside the TUI causes the TUI loop to exit, which in turn causes
/// `main()` to drop `shutdown_tx`, which causes this future to
/// resolve — so the server shuts down without the operator having
/// to press Ctrl-C a second time.
async fn shutdown_signal(mut shutdown_rx: watch::Receiver<()>) {
    let ctrl_c = async {
        signal::ctrl_c().await.expect("install ctrl-c handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(windows)]
    let close = async {
        signal::windows::ctrl_close()
            .expect("install ctrl_close handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    #[cfg(not(windows))]
    let close = std::future::pending::<()>();

    tokio::select! {
        // Fires when `main()` drops `shutdown_tx` after the TUI exits.
        // `_` matches both `Ok(())` (sender sent a value) and
        // `Err(_)` (sender dropped without sending).
        _ = shutdown_rx.changed() => {},
        _ = ctrl_c => {},
        _ = terminate => {},
        _ = close => {},
    }
}

#[cfg(test)]
mod shutdown_signal_tests {
    //! Regression tests for the TUI-exit-triggered shutdown mechanism.
    //!
    //! The bug: pressing `q` in the TUI exited the TUI but left the
    //! axum server running, because the only thing that resolved
    //! `shutdown_signal()` was `signal::ctrl_c()`. The user had to
    //! press Ctrl-C a *second* time to actually stop the server.
    //!
    //! The fix wires `shutdown_signal()` to a `watch::Receiver`
    //! alongside the OS signal handlers; dropping the sender (which
    //! we do immediately after the TUI's `run()` returns) resolves
    //! the receiver and triggers the graceful-shutdown path.

    use std::time::Duration;

    /// Dropping the sender must cause `shutdown_signal`'s
    /// `watch::Receiver::changed()` branch to resolve. This pins
    /// the primitive we chose: a `watch::channel` whose sender we
    /// hold in `main()` and drop after the TUI exits. Tokio's
    /// `watch::Receiver::changed()` returns `Err(RecvError)` when
    /// the sender is dropped — the `_` pattern in `select!`
    /// matches both `Ok` and `Err`, so the branch fires and
    /// triggers graceful shutdown.
    #[tokio::test]
    async fn watch_changed_resolves_when_sender_dropped() {
        let (tx, mut rx) = tokio::sync::watch::channel(());
        drop(tx);
        let result = tokio::time::timeout(Duration::from_millis(200), rx.changed()).await;
        match result {
            Ok(Err(_)) => {} // expected: Err when sender is dropped
            Ok(Ok(())) => panic!("watch::changed() resolved to Ok(()) without a value change"),
            Err(_) => panic!(
                "watch::changed() did not resolve within 200ms — sender drop did not wake receiver"
            ),
        }
    }

    /// The actual mechanism: a `tokio::select!` racing a
    /// `watch::Receiver::changed()` against `tokio::time::sleep`
    /// must pick the receiver branch when the sender is dropped.
    /// This is the shape used in `shutdown_signal`.
    #[tokio::test]
    async fn select_picks_watch_branch_when_sender_dropped() {
        let (tx, mut rx) = tokio::sync::watch::channel(());
        drop(tx);
        let picked = tokio::select! {
            _ = rx.changed() => "watch",
            _ = tokio::time::sleep(Duration::from_secs(10)) => "sleep",
        };
        assert_eq!(
            picked, "watch",
            "select! must pick the dropped-watch branch"
        );
    }
}
