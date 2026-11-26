// Library entrypoint: re-exports the modules used by `main.rs` and by tests.
mod tools;
pub use tools::advisor;
pub use tools::web_fetch;
pub mod capabilities;
pub mod config;
mod context_overflow;
mod count_tokens;
mod error;
pub mod responses;
pub mod server_tool_id;
pub mod stream;

pub mod anthropic;
mod model_rejection;
pub mod proxy;
pub mod repair;
mod schema_refs;
mod translate;
pub mod tui;
mod upstream_gate;

pub use capabilities::{CapabilityRegistry, CapabilityStore, RequestParam};
pub use config::{Config, UNBOUNDED_ADVISOR_LOOP_WARNING};
pub use error::AppError;
pub use tui::{
    MappingsStore, OutputSink, RuntimeMappings, SessionStatsSnapshot, SessionStatsStore,
    TokenTotals, TokenUsage,
};
