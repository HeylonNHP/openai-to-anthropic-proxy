//! What one served tool call did, in the terms the operator sees.
//!
//! A server tool's call is invisible in the request log: the client only
//! ever sees `server_tool_use` blocks, so a turn that fetched three pages
//! looks exactly like a turn that did nothing. This module is what makes the
//! call visible — the shared loop turns each served call into one row in
//! the RECENT REQUESTS pane:
//!
//! ```text
//!   web_fetch  |  https://example.com/docs  |  ok  |  1.24s  |  18.43k chars
//! ```
//!
//! | column | what it holds |
//! |---|---|
//! | tool | the client-facing tool name (`web_fetch`, `web_search`, `advisor`) |
//! | subject | what the call acted on: a URL, a query, a model |
//! | outcome | `ok`, `error`, `refused` or `partial` |
//! | elapsed | how long the call took |
//! | detail | the error code, or the size of what came back |
//!
//! The row is built from facts the tool already knows; the loop only adds
//! the elapsed time, because only the loop knows how long the whole call
//! took.

use std::time::Duration;

use crate::tui::format::DASH;

/// The longest subject shown before it is elided. A URL's tail says which
/// page, so the subject keeps both ends rather than only its head.
const MAX_SUBJECT: usize = 48;

/// The longest detail shown before it is cut.
const MAX_DETAIL: usize = 40;

/// What one served tool call did, in the terms the operator sees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CallSummary {
    /// The client-facing tool name (`web_fetch`, `web_search`, `advisor`).
    pub tool: &'static str,
    /// What the call acted on: a URL, a query, a model.
    pub subject: String,
    /// How it ended: one of the `OUTCOME_*` words.
    pub outcome: &'static str,
    /// The error code, or the size of what came back.
    pub detail: Option<String>,
}

impl CallSummary {
    /// A summary of a call, before its detail is known.
    ///
    /// The subject is put on one line and elided here, so every tool's row
    /// is the same shape whatever the model passed in: a query with a
    /// newline in it cannot break the row's columns.
    pub(crate) fn new(
        tool: &'static str,
        subject: impl Into<String>,
        outcome: &'static str,
    ) -> Self {
        Self {
            tool,
            subject: elide(&one_line(&subject.into())),
            outcome,
            detail: None,
        }
    }

    /// Add the detail: the error code, or the size of what came back.
    #[must_use]
    pub(crate) fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(cut(&one_line(&detail.into()), MAX_DETAIL));
        self
    }

    /// The row as it appears in the request log.
    pub(crate) fn row(&self, elapsed: Duration) -> String {
        format!(
            "  {}  |  {}  |  {}  |  {:.2}s  |  {}",
            self.tool,
            self.subject,
            self.outcome,
            elapsed.as_secs_f64(),
            self.detail.as_deref().unwrap_or(DASH),
        )
    }
}

/// Collapse every run of whitespace into a single space: a row is one line,
/// and a row is split on its column separator.
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Cut a subject to [`MAX_SUBJECT`] characters, keeping BOTH ends: the head
/// says which host, the tail says which page.
fn elide(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= MAX_SUBJECT {
        return s.to_owned();
    }
    let head = MAX_SUBJECT / 2 - 1;
    let tail = MAX_SUBJECT - head - 1;
    let mut out: String = chars[..head].iter().collect();
    out.push('…');
    out.extend(chars[chars.len() - tail..].iter());
    out
}

/// Cut a detail to `max` characters, keeping the head.
fn cut(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests;
