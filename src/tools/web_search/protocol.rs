//! Pure builders for the `web_search` wire blocks the CLIENT sees.
//!
//! Each query becomes a `server_tool_use` / `web_search_tool_result` pair,
//! exactly the shape Anthropic's own server-side search produces, so the
//! client's rendering needs no special case. The blocks travel as
//! [`crate::anthropic::ResponseContentBlock::Raw`].

use serde_json::{Value, json};

use crate::tools::web_search::run::Entry;
use crate::tools::web_search::{format, history};

/// Anthropic's error codes for a failed `web_search_tool_result`.
pub const ERROR_INVALID_INPUT: &str = "invalid_input";
pub const ERROR_MAX_USES_EXCEEDED: &str = "max_uses_exceeded";
pub const ERROR_UNAVAILABLE: &str = "unavailable";

/// The `server_tool_use` block that opens one query. `name` must be exactly
/// `"web_search"` — the client matches on it.
#[must_use]
pub fn server_tool_use(id: &str, query: &str) -> Value {
    json!({
        "type": "server_tool_use",
        "id": id,
        "name": "web_search",
        "input": { "query": query },
    })
}

/// A successful `web_search_tool_result`: one result per entry (title + URL).
///
/// The page text itself goes to the executor, not the client, but each result
/// also carries it in `encrypted_content`, so that the client sending the
/// message back on the next turn puts it back in front of the model (see
/// [`crate::tools::web_search::history`]).
#[must_use]
pub fn search_result<'a>(id: &str, entries: impl IntoIterator<Item = &'a Entry>) -> Value {
    let content: Vec<Value> = entries
        .into_iter()
        .map(|entry| {
            json!({
                "type": "web_search_result",
                "url": entry.hit.url,
                "title": entry.hit.title,
                "page_age": null,
                "encrypted_content": history::encode(&format::replay_text(entry)),
            })
        })
        .collect();
    json!({
        "type": "web_search_tool_result",
        "tool_use_id": id,
        "content": content,
    })
}

/// A failed `web_search_tool_result`. It arrives inside a normal HTTP 200
/// response, so the turn continues.
#[must_use]
pub fn error_result(id: &str, error_code: &str) -> Value {
    json!({
        "type": "web_search_tool_result",
        "tool_use_id": id,
        "content": {
            "type": "web_search_tool_result_error",
            "error_code": error_code,
        },
    })
}

#[cfg(test)]
mod tests;
