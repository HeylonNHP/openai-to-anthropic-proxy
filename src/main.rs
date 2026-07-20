//! Proxy server entrypoint.
//!
//! Loads configuration, builds a shared reqwest client and an axum router,
//! then binds the configured listen address. Shuts down on `Ctrl-C`.
//!
//! Logs go to stderr *and* a rotating file under `target/logs/proxy.log`,
//! so the agent (and an operator tailing the log) can inspect what the
//! proxy sent upstream and what the upstream returned.

use std::sync::Arc;

use anyhow::{Context, Result};
use openai_to_anthropic_proxy::Config;
use tokio::net::TcpListener;
use tokio::signal;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

#[tokio::main]
async fn main() -> Result<()> {
    let _log_guard = init_tracing();

    let config = Config::load().context("load configuration")?;
    tracing::info!(?config.listen_addr, %config.upstream_base_url, "starting proxy");

    let client = build_upstream_client(&config)?;
    let app = openai_to_anthropic_proxy::proxy::router(Arc::new(config.clone()), client);

    let listener = TcpListener::bind(config.listen_addr)
        .await
        .with_context(|| format!("bind {}", config.listen_addr))?;
    tracing::info!("listening on {}", config.listen_addr);

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("axum server error")?;

    Ok(())
}

fn build_upstream_client(config: &Config) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(config.request_timeout)
        .build()
        .context("build reqwest client")
}

/// Initialize tracing with two sinks: stderr (so the operator sees it
/// in the terminal) and `target/logs/proxy.log` (so the agent — or a
/// log-tailing operator — can read it after the fact).
///
/// Returns a `WorkerGuard` that must be kept alive for the lifetime of
/// the program; dropping it flushes and stops the background log writer.
fn init_tracing() -> WorkerGuard {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,openai_to_anthropic_proxy=debug"));

    let log_dir = std::path::Path::new("target").join("logs");
    let file_appender = tracing_appender::rolling::daily(&log_dir, "proxy.log");
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);

    // Stderr layer — colored, human-readable, what you see in the terminal.
    // Filters out the `upstream_payload` target so the full request body
    // (which can contain the user's prompt) is never printed to the terminal.
    // That target still reaches the file layer for postmortem debugging.
    let stderr_layer = fmt::layer()
        .with_writer(std::io::stderr)
        .with_target(true)
        .with_ansi(true)
        .with_filter(tracing_subscriber::filter::filter_fn(|meta| {
            meta.target() != "upstream_payload"
        }));

    // File layer — plain text, no colors (easier to grep). One line per record.
    let file_layer = fmt::layer()
        .with_writer(file_writer)
        .with_target(true)
        .with_ansi(false)
        .with_level(true);

    tracing_subscriber::registry()
        .with(filter)
        .with(stderr_layer)
        .with(file_layer)
        .init();

    guard
}

/// Resolves when the user hits Ctrl-C, sends SIGTERM (Unix), or sends
/// a console close event (Windows).
async fn shutdown_signal() {
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

    // Windows: `taskkill /pid <pid>` (without /f) sends a console close
    // event to a console-attached process. Listening for it gives a
    // graceful-shutdown path on Windows. Note this still doesn't help
    // if the process was started detached (no console); the operator
    // can use `taskkill /pid <pid>` (no /f) for a console-attached run
    // or hit Ctrl-C in the terminal where it was launched. `taskkill /f`
    // is uncatchable by design — there's no way to drain in-flight
    // requests in that case.
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
        _ = ctrl_c => tracing::info!("received Ctrl-C, shutting down"),
        _ = terminate => tracing::info!("received SIGTERM, shutting down"),
        _ = close => tracing::info!("received console close, shutting down"),
    }
}
