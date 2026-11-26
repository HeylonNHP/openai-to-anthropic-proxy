//! Pure builders for the `web_fetch` wire protocol.
//!
//! Every function here returns a [`serde_json::Value`] and performs no
//! I/O. The blocks are carried through the shared wire types as
//! [`crate::anthropic::ResponseContentBlock::Raw`], so no per-feature
//! enum variant is needed (the same trick the advisor uses).
//!
//! ## The shape
//!
//! A successful fetch produces TWO blocks that pair up by id:
//!
//! ```json
//! {
//!   "role": "assistant",
//!   "content": [
//!     {
//!       "type": "server_tool_use",
//!       "id": "srvtoolu_01234567890abcdef",
//!       "name": "web_fetch",
//!       "input": { "url": "https://example.com/article" }
//!     },
//!     {
//!       "type": "web_fetch_tool_result",
//!       "tool_use_id": "srvtoolu_01234567890abcdef",
//!       "content": {
//!         "type": "web_fetch_result",
//!         "url": "https://example.com/article",
//!         "content": {
//!           "type": "document",
//!           "source": {
//!             "type": "text",
//!             "media_type": "text/plain",
//!             "data": "Full text content of the article..."
//!           },
//!           "title": "Article Title"
//!         },
//!         "retrieved_at": "2025-08-25T10:30:00Z"
//!       }
//!     }
//!   ]
//! }
//! ```
//!
//! A FAILED fetch produces the same pair, with an error result:
//!
//! ```json
//! {
//!   "type": "web_fetch_tool_result",
//!   "tool_use_id": "srvtoolu_01234567890abcdef",
//!   "content": { "type": "web_fetch_tool_result_error", "error_code": "url_not_accessible" }
//! }
//! ```
//!
//! The error codes are Anthropic's own enum. The proxy emits:
//!
//! | code | when |
//! |---|---|
//! | `invalid_tool_input` | the URL was malformed, used a bad scheme, or carried credentials |
//! | `url_not_allowed` | the URL resolved to a private or reserved address |
//! | `url_not_accessible` | DNS failure, connection failure, timeout, or a non-2xx response |
//! | `unavailable` | the fetch could not be performed for another reason |

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

/// The error codes Anthropic's `web_fetch` result can carry.
pub const ERROR_CODE_INVALID_TOOL_INPUT: &str = "invalid_tool_input";
pub const ERROR_CODE_URL_NOT_ALLOWED: &str = "url_not_allowed";
pub const ERROR_CODE_URL_NOT_ACCESSIBLE: &str = "url_not_accessible";
pub const ERROR_CODE_UNAVAILABLE: &str = "unavailable";
pub const ERROR_CODE_MAX_USES_EXCEEDED: &str = "max_uses_exceeded";

/// The `server_tool_use` block that opens one fetch.
///
/// `name` must be exactly `"web_fetch"` — the client matches on that
/// string.
#[must_use]
pub fn server_tool_use(id: &str, url: &str) -> Value {
    json!({
        "type": "server_tool_use",
        "id": id,
        "name": "web_fetch",
        "input": { "url": url },
    })
}

/// A successful `web_fetch_tool_result` block.
///
/// `tool_use_id` must equal the `id` on the preceding
/// [`server_tool_use`] block.
#[must_use]
pub fn web_fetch_result(id: &str, url: &str, title: Option<&str>, text: &str) -> Value {
    let mut document = json!({
        "type": "document",
        "source": {
            "type": "text",
            "media_type": "text/plain",
            "data": text,
        },
    });
    if let Some(title) = title {
        document["title"] = Value::String(title.to_owned());
    }
    json!({
        "type": "web_fetch_tool_result",
        "tool_use_id": id,
        "content": {
            "type": "web_fetch_result",
            "url": url,
            "content": document,
            "retrieved_at": now_iso8601(),
        },
    })
}

/// A failed `web_fetch_tool_result` block.
///
/// The error arrives inside a normal HTTP 200 response, so the turn
/// continues — the client sees the failure as tool output, not as a
/// transport failure.
#[must_use]
pub fn error_result(id: &str, error_code: &str) -> Value {
    json!({
        "type": "web_fetch_tool_result",
        "tool_use_id": id,
        "content": {
            "type": "web_fetch_tool_result_error",
            "error_code": error_code,
        },
    })
}

/// Collision-resistant id for one fetch.
///
/// `srvtoolu_` is the real prefix Anthropic uses for server tools, and
/// the id must match `^srvtoolu_[a-zA-Z0-9_]+$` — the API validates a
/// `server_tool_use` id on INBOUND history, so one that does not match
/// would be rejected when the client replays the conversation against
/// Anthropic itself.
///
/// Delegates to the shared generator, so every server-tool id in the
/// process comes from one counter and cannot collide.
#[must_use]
pub fn new_id() -> String {
    crate::server_tool_id::new_server_tool_id()
}

/// The current time as an ISO-8601 timestamp.
///
/// Anthropic's `retrieved_at` is an ISO-8601 string. This produces the
/// `YYYY-MM-DDTHH:MM:SSZ` shape without pulling in a date-time crate:
/// the conversion from the Unix epoch is done by hand.
#[must_use]
pub fn now_iso8601() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_epoch(secs)
}

/// Format a Unix timestamp as `YYYY-MM-DDTHH:MM:SSZ`.
fn format_epoch(secs: u64) -> String {
    // Days since the epoch -> civil date (Howard Hinnant's algorithm).
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!("{y:04}-{m:02}-{d:02}T{hour:02}:{minute:02}:{second:02}Z")
}

#[cfg(test)]
mod tests;
