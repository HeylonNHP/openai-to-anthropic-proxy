//! Turning a fetched body into readable text.
//!
//! Two extractors, in order:
//!
//! 1. **`dom_smoothie`** — a Readability (readability.js) port. It finds
//!    the article body of a page and drops the navigation, the footer and
//!    the other chrome. This is the extractor for HTML.
//! 2. **`fast_html2md`** — a full-page HTML-to-Markdown converter. Used as
//!    the FALLBACK when Readability cannot find an article (a docs
//!    landing page, a listing page, an app shell).
//!
//! A body that is not HTML at all (plain text, JSON, …) is returned
//! as-is: there is nothing to "read" out of it.
//!
//! The extraction is deliberately lossy in one direction only: it never
//! invents content, it only removes chrome.

use dom_query::Document;
use dom_smoothie::{Config as ReadabilityConfig, Readability, TextMode};

/// The maximum number of DOM elements `dom_smoothie` will parse.
///
/// A pathological document (a million nested `<div>`s) would otherwise
/// burn unbounded CPU inside the parser. This is a parse bound, not a
/// content bound: the operator's `max_content_bytes` still governs what
/// is handed to the model.
const MAX_ELEMENTS: usize = 200_000;

/// Extract the readable text (and title) from a fetched body.
///
/// `content_type` is the response's `Content-Type` header, when the
/// server sent one. It is used only to decide whether the body is worth
/// running through the HTML extractors at all.
#[must_use]
pub fn extract(
    bytes: &[u8],
    page_url: &str,
    content_type: Option<&str>,
) -> (Option<String>, String) {
    let text: String = std::str::from_utf8(bytes).map_or_else(
        |_| {
            // Not valid UTF-8. The bytes may still be a UTF-16 or
            // latin-1 page; decode lossily so the model gets
            // *something* rather than nothing.
            String::from_utf8_lossy(bytes).into_owned()
        },
        str::to_owned,
    );

    if !looks_like_html(&text, content_type) {
        return (None, text);
    }

    let (title, extracted) = readability(&text, page_url);
    if extracted.trim().is_empty() {
        // Readability found no article: fall back to the whole page. The
        // title is still reported — it comes from the document head, not
        // from the article.
        let md = html2md::rewrite_html(&text, false);
        return (title.or_else(|| document_title(&text)), md);
    }
    (title.or_else(|| document_title(&text)), extracted)
}

/// The `<title>` of the document, if it has one.
///
/// Extracted independently of Readability: a page can have a perfectly
/// good title and no article body (a docs landing page, a search
/// results page). Reporting the title in that case is useful; dropping
/// it is not.
fn document_title(html: &str) -> Option<String> {
    let doc = Document::from(html);
    let title = doc
        .select("title")
        .nodes()
        .first()
        .map(|n| n.text().trim().to_owned())
        .unwrap_or_default();
    if title.is_empty() { None } else { Some(title) }
}

/// Whether the body should go through the HTML extractors.
fn looks_like_html(text: &str, content_type: Option<&str>) -> bool {
    if let Some(ct) = content_type {
        let ct = ct.to_ascii_lowercase();
        if ct.contains("html") || ct.contains("xml") {
            return true;
        }
        // A declared non-HTML type is trusted: do not run the HTML
        // extractors over it.
        if !ct.is_empty() && ct != "text/plain" {
            return false;
        }
    }
    // No usable content type: sniff the body. The comparison is
    // CASE-INSENSITIVE — a real page starts with `<!DOCTYPE html>`.
    let head = text.trim_start().to_ascii_lowercase();
    head.starts_with("<!doctype html")
        || head.starts_with("<html")
        || head.starts_with("<head")
        || head.starts_with("<body")
}

/// Run Readability over the page.
///
/// Returns `(title, markdown_text)`.
///
/// Note that Readability's own `is_probably_readable` pre-filter is NOT
/// used: it rejects perfectly good short articles (a two-paragraph post
/// scores below its length threshold) even though `parse()` extracts
/// them correctly. It is a heuristic for "is this page mostly article?",
/// not a precondition for extraction, so using it as a gate loses real
/// content.
///
/// An empty text means "no article found" and the caller falls back to
/// the whole page.
fn readability(html: &str, page_url: &str) -> (Option<String>, String) {
    let cfg = ReadabilityConfig {
        text_mode: TextMode::Markdown,
        max_elements_to_parse: MAX_ELEMENTS,
        ..ReadabilityConfig::default()
    };

    // Readability needs an ABSOLUTE document URL to resolve relative
    // links; without one it refuses to run.
    let Ok(mut reader) = Readability::new(html, Some(page_url), Some(cfg)) else {
        return (None, String::new());
    };
    match reader.parse() {
        Ok(article) => {
            let title = if article.title.trim().is_empty() {
                None
            } else {
                Some(article.title.to_string())
            };
            (title, article.text_content.to_string())
        }
        Err(_) => (None, String::new()),
    }
}

#[cfg(test)]
mod tests;
