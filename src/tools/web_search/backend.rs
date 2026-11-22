//! The I/O seam of the tool.
//!
//! [`Backend`] is everything the orchestration ([`super::run`]) needs from the
//! outside world: run a search, fetch a page, shorten text. [`LiveBackend`]
//! is the production implementation; tests substitute a fake, so the
//! orchestration is exercised without a network or an upstream.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use crate::config::{WebFetchConfig, WebSearchConfig};
use crate::tools::domains::DomainRules;
use crate::tools::web_fetch::fetch as page_fetch;
use crate::tools::web_search::compact::{CompactRequest, Compactor, UpstreamCompactor};
use crate::tools::web_search::ddg::{self, SearchHit};
use crate::tools::web_search::diagnostics;
use crate::tools::web_search::gate;
use crate::tools::web_search::session::{self, SearchSession};

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
    /// The shared HTTP client, cookie session and result cache.
    session: Arc<SearchSession>,
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
        let session = session::session_for(&cfg)?;
        Ok(Self {
            cfg,
            session,
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
        let cache = &self.session.cache;
        // A repeat of a recent search never reaches the engine, and does not
        // wait its turn at the gate either.
        if let Some(hits) = cache.get(self.region, query, limit) {
            return Ok(hits);
        }
        // One search at a time, spaced out, across every request.
        let gate = gate::gate_for(&self.cfg.search_endpoint);
        let mut slot = gate
            .acquire(Duration::from_millis(self.cfg.search_min_interval_ms))
            .await;
        // The same search may have finished while this one queued.
        if let Some(hits) = cache.get(self.region, query, limit) {
            return Ok(hits);
        }
        let result = ddg::search(
            &self.session.client,
            &self.cfg.search_endpoint,
            query,
            self.region,
            limit,
            Duration::from_secs(self.cfg.search_timeout_secs),
        )
        .await;
        let state_dir = self.cfg.state_dir.as_deref();
        let outcome = match result {
            Ok(searched) => {
                slot.succeeded();
                self.session.note_ok();
                if searched.hits.is_empty() && !searched.report.says_no_results() {
                    // Nothing found and no explanation: a soft block or a
                    // markup change looks exactly like this.
                    let ctx = self.session.context(
                        &self.cfg.search_endpoint,
                        self.region,
                        query,
                        self.session.snapshot(),
                    );
                    let saved = diagnostics::save_page(state_dir, &searched.report);
                    diagnostics::log_unexplained_empty(&ctx, &searched.report, saved.as_deref());
                } else {
                    tracing::debug!(
                        query,
                        hits = searched.hits.len(),
                        status = ?searched.report.status,
                        http_version = %searched.report.http_version,
                        elapsed_ms = searched.report.elapsed_ms,
                        "web_search search ok"
                    );
                }
                cache.put(self.region, query, limit, &searched.hits);
                Ok(searched.hits)
            }
            Err(error) => {
                let snapshot = self.session.note_failure(error.blocked);
                let ctx =
                    self.session
                        .context(&self.cfg.search_endpoint, self.region, query, snapshot);
                let saved = error
                    .report
                    .as_deref()
                    .filter(|_| error.blocked)
                    .and_then(|r| diagnostics::save_page(state_dir, r));
                diagnostics::log_failure(
                    &ctx,
                    error.blocked,
                    &error.message,
                    error.report.as_deref(),
                    saved.as_deref(),
                );
                if error.blocked {
                    Err(gate::blocked_message(
                        &error.message,
                        slot.blocked(),
                        self.cfg.block_wait_minutes,
                    ))
                } else {
                    Err(error.message)
                }
            }
        };
        drop(slot);
        outcome
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
