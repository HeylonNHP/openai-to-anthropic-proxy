// Library entrypoint: re-exports the modules used by `main.rs` and by tests.
pub mod capabilities;
pub mod config;
mod error;
pub mod responses;
pub mod stream;

pub mod anthropic;
pub mod proxy;
pub mod repair;
mod translate;
pub mod tui;

pub use capabilities::{CapabilityRegistry, CapabilityStore, RequestParam};
pub use config::Config;
pub use error::AppError;
pub use tui::{
    MappingsStore, OutputSink, RuntimeMappings, SessionStatsSnapshot, SessionStatsStore,
    TokenTotals, TokenUsage,
};
