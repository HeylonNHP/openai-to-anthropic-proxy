//! Anthropic server-side tool implementations.
//!
//! Each sub-module implements one server-side tool: Anthropic's server
//! tools have no `input_schema` and are executed by the API, so this crate
//! (standing in for the server) has to do the work itself.
//!
//! - `domains` — the allow/block lists a client sets on a search or fetch tool.
//! - `server_loop` — the loop shared by every proxy-served server tool.
//! - `advisor` — the experimental second-opinion consultation.
//! - `web_fetch` — fetch an http/https URL and hand the readable text back
//!   to the model.
//! - `web_search` — optional proxy-native web search (DuckDuckGo scraping),
//!   used instead of the upstream's built-in search when enabled. Self
//!   contained and removable: see `web_search/README.md`.
//!
//! These are re-exported from the crate root under their original flat names
//! (`advisor`, `web_fetch`) so the existing call sites and docs keep
//! working; the `tools::` namespace is the canonical home from here on.

pub mod advisor;
pub mod domains;
pub mod server_loop;
pub mod web_fetch;
pub mod web_search;
