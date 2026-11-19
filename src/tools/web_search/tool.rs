//! `web_search` as a [`ServerTool`]: serve the executor's search calls.
//!
//! The loop around it (running the executor, finding calls, injecting
//! outputs, assembling the response) is the shared
//! [`server_loop`](crate::tools::server_loop); this module is only what is
//! specific to searching.
//!
//! ## Fail closed
//!
//! A failed search or page never fails the client request: it becomes an
//! error block for the client and error text for the executor, and the turn
//! continues.
//!
//! ## Bounded
//!
//! Nothing here is unlimited. A call runs at most `max_queries` queries, the
//! client's `max_uses` caps the queries in a request, and the shared loop caps
//! the rounds (`server_tools.max_rounds`).

// `tools` is a private module, so these crate-visible items look redundant to
// clippy; they must be `pub(crate)` because they name `proxy::Attempt`.
#![allow(clippy::redundant_pub_crate)]

use futures_util::future::BoxFuture;
use serde_json::Value;

use crate::anthropic::{ResponseContentBlock, ServerToolOptions, ServerToolUseUsage, Usage};
use crate::config::WebSearchConfig;
use crate::error::AppError;
use crate::proxy::Attempt;
use crate::tools::domains::DomainRules;
use crate::tools::server_loop::handler::{Call, Served, ServerTool};
use crate::tools::web_fetch::protocol::new_id;
use crate::tools::web_search::backend::{Backend, LiveBackend};
use crate::tools::web_search::compact::UpstreamCompactor;
use crate::tools::web_search::ddg;
use crate::tools::web_search::run::{self, RunSettings};
use crate::tools::web_search::{EXECUTOR_TOOL_NAME, format, protocol};
use crate::tui::{LogKind, LogLine};

const INVALID_INPUT: &str = "The web search request had no usable queries. Call the tool again \
     with `queries` set to a list of non-empty search strings.";

/// Serves `proxy_web_search` calls through a [`Backend`].
pub struct WebSearchTool<B> {
    backend: B,
    settings: RunSettings,
    /// The client's `max_uses`: queries searched in this request, across
    /// every call. `None` is only the per-call and per-turn limits below.
    max_uses: Option<usize>,
    /// Queries searched so far (reported in `usage`).
    search_requests: u32,
    /// One operator-visible warning per turn, however many searches fail.
    warned: bool,
}

impl WebSearchTool<LiveBackend> {
    /// The production tool for this request, configured with the options the
    /// client set on its `web_search` declaration.
    ///
    /// # Errors
    ///
    /// The search HTTP client could not be built.
    pub(crate) fn live(
        attempt: &Attempt,
        cfg: &WebSearchConfig,
        options: &ServerToolOptions,
    ) -> Result<Self, AppError> {
        let compactor = UpstreamCompactor::new(
            attempt.state.clone(),
            attempt.url(),
            attempt.headers.clone(),
            cfg,
        );
        let domains = DomainRules::from_options(options);
        let region = ddg::region_for(options.user_location.as_ref());
        let backend = LiveBackend::new(cfg.clone(), compactor, region, domains.clone())
            .map_err(AppError::Internal)?;
        let settings = RunSettings {
            domains,
            ..RunSettings::from_config(cfg)
        };
        Ok(Self::new(backend, settings).with_max_uses(options.max_uses))
    }
}

/// The answer to one call, plus the failures worth telling the operator.
struct Answer {
    served: Served,
    /// `(query, error)` for every search that failed.
    failures: Vec<(String, String)>,
}

impl<B: Backend> WebSearchTool<B> {
    pub const fn new(backend: B, settings: RunSettings) -> Self {
        Self {
            backend,
            settings,
            max_uses: None,
            search_requests: 0,
            warned: false,
        }
    }

    /// Apply the client's `max_uses`.
    #[must_use]
    pub fn with_max_uses(mut self, max_uses: Option<u32>) -> Self {
        self.max_uses = max_uses.map(|m| usize::try_from(m).unwrap_or(usize::MAX));
        self
    }

    /// How many more queries the client's `max_uses` allows.
    fn remaining_uses(&self) -> usize {
        self.max_uses.map_or(usize::MAX, |max| {
            max.saturating_sub(usize::try_from(self.search_requests).unwrap_or(usize::MAX))
        })
    }

    /// Answer one call. Performs the searches; touches nothing outside `self`.
    async fn answer(&mut self, call: &Call) -> Answer {
        let queries = parse_queries(&call.arguments);

        if run::normalize_queries(&queries, usize::MAX).0.is_empty() {
            return Answer {
                served: Served {
                    blocks: pair_error("", protocol::ERROR_INVALID_INPUT),
                    output: INVALID_INPUT.to_owned(),
                },
                failures: Vec::new(),
            };
        }

        // Each query is one use. Those beyond the client's `max_uses` are
        // refused (an error result for the client, a note for the model); the
        // rest run.
        let (kept, capped) = run::normalize_queries(&queries, self.settings.max_queries);
        let allowed = kept.len().min(self.remaining_uses());
        let (to_run, refused) = kept.split_at(allowed);
        let mut report = if to_run.is_empty() {
            run::Report::default()
        } else {
            run::run(&self.backend, &self.settings, to_run).await
        };
        report.dropped_queries = capped;
        self.search_requests = self
            .search_requests
            .saturating_add(u32::try_from(report.queries.len()).unwrap_or(u32::MAX));

        let mut blocks = Vec::new();
        let mut failures = Vec::new();
        for query in &report.queries {
            let id = new_id();
            blocks.push(ResponseContentBlock::Raw(protocol::server_tool_use(
                &id,
                &query.query,
            )));
            blocks.push(ResponseContentBlock::Raw(match &query.outcome {
                Ok(entries) => protocol::search_result(&id, entries),
                Err(error) => {
                    failures.push((query.query.clone(), error.clone()));
                    protocol::error_result(&id, protocol::ERROR_UNAVAILABLE)
                }
            }));
        }
        for query in refused {
            blocks.extend(pair_error(query, protocol::ERROR_MAX_USES_EXCEEDED));
        }
        let mut output = format::render(&report);
        if !refused.is_empty() {
            output.push_str(&format!(
                "\nnote: {} queries were NOT run because the web search limit for this request \
                 ({}) was reached. Answer with what you have.\n",
                refused.len(),
                self.max_uses.unwrap_or_default()
            ));
        }
        Answer {
            served: Served { blocks, output },
            failures,
        }
    }

    /// The first failure of the turn is the useful one; repeats are noise.
    fn warn_once(&mut self, attempt: &Attempt, failures: &[(String, String)]) {
        let Some((query, error)) = failures.first() else {
            return;
        };
        if std::mem::replace(&mut self.warned, true) {
            return;
        }
        tracing::warn!(query = %query, error = %error, "web_search query failed; continuing without it");
        attempt.state.output.emit(LogLine {
            kind: LogKind::Warning,
            text: format!("  \u{26a0} web_search failed for \"{query}\": {error}"),
        });
    }
}

impl<B: Backend + Send + 'static> ServerTool for WebSearchTool<B> {
    fn executor_name(&self) -> &'static str {
        EXECUTOR_TOOL_NAME
    }

    fn serve<'a>(&'a mut self, attempt: &'a Attempt, call: &'a Call) -> BoxFuture<'a, Served> {
        Box::pin(async move {
            let answer = self.answer(call).await;
            self.warn_once(attempt, &answer.failures);
            answer.served
        })
    }

    fn contribute_usage(&self, usage: &mut Usage) {
        if self.search_requests > 0 {
            usage
                .server_tool_use
                .get_or_insert_with(ServerToolUseUsage::default)
                .web_search_requests = self.search_requests;
        }
    }
}

/// An error pair for a call that could not be served.
fn pair_error(query: &str, code: &str) -> Vec<ResponseContentBlock> {
    let id = new_id();
    vec![
        ResponseContentBlock::Raw(protocol::server_tool_use(&id, query)),
        ResponseContentBlock::Raw(protocol::error_result(&id, code)),
    ]
}

/// The `queries` strings of a call's arguments; anything malformed is an
/// empty list (answered with [`INVALID_INPUT`], never a loop-spinning skip).
pub(crate) fn parse_queries(arguments: &str) -> Vec<String> {
    serde_json::from_str::<Value>(arguments)
        .ok()
        .and_then(|v| v.get("queries").cloned())
        .and_then(|q| q.as_array().cloned())
        .map(|items| {
            items
                .iter()
                .filter_map(|q| q.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests;
