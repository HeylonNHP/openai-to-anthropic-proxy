//! Proxy server entrypoint.
//!
//! Loads configuration, builds a shared reqwest client and an axum router,
//! then binds the configured listen address. Shuts down on `Ctrl-C`.

use std::sync::Arc;

use anyhow::{Context, Result};
use openai_to_anthropic_proxy::Config;
use tokio::net::TcpListener;
use tokio::signal;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

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

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,openai_to_anthropic_proxy=debug"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

/// Resolves when the user hits Ctrl-C, or (on Unix) sends SIGTERM.
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

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("received Ctrl-C, shutting down"),
        _ = terminate => tracing::info!("received SIGTERM, shutting down"),
    }
}
