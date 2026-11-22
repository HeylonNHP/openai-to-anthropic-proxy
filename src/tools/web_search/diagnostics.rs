//! What to record when a search fails or looks wrong, so a block can be
//! diagnosed (and the client hardened) from the log alone.
//!
//! A [`Report`] is the engine's side of one search: the status, protocol,
//! the headers that identify a challenge or a rate limit, the cookies it set,
//! how long it took, and the start of the page. It is built for EVERY search
//! (it is cheap) and used when something goes wrong:
//!
//! * a **block** (bot challenge or a rate-limiting status) is logged as one
//!   `warn` event carrying the report, the session (how many searches it has
//!   made, how many succeeded before this block, how long since the last block,
//!   the session's age, whether cookies were sent) and the request context;
//! * a page with **no results and no recognisable "no results" message**, which
//!   is what a silent soft-block or a markup change looks like, is logged too;
//! * the offending page is **saved** under `<state_dir>/blocked-pages/` (the
//!   newest [`MAX_SAVED_PAGES`] are kept) so the challenge or the new markup
//!   can be read after the fact.
//!
//! Nothing here changes what a search returns.

use std::io::Write;
use std::path::{Path, PathBuf};

use dom_query::Document;

use crate::tools::web_search::cookies;

/// The most characters of a page kept in a [`Report`] (and written to disk).
const MAX_BODY_CHARS: usize = 64 * 1024;
/// How much of the page's visible text goes in the log line.
const HEAD_CHARS: usize = 240;
/// Saved pages kept; older ones are deleted.
pub const MAX_SAVED_PAGES: usize = 5;
const SAVED_DIR: &str = "blocked-pages";

/// The engine's side of one search.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// HTTP status, absent when no response arrived.
    pub status: Option<u16>,
    /// `HTTP/1.1`, `HTTP/2.0`, …
    pub http_version: String,
    pub elapsed_ms: u64,
    pub content_type: Option<String>,
    pub server: Option<String>,
    pub retry_after: Option<String>,
    /// Names only: the values are session tokens.
    pub set_cookie_names: Vec<String>,
    /// The page's full length in bytes, before it was capped.
    pub body_bytes: usize,
    /// The page, capped at [`MAX_BODY_CHARS`].
    pub body: String,
    /// For a failure with no response: what kind (`timeout`, `connect`, …)
    /// and the error chain.
    pub transport: Option<String>,
}

impl Report {
    /// The report for a request that never got a response.
    #[must_use]
    pub fn from_transport_error(error: &reqwest::Error, elapsed_ms: u64) -> Self {
        let kind = if error.is_timeout() {
            "timeout"
        } else if error.is_connect() {
            "connect"
        } else if error.is_redirect() {
            "redirect"
        } else if error.is_body() {
            "body"
        } else if error.is_decode() {
            "decode"
        } else {
            "request"
        };
        let mut chain = vec![error.to_string()];
        let mut source = std::error::Error::source(error);
        while let Some(cause) = source {
            chain.push(cause.to_string());
            source = cause.source();
        }
        Self {
            elapsed_ms,
            transport: Some(format!("{kind}: {}", chain.join(": "))),
            ..Self::default()
        }
    }

    /// The report for a response whose headers have arrived.
    #[must_use]
    pub fn from_response(response: &reqwest::Response, elapsed_ms: u64) -> Self {
        let headers = response.headers();
        let text = |name: &str| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };
        let set_cookie_names = headers
            .get_all(reqwest::header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .filter_map(|c| c.split('=').next())
            .map(|n| n.trim().to_owned())
            .collect();
        Self {
            status: Some(response.status().as_u16()),
            http_version: format!("{:?}", response.version()),
            elapsed_ms,
            content_type: text("content-type"),
            server: text("server"),
            retry_after: text("retry-after"),
            set_cookie_names,
            ..Self::default()
        }
    }

    /// Record the page (and the time the whole exchange took).
    pub fn set_body(&mut self, body: &str, elapsed_ms: u64) {
        self.elapsed_ms = elapsed_ms;
        self.body_bytes = body.len();
        self.body = body.chars().take(MAX_BODY_CHARS).collect();
    }

    /// Which known page features are present. A challenge, a "no results"
    /// page and a normal results page each have their own.
    #[must_use]
    pub fn markers(&self) -> Vec<&'static str> {
        const MARKERS: &[(&str, &str)] = &[
            ("challenge-form", "challenge-form"),
            ("anomaly-modal", "anomaly-modal"),
            ("bots use DuckDuckGo", "bots-use-duckduckgo"),
            ("web-result", "web-result"),
            ("result__a", "result__a"),
            ("No results", "no-results-text"),
            ("no-results", "no-results-class"),
        ];
        MARKERS
            .iter()
            .filter(|(needle, _)| self.body.contains(needle))
            .map(|(_, label)| *label)
            .collect()
    }

    /// True if the page says there were no results (so an empty hit list is
    /// the truth, not a symptom).
    #[must_use]
    pub fn says_no_results(&self) -> bool {
        let markers = self.markers();
        markers.contains(&"no-results-text") || markers.contains(&"no-results-class")
    }

    /// The page's title and the start of its visible text, on one line.
    #[must_use]
    pub fn head(&self) -> String {
        if self.body.is_empty() {
            return String::new();
        }
        let doc = Document::from(self.body.as_str());
        let title = doc.select("title").text();
        let body = doc.select("body");
        // Scripts and styles are not what a person would read.
        body.select("script, style").remove();
        let text = body.text();
        let joined = format!("{} {}", title.trim(), text)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        joined.chars().take(HEAD_CHARS).collect()
    }
}

/// Where a search stood when it failed: the request and the session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Context {
    pub query: String,
    pub region: String,
    pub endpoint: String,
    /// Cookies the jar would send for this endpoint.
    pub cookies_sent: usize,
    pub session_age_secs: u64,
    /// Searches made through this session so far (this one included).
    pub searches: u64,
    /// Searches that succeeded since the last block or failure.
    pub ok_streak: u64,
    pub secs_since_last_ok: Option<u64>,
    pub secs_since_last_block: Option<u64>,
}

/// Log a failed search: one `warn` event with everything needed to diagnose it.
pub fn log_failure(
    ctx: &Context,
    blocked: bool,
    message: &str,
    report: Option<&Report>,
    saved: Option<&Path>,
) {
    let empty = Report::default();
    let r = report.unwrap_or(&empty);
    tracing::warn!(
        kind = if blocked { "blocked" } else { "failed" },
        error = message,
        query = %ctx.query,
        region = %ctx.region,
        endpoint = %ctx.endpoint,
        status = ?r.status,
        http_version = %r.http_version,
        elapsed_ms = r.elapsed_ms,
        content_type = ?r.content_type,
        server = ?r.server,
        retry_after = ?r.retry_after,
        set_cookies = ?r.set_cookie_names,
        body_bytes = r.body_bytes,
        markers = ?r.markers(),
        transport = ?r.transport,
        page_head = %r.head(),
        cookies_sent = ctx.cookies_sent,
        session_age_secs = ctx.session_age_secs,
        searches_in_session = ctx.searches,
        ok_before_this = ctx.ok_streak,
        secs_since_last_ok = ?ctx.secs_since_last_ok,
        secs_since_last_block = ?ctx.secs_since_last_block,
        saved_page = ?saved,
        "web_search search failed"
    );
}

/// Log a search that returned nothing and did not say why.
pub fn log_unexplained_empty(ctx: &Context, report: &Report, saved: Option<&Path>) {
    tracing::warn!(
        query = %ctx.query,
        region = %ctx.region,
        endpoint = %ctx.endpoint,
        status = ?report.status,
        http_version = %report.http_version,
        elapsed_ms = report.elapsed_ms,
        body_bytes = report.body_bytes,
        markers = ?report.markers(),
        page_head = %report.head(),
        cookies_sent = ctx.cookies_sent,
        searches_in_session = ctx.searches,
        saved_page = ?saved,
        "web_search search returned no results and no 'no results' message; this can be a soft block or changed markup"
    );
}

/// Save `report`'s page for later reading. `None` when there is no state
/// directory or nothing to save; a failure to write is logged, never raised.
pub fn save_page(state_dir: Option<&Path>, report: &Report) -> Option<PathBuf> {
    let dir = state_dir?.join(SAVED_DIR);
    if report.body.is_empty() {
        return None;
    }
    match write_page(&dir, &report.body, millis_now()) {
        Ok(path) => Some(path),
        Err(e) => {
            tracing::warn!(dir = %dir.display(), error = %e, "could not save the blocked web_search page");
            None
        }
    }
}

fn millis_now() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}

fn write_page(dir: &Path, body: &str, now_millis: u128) -> std::io::Result<PathBuf> {
    cookies::create_private_dir(dir)?;
    // Zero-padded so names sort in time order.
    let path = dir.join(format!("search-{now_millis:016}.html"));
    let mut file = cookies::open_private(&path)?;
    file.write_all(body.as_bytes())?;
    drop(file);
    prune(dir)?;
    Ok(path)
}

/// Delete all but the newest [`MAX_SAVED_PAGES`] saved pages.
fn prune(dir: &Path) -> std::io::Result<()> {
    let mut names: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(Result::ok)
        .map(|e| e.file_name())
        .filter(|n| n.to_string_lossy().starts_with("search-"))
        .collect();
    names.sort();
    let excess = names.len().saturating_sub(MAX_SAVED_PAGES);
    for name in names.into_iter().take(excess) {
        let _ = std::fs::remove_file(dir.join(name));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
