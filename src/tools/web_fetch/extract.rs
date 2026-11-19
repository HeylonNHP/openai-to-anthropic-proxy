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
/// server sent one. It decides whether the body is worth running through
/// the HTML extractors, and its `charset` decides how the bytes are
/// decoded (see [`decode`]). Only call this for a body that is text:
/// [`content_kind`] and [`looks_binary`] say which bodies are not.
#[must_use]
pub fn extract(
    bytes: &[u8],
    page_url: &str,
    content_type: Option<&str>,
) -> (Option<String>, String) {
    let text = decode(bytes, content_type);

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

/// Whether a response's declared `Content-Type` is text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentKind {
    /// Text the model can read: HTML, XML, JSON, plain text, source code…
    Text,
    /// Something that is not text: a PDF, an image, an archive, audio…
    NonText,
    /// No usable declaration (no header, or the catch-all
    /// `application/octet-stream`, which servers send for text files too):
    /// the body itself has to be looked at ([`looks_binary`]).
    Unknown,
}

/// The `type/subtype` of a `Content-Type`, lowercased, without parameters.
fn essence(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

/// Classify a response by its `Content-Type` header.
#[must_use]
pub fn content_kind(content_type: Option<&str>) -> ContentKind {
    /// `application/*` types that are text in practice.
    const TEXTUAL_APPLICATION_TYPES: &[&str] = &[
        "application/json",
        "application/xml",
        "application/javascript",
        "application/ecmascript",
        "application/x-javascript",
        "application/yaml",
        "application/x-yaml",
        "application/toml",
        "application/x-sh",
        "application/x-httpd-php",
        "application/sql",
        "application/graphql",
        "application/x-ndjson",
        "application/csv",
        "application/x-tex",
        "application/x-www-form-urlencoded",
    ];
    let Some(essence) = content_type.map(essence).filter(|e| !e.is_empty()) else {
        return ContentKind::Unknown;
    };
    if essence == "application/octet-stream" || essence == "binary/octet-stream" {
        return ContentKind::Unknown;
    }
    let textual = essence.starts_with("text/")
        || essence.ends_with("+json")
        || essence.ends_with("+xml")
        || essence.ends_with("+yaml")
        || TEXTUAL_APPLICATION_TYPES.contains(&essence.as_str());
    if textual {
        ContentKind::Text
    } else {
        ContentKind::NonText
    }
}

/// Whether an undeclared body is binary. Text never contains a NUL byte;
/// nearly every binary format does within its first few kilobytes.
#[must_use]
pub fn looks_binary(bytes: &[u8]) -> bool {
    const SNIFF_BYTES: usize = 8192;
    bytes[..bytes.len().min(SNIFF_BYTES)].contains(&0)
}

/// What the model is told instead of the body of a non-text response, so it
/// knows what the URL holds and can look for another way to read it.
#[must_use]
pub fn non_text_notice(content_type: Option<&str>, length: Option<u64>) -> String {
    let kind = content_type
        .map(str::trim)
        .filter(|ct| !ct.is_empty())
        .unwrap_or("unknown (no Content-Type header; the body looks binary)");
    let size = length.map_or_else(String::new, |n| format!(", {n} bytes"));
    format!(
        "[This URL returned non-text content (Content-Type: {kind}{size}). Its body was not \
         converted to text. To read it, look for an HTML or plain-text version of the same \
         resource, or another source for the same information.]"
    )
}

/// The `charset` a `Content-Type` header declares, if any.
fn header_charset(content_type: &str) -> Option<String> {
    content_type.split(';').skip(1).find_map(|param| {
        let (name, value) = param.split_once('=')?;
        name.trim()
            .eq_ignore_ascii_case("charset")
            .then(|| value.trim().trim_matches(['"', '\'']).to_owned())
    })
}

/// The `charset` an HTML document declares in a `<meta>` tag near its
/// start (`<meta charset="…">` or the `http-equiv` form).
fn meta_charset(bytes: &[u8]) -> Option<String> {
    const SNIFF_BYTES: usize = 2048;
    let head = &bytes[..bytes.len().min(SNIFF_BYTES)];
    let head = String::from_utf8_lossy(head).to_ascii_lowercase();
    let start = head.find("charset=")? + "charset=".len();
    let label: String = head[start..]
        .trim_start_matches(['"', '\''])
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '.'))
        .collect();
    (!label.is_empty()).then_some(label)
}

/// Decode a text body to a `String`.
///
/// A byte-order mark wins, then the header's `charset`, then a `<meta>`
/// charset; with none of them the body is taken as UTF-8. A body that is
/// valid UTF-8 and declares nothing else is used as it is. Undecodable
/// bytes become U+FFFD rather than dropping the page.
#[must_use]
pub fn decode(bytes: &[u8], content_type: Option<&str>) -> String {
    // A `<meta>` charset only means something in HTML: a plain-text or
    // JSON body that happens to contain "charset=" must not re-decode it.
    let may_be_html = content_type.is_none_or(|ct| essence(ct).contains("html"));
    let declared = content_type
        .and_then(header_charset)
        .or_else(|| may_be_html.then(|| meta_charset(bytes)).flatten())
        .and_then(|label| encoding_rs::Encoding::for_label(label.as_bytes()));
    let encoding = declared.unwrap_or(encoding_rs::UTF_8);
    // `decode` sniffs a BOM first, so a UTF-16 page is read as UTF-16
    // whatever the header claims.
    let (text, _, _) = encoding.decode(bytes);
    text.into_owned()
}

#[cfg(test)]
mod tests;
