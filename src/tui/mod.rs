//! Terminal UI for runtime model-routing changes.
//!
//! Public surface:
//!
//! - [`runtime::MappingsStore`]: the lock-free snapshot store the
//!   request handler reads and the TUI writes.
//! - [`output::OutputSink`]: per-process log sink that routes
//!   request lines into the TUI or, when the TUI is disabled, to
//!   stdout.
//! - [`runner::run`]: blocks (async) until the user quits.
//!
//! All other types are internal to the module.

pub mod app;
pub mod output;
pub mod panels;
pub mod runner;
pub mod runtime;

pub use output::{LogKind, LogLine, OutputSink};
pub use runtime::{MappingsSnapshot, MappingsStore, RuntimeMappings};
