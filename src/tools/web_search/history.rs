//! Keeping search results in the model's context on later turns.
//!
//! Anthropic puts each result's page content in an opaque
//! `encrypted_content` field; the client sends the whole assistant message
//! back on the next turn, and the model sees the content again. The proxy
//! does the same, with a format of its own:
//!
//! ```text
//! encrypted_content = "oap1:" + base64(the text the model read for this result)
//! ```
//!
//! It is not encrypted: the client is the user, so there is nothing to hide
//! from them. It is only opaque, which is all Anthropic's wire format asks.
//!
//! A result the proxy did not make (the upstream's own built-in search, or
//! another server's) carries a blob that cannot be read. Its title and URL are
//! still replayed, which is far better than forgetting that the search
//! happened.
//!
//! ## Bounded
//!
//! Replaying costs context on every later turn, so each result is capped at
//! [`MAX_REPLAY_CHARS`] when it is encoded. (A page is already bounded by the
//! compaction threshold; this is the backstop for a large threshold.)

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::Value;

/// Marks a blob this module wrote.
const MAGIC: &str = "oap1:";

/// Most characters of one result carried into later turns.
pub const MAX_REPLAY_CHARS: usize = 12_000;

/// Delimiters that tell the model where replayed content starts and ends,
/// so it is not mistaken for the assistant's own words (or for instructions).
const PREFIX: &str = "[Web search results retrieved earlier";
const SUFFIX: &str = "]\n";

/// Pack the text the model read for one result into an `encrypted_content`.
#[must_use]
pub fn encode(text: &str) -> String {
    let text = cap(text);
    format!("{MAGIC}{}", STANDARD.encode(text.as_bytes()))
}

/// Unpack a blob [`encode`] wrote. `None` for anything else, including a
/// blob that claims to be ours but is not valid.
#[must_use]
pub fn decode(encrypted_content: &str) -> Option<String> {
    let payload = encrypted_content.strip_prefix(MAGIC)?;
    let bytes = STANDARD.decode(payload).ok()?;
    String::from_utf8(bytes).ok()
}

/// The text to put in the assistant turn for an echoed `web_search_tool_result`
/// whose `content` is `content`. Empty when there is nothing to replay: an
/// error result, or a search that found nothing.
#[must_use]
pub fn replay_text(query: Option<&str>, content: &Value) -> String {
    let Some(results) = content.as_array() else {
        // `web_search_tool_result_error`: the search failed, nothing was seen.
        return String::new();
    };
    let parts: Vec<String> = results.iter().filter_map(replay_one).collect();
    if parts.is_empty() {
        return String::new();
    }
    let mut out = String::from("\n");
    out.push_str(PREFIX);
    if let Some(query) = query.filter(|q| !q.trim().is_empty()) {
        out.push_str(&format!(" for the query \"{query}\""));
    }
    out.push_str(":\n");
    out.push_str(&parts.join("\n"));
    out.push('\n');
    out.push_str(SUFFIX);
    out
}

/// One result: the full text if the proxy wrote it, else its title and URL.
fn replay_one(result: &Value) -> Option<String> {
    if let Some(text) = result
        .get("encrypted_content")
        .and_then(Value::as_str)
        .and_then(decode)
    {
        return Some(text);
    }
    let url = result.get("url").and_then(Value::as_str)?;
    let title = result.get("title").and_then(Value::as_str).unwrap_or("");
    Some(if title.is_empty() {
        format!("- {url}")
    } else {
        format!("- {title} — {url}")
    })
}

/// `text` limited to [`MAX_REPLAY_CHARS`], with a note when cut.
fn cap(text: &str) -> String {
    if text.chars().count() <= MAX_REPLAY_CHARS {
        return text.to_owned();
    }
    let cut: String = text.chars().take(MAX_REPLAY_CHARS).collect();
    format!("{cut}\n[content truncated]")
}

#[cfg(test)]
mod tests;
