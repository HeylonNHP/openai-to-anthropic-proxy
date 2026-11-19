//! The orchestration of one `web_search` tool call.
//!
//! ```text
//! queries ──▶ normalise (trim, dedup, cap at max_queries)
//!         ──▶ search each query   (staggered, ≤ 2 at a time)
//!         ──▶ fetch each UNIQUE result URL, once   (≤ 4 at a time)
//!         ──▶ fit each page to the compaction threshold
//!         ──▶ Report
//! ```
//!
//! It is generic over [`Backend`] and performs no I/O of its own. One failed
//! query or page never discards the others: failures are data in the
//! [`Report`].

use std::collections::HashMap;
use std::time::Duration;

use futures_util::StreamExt;
use futures_util::stream;

use crate::config::WebSearchConfig;
use crate::tools::domains::DomainRules;
use crate::tools::web_search::backend::Backend;
use crate::tools::web_search::compact::fit;
use crate::tools::web_search::ddg::SearchHit;

/// DuckDuckGo answers a burst of searches with a CAPTCHA, so searches are
/// few at a time and spaced out.
const SEARCH_CONCURRENCY: usize = 2;
const SEARCH_STAGGER: Duration = Duration::from_millis(400);
/// Pages (and the compaction calls behind them) in flight at once.
const PAGE_CONCURRENCY: usize = 4;

/// The knobs one run needs.
#[derive(Debug, Clone)]
pub struct RunSettings {
    pub max_queries: usize,
    pub max_results_per_query: usize,
    pub threshold_chars: usize,
    pub search_concurrency: usize,
    pub page_concurrency: usize,
    /// Delay between the start of consecutive searches.
    pub stagger: Duration,
    /// The client's `allowed_domains` / `blocked_domains`: put in each query
    /// as search operators, and enforced on the hits afterwards (the engine
    /// may ignore an operator).
    pub domains: DomainRules,
}

impl RunSettings {
    #[must_use]
    pub fn from_config(cfg: &WebSearchConfig) -> Self {
        Self {
            max_queries: cfg.max_queries,
            max_results_per_query: cfg.max_results_per_query,
            threshold_chars: cfg.compaction_threshold_chars,
            search_concurrency: SEARCH_CONCURRENCY,
            page_concurrency: PAGE_CONCURRENCY,
            stagger: SEARCH_STAGGER,
            domains: DomainRules::default(),
        }
    }
}

/// A page, ready for the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    pub final_url: String,
    pub title: Option<String>,
    pub text: String,
    /// Set when `text` is not verbatim (summarised or truncated).
    pub note: Option<String>,
}

/// One search result and what became of its page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub hit: SearchHit,
    pub page: Result<Page, String>,
}

/// One query's outcome: its entries, or why the search itself failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryReport {
    pub query: String,
    pub outcome: Result<Vec<Entry>, String>,
}

/// Everything one tool call produced.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Report {
    pub queries: Vec<QueryReport>,
    /// Queries the model asked for beyond `max_queries`.
    pub dropped_queries: usize,
}

/// Trim, drop empties and duplicates (case-insensitively), and cap at `max`.
/// Returns the kept queries and how many valid ones were dropped by the cap.
#[must_use]
pub fn normalize_queries(raw: &[String], max: usize) -> (Vec<String>, usize) {
    let mut kept: Vec<String> = Vec::new();
    let mut dropped = 0;
    for query in raw {
        let query = query.split_whitespace().collect::<Vec<_>>().join(" ");
        if query.is_empty() || kept.iter().any(|k| k.eq_ignore_ascii_case(&query)) {
            continue;
        }
        if kept.len() < max {
            kept.push(query);
        } else {
            dropped += 1;
        }
    }
    (kept, dropped)
}

/// Run one tool call's queries.
pub async fn run<B: Backend>(
    backend: &B,
    settings: &RunSettings,
    raw_queries: &[String],
) -> Report {
    let (queries, dropped_queries) = normalize_queries(raw_queries, settings.max_queries);

    // Phase 1: search.
    let searches: Vec<Result<Vec<SearchHit>, String>> =
        stream::iter(queries.clone().into_iter().enumerate())
            .map(|(index, query)| async move {
                if index > 0 && !settings.stagger.is_zero() {
                    tokio::time::sleep(settings.stagger * u32::try_from(index).unwrap_or(u32::MAX))
                        .await;
                }
                let wire_query = settings.domains.narrow_query(&query);
                let mut hits = backend
                    .search(&wire_query, settings.max_results_per_query)
                    .await?;
                hits.retain(|hit| settings.domains.allows(&hit.url));
                Ok(hits)
            })
            .buffered(settings.search_concurrency.max(1))
            .collect()
            .await;

    // Phase 2: every unique URL once, however many queries returned it.
    let mut urls: Vec<String> = Vec::new();
    for hits in searches.iter().flatten() {
        for hit in hits {
            if !urls.contains(&hit.url) {
                urls.push(hit.url.clone());
            }
        }
    }
    let pages: HashMap<String, Result<Page, String>> = stream::iter(urls)
        .map(|url| async move {
            let page = load_page(backend, &url, settings.threshold_chars).await;
            (url, page)
        })
        .buffered(settings.page_concurrency.max(1))
        .collect()
        .await;

    let queries = queries
        .into_iter()
        .zip(searches)
        .map(|(query, search)| QueryReport {
            query,
            outcome: search.map(|hits| {
                hits.into_iter()
                    .map(|hit| {
                        let page = pages
                            .get(&hit.url)
                            .cloned()
                            .unwrap_or_else(|| Err("the page was not fetched".to_owned()));
                        Entry { hit, page }
                    })
                    .collect()
            }),
        })
        .collect();

    Report {
        queries,
        dropped_queries,
    }
}

/// Fetch one page and fit it to the threshold.
async fn load_page<B: Backend>(backend: &B, url: &str, threshold: usize) -> Result<Page, String> {
    let fetched = backend.fetch_page(url).await?;
    let fitted = fit(fetched.text, threshold, backend).await;
    Ok(Page {
        final_url: fetched.final_url,
        title: fetched.title,
        note: fitted.note(),
        text: fitted.text().to_owned(),
    })
}

#[cfg(test)]
mod tests;
