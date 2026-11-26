//! Ids for Anthropic's server-tool blocks.
//!
//! Anthropic's Messages API constrains the `id` of a `server_tool_use`
//! block — and the `tool_use_id` of every server-tool result block that
//! pairs with it — to the `srvtoolu_` prefix followed by one or more
//! `[a-zA-Z0-9_]` characters.
//!
//! The API checks that shape on INBOUND history, not only on ids it
//! minted itself. An id the proxy makes up is therefore a liability: it
//! lands in the client's transcript, the client sends that transcript
//! back on every later turn, and if any later turn goes to Anthropic
//! rather than back to the proxy, the API rejects the whole request
//! with a 400 naming the offending block. A conversation recorded
//! through the proxy can then never be resumed against a real
//! Anthropic subscription.
//!
//! Every server-tool id the proxy mints comes from here, so the shape is
//! right by construction and no id repeats inside a conversation.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// The prefix Anthropic's API requires on a server-tool block id.
pub const SERVER_TOOL_ID_PREFIX: &str = "srvtoolu_";

/// The pattern Anthropic's Messages API enforces on a server-tool block
/// id, per the published request schema.
///
/// The API's runtime validator is slightly more permissive — it also
/// allows `-` — but the proxy mints only `[a-zA-Z0-9_]`, so its ids
/// satisfy both.
pub const SERVER_TOOL_ID_PATTERN: &str = "^srvtoolu_[a-zA-Z0-9_]+$";

/// Whether `id` is shaped like a server-tool id Anthropic will accept.
///
/// This checks the published schema's shape directly, without a regex:
/// the prefix, then a non-empty run of `[a-zA-Z0-9_]`.
#[must_use]
pub fn is_server_tool_id(id: &str) -> bool {
    let Some(suffix) = id.strip_prefix(SERVER_TOOL_ID_PREFIX) else {
        return false;
    };
    !suffix.is_empty()
        && suffix
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A fresh server-tool id, unique for the life of the process.
///
/// Nanoseconds since the epoch plus a process-local counter: no new
/// dependency, no `rand` crate. The counter keeps ids unique when two
/// calls land in the same nanosecond (or the clock is coarse), and the
/// timestamp keeps them unique across a restart.
///
/// The counter is shared by every server tool in the process, so no two
/// server-tool blocks — a `web_search` and an `advisor`, say — can ever
/// carry the same id.
#[must_use]
pub fn new_server_tool_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{SERVER_TOOL_ID_PREFIX}{nanos:x}_{n:x}")
}

#[cfg(test)]
mod tests;
