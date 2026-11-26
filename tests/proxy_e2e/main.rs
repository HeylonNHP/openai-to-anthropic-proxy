//! End-to-end test: drive the proxy with a real Anthropic-shaped
//! request, point it at a fake in-process OpenAI Responses-shaped
//! upstream, and verify the proxy translates the request correctly and
//! the response comes back in Anthropic shape.
//!
//! The streaming path is exercised the same way, with the fake
//! upstream returning a server-sent event stream in the Responses
//! event format.
//!
//! Layout: `harness.rs` holds the shared fake-upstream/proxy scaffolding
//! (no tests); each sibling module holds one topic's tests, named after the
//! `src/` module it exercises where one exists. `cargo test --test proxy_e2e
//! <module>::` filters by topic.

mod advisor;
mod basics;
mod config;
mod context_overflow;
mod count_tokens;
mod documents;
mod errors;
mod harness;
mod images;
mod instructions;
mod max_tokens;
mod model_aliases;
mod model_fallback;
mod retry_pipeline;
mod server_tool_options;
mod server_tools;
mod server_tools_streamed;
mod tool_repair;
mod translate;
mod upstream_gate;
mod web_fetch;
mod web_search;
