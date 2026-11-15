//! Advisor feature — pure-logic skeleton (phase 2; **not wired in**).
//!
//! The advisor is Anthropic's server-side second opinion. It is triggered
//! by an executor `function_call{name:"advisor"}` and answered by a
//! separate upstream call whose advice is injected back into the executor's
//! own prompt. This module will own that protocol once it is integrated.
//!
//! Public surface (all pure functions, no I/O):
//!
//! - [`protocol::server_tool_use`], [`protocol::advisor_result`],
//!   [`protocol::unavailable`] — the content blocks Claude Code expects.
//! - [`protocol::new_id`] — unique `srvtoolu_…` ids for pairing blocks.
//! - [`protocol::advisor_iteration`], [`protocol::message_iteration`] —
//!   `usage.iterations[]` billing entries.
//! - [`sse::message_to_events`] — turn one buffered [`crate::anthropic::Message`]
//!   into a synthetic Anthropic SSE event sequence.
//!
//! ## Guarantees for this phase
//!
//! - **Off by default, completely unreachable.** [`crate::config::AdvisorConfig`]
//!   defaults to `enabled: false`. Nothing in `handle_messages`, `Attempt`,
//!   or `translate.rs` references `advisor::` yet — this module is only
//!   exercised by its own in-file unit tests. No behavior change.
//! - **Fail closed.** When the advisor call fails for any reason (404,
//!   timeout, bad response, or any internal error), the executor's answer
//!   must still be delivered, with `error_code: "unavailable"` on the
//!   result block. Never propagate a fatal error, never emit a bare
//!   `pause_turn`, and never burn the executor's `default_model` fallback
//!   on an advisor request (that would turn the second opinion into a
//!   self-review and corrupt stats attribution).
//! - **One assistant message.** `server_tool_use` and its matching
//!   `advisor_tool_result` are contiguous blocks in the *same* `content[]`,
//!   with equal, non-empty ids, optional executor text on either side, and
//!   `stop_reason: "end_turn"`. Streaming is one `message_start` … one
//!   `message_stop`; the advisor sub-inference does not stream, and its
//!   result arrives whole in a single `content_block_start` with no delta.
//!
//! Integration (the tool declaration, the upstream call, advice injection,
//! `usage.iterations[]`, and inbound echo) comes in later phases. See
//! `ADVISOR_TOOL_RESEARCH.md` §§3, 7, 11, 12, 17.
//!
//! This skeleton is deliberately unreferenced by production code until
//! integration; allow dead_code here rather than sprinkling `#[allow]` on
//! each helper.
#![allow(dead_code, unused_imports)]

// `mod advisor;` is private (like `mod model_rejection;`), so plain `pub`
// here is already crate-invisible; `pub(crate)` would only earn a clippy
// `redundant_pub_crate` warning.
pub mod protocol;
pub mod sse;

// Re-export the public surface for the (later) integration phase. Until
// then nothing outside this module references them; the module-level
// `unused_imports` allow above keeps the clippy warning count at baseline.
pub use protocol::{
    ADVISOR_ERROR_CODES, ERROR_CODE_EXECUTION_TIME_EXCEEDED, ERROR_CODE_MAX_USES_EXCEEDED,
    ERROR_CODE_MODEL_NOT_FOUND, ERROR_CODE_OVERLOADED, ERROR_CODE_PROMPT_TOO_LONG,
    ERROR_CODE_TOO_MANY_REQUESTS, ERROR_CODE_UNAVAILABLE, advisor_iteration, advisor_result,
    message_iteration, new_id, server_tool_use, unavailable,
};
pub use sse::message_to_events;
