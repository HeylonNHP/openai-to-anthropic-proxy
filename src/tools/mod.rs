//! Anthropic server-side tool implementations.
//!
//! Each sub-module implements one server-side tool: Anthropic's server
//! tools have no `input_schema` and are executed by the API, so this crate
//! (standing in for the server) has to do the work itself.
//!
//! - `advisor` — the experimental second-opinion consultation.
//! - `web_fetch` — fetch an http/https URL and hand the readable text back
//!   to the model.
//!
//! These are re-exported from the crate root under their original flat names
//! (`advisor`, `web_fetch`) so the existing call sites and docs keep
//! working; the `tools::` namespace is the canonical home from here on.

pub mod advisor;
pub mod web_fetch;
