//! The I/O seam of the tool.
//!
//! [`Backend`] is everything the orchestration ([`super::run`]) needs from the
//! outside world: run a search, fetch a page, shorten text. [`LiveBackend`]
//! is the production implementation; tests substitute a fake, so the
//! orchestration is exercised without a network or an upstream.

use std::future::Future;
use std::time::Duration;

use crate::config::{WebFetchConfig, WebSearchConfig};
use crate::tools::domains::DomainRules;
use crate::tools::web_fetch::fetch as page_fetch;
use crate::tools::web_search::compact::{CompactRequest, Compactor, UpstreamCompactor};
use crate::tools::web_search::ddg::{self, SearchHit};

/// A fetched, readable page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageText {
    /// The URL after redirects.
    pub final_url: String,
    pub title: Option<String>,
    pub text: String,
}

/// Everything the tool needs from the outside world.
pub trait Backend: Compactor + Sync {
    /// Search for `query`, returning at most `limit` hits.
    fn search(
        &self,
        query: &str,
        limit: usize,
    ) -> impl Future<Output = Result<Vec<SearchHit>, String>> + Send;

    /// Fetch one result page and extract its readable text.
    fn fetch_page(&self, url: &str) -> impl Future<Output = Result<PageText, String>> + Send;
}

/// The production [`Backend`]: DuckDuckGo over HTTP, pages through the
/// SSRF-hardened `web_fetch` fetcher, compaction through the upstream.
pub struct LiveBackend {
    cfg: WebSearchConfig,
    client: reqwest::Client,
    compactor: UpstreamCompactor,
    /// DuckDuckGo region (`kl`) from the client's `user_location`.
    region: &'static str,
    /// The client's domain rules, enforced on every page fetch (including
    /// each redirect hop).
    domains: DomainRules,
}

impl LiveBackend {
    /// # Errors
    /// The HTTP client could not be built.
    pub fn new(
        cfg: WebSearchConfig,
        compactor: UpstreamCompactor,
        region: &'static str,
        domains: DomainRules,
    ) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .http1_title_case_headers()
            // This endpoint is the operator-configured search service, not an
            // arbitrary URL fetched on a model's behalf. System proxy support
            // is safe here; page fetches use their separate pinned client.
            .timeout(Duration::from_secs(cfg.search_timeout_secs))
            .redirect(reqwest::redirect::Policy::limited(3))
            .build()
            .map_err(|e| format!("could not build the search HTTP client: {e}"))?;
        Ok(Self {
            cfg,
            client,
            compactor,
            region,
            domains,
        })
    }
}

impl Compactor for LiveBackend {
    fn compact(
        &self,
        request: CompactRequest<'_>,
    ) -> impl Future<Output = Result<String, String>> + Send {
        self.compactor.compact(request)
    }
}

impl Backend for LiveBackend {
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>, String> {
        ddg::search(
            &self.client,
            &self.cfg.search_endpoint,
            query,
            self.region,
            limit,
        )
        .await
        .map_err(|e| e.to_string())
    }

    async fn fetch_page(&self, url: &str) -> Result<PageText, String> {
        // Result URLs come from a web page, i.e. from an untrusted party, so
        // they go through the same SSRF-hardened fetcher as `web_fetch`. Only
        // the three limits below are borrowed from it.
        let fetch_cfg = WebFetchConfig {
            enabled: true,
            timeout_secs: Some(self.cfg.page_timeout_secs),
            max_bytes: Some(self.cfg.max_page_bytes),
            max_content_bytes: None,
        };
        let page = page_fetch::fetch_within(url, &fetch_cfg, &self.domains)
            .await
            .map_err(|e| format!("{} ({})", e.detail, e.code))?;
        Ok(PageText {
            final_url: page.final_url,
            title: page.title,
            text: page.text,
        })
    }
}
