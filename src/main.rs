//! Proxy server entrypoint.
//!
//! Loads configuration, builds a shared reqwest client and an axum router,
//! then binds the configured listen address. Shuts down on `Ctrl-C`.
//!
//! The terminal shows only the explicit `println!` / `eprintln!` lines
//! in this binary (startup banner, per-request summary, shutdown notice).
//! `tracing` events are **silent by default** — they reach neither the
//! terminal nor a file. Set `log_to_disk = true` in `proxy.toml` (or
//! `LOG_TO_DISK=1` in the env) to capture them in a rotating file at
//! `target/logs/proxy.log` for postmortem inspection.

use std::sync::Arc;

use anyhow::{Context, Result};
use openai_to_anthropic_proxy::Config;
use tokio::net::TcpListener;
use tokio::signal;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::load().context("load configuration")?;

    // Initialize tracing after we know whether file logging is
    // enabled. When `log_to_disk` is off (the default), tracing
    // events are dropped — no output, no file. When on, they go
    // to `target/logs/proxy.log`.
    let _log_guard = init_tracing(config.log_to_disk);

    // Warn loudly if the proxy is reachable but unauthenticated.
    // The `eprintln!` is the only way this surfaces to the operator
    // under the default (`log_to_disk = false`), since the matching
    // `tracing::warn!` below is dropped.
    if config.proxy_key.is_none() {
        eprintln!(
            "WARNING: proxy_key is not set; /v1/messages accepts requests from any client. \
             Set `proxy_key` in proxy.toml or PROXY_KEY env to require authentication."
        );
        tracing::warn!(
            "proxy_key is not set; /v1/messages accepts requests from any client"
        );
    }

    let client = build_upstream_client(&config)?;
    let app = openai_to_anthropic_proxy::proxy::router(Arc::new(config.clone()), client);

    let listener = TcpListener::bind(config.listen_addr)
        .await
        .with_context(|| format!("bind {}", config.listen_addr))?;

    println!(
        "Proxy listening on {} → {}{}",
        config.listen_addr,
        config.upstream_base_url.trim_end_matches('/'),
        config.upstream_path
    );

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

/// Initialize tracing. When `log_to_disk` is `true` (opt-in),
/// structured events go to a rotating file at
/// `target/logs/proxy.log`. When `false` (the default), events are
/// dropped — they reach neither the terminal nor a file, and only
/// the explicit `println!` / `eprintln!` lines in this binary are
/// visible to the operator.
///
/// Returns a `WorkerGuard` that must be kept alive for the lifetime
/// of the program; dropping it flushes and stops the background log
/// writer. The guard is meaningful only for the file path; the
/// silent path drops it harmlessly.
fn init_tracing(log_to_disk: bool) -> WorkerGuard {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,openai_to_anthropic_proxy=debug"));

    if log_to_disk {
        let log_dir = std::path::Path::new("target").join("logs");
        let file_appender = tracing_appender::rolling::daily(&log_dir, "proxy.log");
        let (file_writer, guard) = tracing_appender::non_blocking(file_appender);

        // File layer — plain text, no colors (easier to grep). One
        // line per record.
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
        // Default: silent. The terminal shows only the explicit
        // `println!` / `eprintln!` lines in this binary. We still
        // pipe through `tracing_appender::non_blocking(io::sink())`
        // (rather than a custom `MakeWriter`) so the return type
        // stays `WorkerGuard` with no signature change at the call
        // site — the writes are dropped at the kernel pipe.
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
