// Library entrypoint: re-exports the modules used by `main.rs` and by tests.
pub mod config;
mod error;
pub mod responses;
pub mod stream;

pub mod anthropic;
pub mod proxy;
mod translate;
pub mod tui;

pub use config::Config;
pub use error::AppError;
pub use tui::{
    MappingsStore, OutputSink, RuntimeMappings, SessionStatsSnapshot, SessionStatsStore,
    TokenTotals, TokenUsage,
};
