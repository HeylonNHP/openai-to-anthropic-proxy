// Library entrypoint: re-exports the modules used by `main.rs` and by tests.
pub mod config;
mod error;
pub mod stream;

// Modules filled in by follow-up tasks.
pub mod anthropic;
pub mod openai;
pub mod proxy;
mod translate;

pub use config::Config;
pub use error::AppError;
