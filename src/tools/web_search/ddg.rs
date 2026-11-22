//! DuckDuckGo HTML-interface scraping.
//!
//! [`parse_results`] is pure (HTML in, hits out) so it can be tested against
//! fixtures; [`search`] is the thin HTTP call around it.
//!
//! Only the FIRST results page is read. It carries about ten organic results,
//! which is also the ceiling `web_search.max_results_per_query` is clamped to,
//! so pagination (and the `vqd` token it needs) is not implemented.

use std::time::Duration;

use dom_query::{Document, Selection};

use crate::tools::http_profile;

/// One organic search result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// What a results page parsed into.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ParsedPage {
    pub hits: Vec<SearchHit>,
    /// DuckDuckGo answered with its bot challenge instead of results.
    pub challenged: bool,
}

/// A failed search. A plain message: it becomes tool output, never a client
/// error.
#[derive(Debug, PartialEq, Eq)]
pub struct SearchError {
    pub message: String,
    /// The engine refused this client (bot challenge, rate limit) rather than
    /// the search failing for some other reason.
    pub blocked: bool,
}

impl SearchError {
    fn failed(message: String) -> Self {
        Self {
            message,
            blocked: false,
        }
    }

    fn blocked(message: String) -> Self {
        Self {
            message,
            blocked: true,
        }
    }
}

impl std::fmt::Display for SearchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Parse one DuckDuckGo HTML results page into at most `limit` hits.
#[must_use]
pub fn parse_results(html: &str, limit: usize) -> ParsedPage {
    let doc = Document::from(html);
    if doc.select("#challenge-form").exists() {
        return ParsedPage {
            hits: Vec::new(),
            challenged: true,
        };
    }

    // The current markup is `div#links div.web-result`; older pages (and the
    // lite fallback) use a bare `.result`.
    let mut rows = doc.select("div#links div.web-result");
    if !rows.exists() {
        rows = doc.select(".result");
    }

    let mut hits: Vec<SearchHit> = Vec::new();
    for row in rows.iter() {
        if hits.len() >= limit {
            break;
        }
        if row.has_class("result--ad") {
            continue;
        }
        let mut link = row.select("h2 a");
        if !link.exists() {
            link = row.select("a.result__a");
        }
        let title = clean(&link.text());
        let href = link.attr("href").map(|h| h.to_string()).unwrap_or_default();
        if title.is_empty() || href.is_empty() {
            continue;
        }
        let Some(url) = resolve_result_url(&href) else {
            continue;
        };
        if hits.iter().any(|h| h.url == url) {
            continue;
        }
        hits.push(SearchHit {
            title,
            url,
            snippet: snippet_of(&row),
        });
    }
    ParsedPage {
        hits,
        challenged: false,
    }
}

fn snippet_of(row: &Selection) -> String {
    let mut snippet = row.select("a.result__snippet");
    if !snippet.exists() {
        snippet = row.select(".result__snippet");
    }
    clean(&snippet.text())
}

/// Collapse runs of whitespace.
fn clean(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Turn a result link into the real target URL.
///
/// DuckDuckGo wraps targets as `//duckduckgo.com/l/?uddg=<encoded url>`.
/// Anything that is not an http(s) URL once unwrapped (ad redirectors,
/// `javascript:` links) is dropped.
fn resolve_result_url(href: &str) -> Option<String> {
    let base = url::Url::parse("https://duckduckgo.com/").ok()?;
    let parsed = base.join(href).ok()?;
    let target = parsed
        .query_pairs()
        .find(|(k, _)| k == "uddg")
        .map_or_else(|| parsed.to_string(), |(_, v)| v.into_owned());
    let target_url = url::Url::parse(&target).ok()?;
    if !matches!(target_url.scheme(), "http" | "https") {
        return None;
    }
    // Sponsored results go through DuckDuckGo's own click tracker.
    if target_url.host_str() == Some("duckduckgo.com") && target_url.path().starts_with("/y.js") {
        return None;
    }
    Some(target_url.to_string())
}

/// DuckDuckGo's "no region" setting.
pub const NO_REGION: &str = "wt-wt";

/// DuckDuckGo's region code (`kl`) for a client's `user_location`.
///
/// Only the ISO country is used: DuckDuckGo localises by country and
/// language, not by city. An absent location, or a country DuckDuckGo has no
/// region for, is [`NO_REGION`].
#[must_use]
pub fn region_for(user_location: Option<&serde_json::Value>) -> &'static str {
    let country = user_location
        .and_then(|loc| loc.get("country"))
        .and_then(serde_json::Value::as_str)
        .map(|c| c.trim().to_ascii_uppercase());
    // (ISO 3166-1 alpha-2, DuckDuckGo `kl`). Note GB is `uk`.
    const REGIONS: &[(&str, &str)] = &[
        ("AR", "ar-es"),
        ("AT", "at-de"),
        ("AU", "au-en"),
        ("BE", "be-fr"),
        ("BR", "br-pt"),
        ("CA", "ca-en"),
        ("CH", "ch-de"),
        ("CL", "cl-es"),
        ("CN", "cn-zh"),
        ("CO", "co-es"),
        ("CZ", "cz-cs"),
        ("DE", "de-de"),
        ("DK", "dk-da"),
        ("ES", "es-es"),
        ("FI", "fi-fi"),
        ("FR", "fr-fr"),
        ("GB", "uk-en"),
        ("GR", "gr-el"),
        ("HK", "hk-tzh"),
        ("HU", "hu-hu"),
        ("ID", "id-en"),
        ("IE", "ie-en"),
        ("IL", "il-en"),
        ("IN", "in-en"),
        ("IT", "it-it"),
        ("JP", "jp-jp"),
        ("KR", "kr-kr"),
        ("MX", "mx-es"),
        ("MY", "my-en"),
        ("NL", "nl-nl"),
        ("NO", "no-no"),
        ("NZ", "nz-en"),
        ("PH", "ph-en"),
        ("PL", "pl-pl"),
        ("PT", "pt-pt"),
        ("RO", "ro-ro"),
        ("RU", "ru-ru"),
        ("SE", "se-sv"),
        ("SG", "sg-en"),
        ("TH", "th-th"),
        ("TR", "tr-tr"),
        ("TW", "tw-tzh"),
        ("UA", "ua-uk"),
        ("US", "us-en"),
        ("VN", "vn-vi"),
        ("ZA", "za-en"),
    ];
    country
        .and_then(|c| REGIONS.iter().find(|(iso, _)| *iso == c).map(|(_, kl)| *kl))
        .unwrap_or(NO_REGION)
}

/// Run one search against `endpoint` and return up to `limit` hits.
///
/// # Errors
/// A transport failure, a non-2xx status, or DuckDuckGo's bot challenge.
pub async fn search(
    client: &reqwest::Client,
    endpoint: &str,
    query: &str,
    region: &str,
    limit: usize,
    timeout: Duration,
) -> Result<Vec<SearchHit>, SearchError> {
    // The client is shared (it carries the session), so the budget is set per
    // request rather than on the client.
    let request = http_profile::apply_form_submit(
        client.post(endpoint),
        endpoint,
        &[("q", query), ("b", ""), ("kl", region)],
    )
    .timeout(timeout);
    let response = request
        .send()
        .await
        .map_err(|e| SearchError::failed(format!("search request failed: {e}")))?;

    let status = response.status();
    if !status.is_success() {
        // DuckDuckGo answers a flagged client with a 202 and no results.
        return Err(SearchError::blocked(format!(
            "the search engine returned {status} (it may be rate-limiting or challenging this IP)"
        )));
    }
    let html = response
        .text()
        .await
        .map_err(|e| SearchError::failed(format!("reading the search response failed: {e}")))?;

    let page = parse_results(&html, limit);
    if page.challenged {
        return Err(SearchError::blocked(
            "the search engine returned a bot challenge (CAPTCHA)".to_owned(),
        ));
    }
    Ok(page.hits)
}

#[cfg(test)]
mod tests;
