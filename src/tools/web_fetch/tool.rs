//! `web_fetch` as a [`ServerTool`]: serve the executor's fetch calls.
//!
//! The loop around it (running the executor, finding calls, injecting
//! outputs, assembling the response) is the shared
//! [`server_loop`](crate::tools::server_loop); this module is only what is
//! specific to fetching a page.
//!
//! ## Fail closed
//!
//! A fetch failure NEVER fails the client request. The failure becomes a
//! `web_fetch_tool_result` error block, the executor is told the fetch
//! failed, and the turn continues: the contract Anthropic's own server tools
//! have (errors come back as HTTP 200 with the error in the body).
//!
//! ## Unlimited fetches
//!
//! Anthropic's tool declaration carries a `max_uses`, but the operator asked
//! for it to be **unlimited**: the executor may fetch as many URLs as it likes
//! in one turn. A runaway loop is made visible in the log (and to the
//! operator) rather than capped.

// `tools` is a private module, so these crate-visible items look redundant to
// clippy; they must be `pub(crate)` because they name `proxy::Attempt`.
#![allow(clippy::redundant_pub_crate)]

use futures_util::future::BoxFuture;
use serde_json::Value;

use crate::anthropic::{ResponseContentBlock, ServerToolOptions, ServerToolUseUsage, Usage};
use crate::config::WebFetchConfig;
use crate::proxy::Attempt;
use crate::tools::domains::DomainRules;
use crate::tools::server_loop::handler::{Call, Served, ServerTool};
use crate::tools::web_fetch::{WEB_FETCH_TOOL_NAME, fetch, protocol};
use crate::tui::{LogKind, LogLine};

/// A turn that fetches this many URLs is a runaway loop.
///
/// The proxy does NOT cap it (the operator asked for unlimited fetches), but
/// it makes the hazard visible in the log and to the operator.
const RUNAWAY_FETCH_THRESHOLD: usize = 10;

/// Text injected into the executor's transcript when the fetch failed.
///
/// It must NOT look like page content: the client sees a
/// `web_fetch_tool_result` error block, and the executor must be told the
/// fetch failed rather than handed an empty page.
const FETCH_FAILED: &str =
    "The web_fetch request failed. The page could not be retrieved. Continue without it.";

/// Text injected when the call carried no usable `url`.
const INVALID_INPUT: &str = "The web_fetch request had no usable `url`. Call the tool again with `url` set to an \
     absolute http:// or https:// address.";

/// Text injected when the client's `max_uses` is used up.
const LIMIT_REACHED: &str = "The web_fetch limit for this request was reached. Answer with the information you already \
     have, and say if it is incomplete.";

/// Characters per token, for turning the client's `max_content_tokens` into
/// a byte budget. A rough, conservative rule of thumb: the fetch layer has no
/// tokenizer.
const CHARS_PER_TOKEN: u64 = 4;

/// Serves `web_fetch` calls.
pub struct WebFetchTool {
    cfg: WebFetchConfig,
    /// The client's `max_uses`. `None` is unlimited, as the operator chose.
    max_uses: Option<usize>,
    /// The client's `allowed_domains` / `blocked_domains`.
    rules: DomainRules,
    /// Fetches attempted so far this turn (reported in `usage`).
    fetches: usize,
    /// One operator-visible warning per turn, however many fetches fail.
    warned: bool,
}

/// The answer to one call, plus what is worth telling the operator.
struct Answer {
    served: Served,
    /// `(error code, detail)` when the fetch failed.
    failure: Option<(&'static str, String)>,
    /// This call was the one that crossed [`RUNAWAY_FETCH_THRESHOLD`].
    runaway: bool,
}

impl WebFetchTool {
    /// The tool for a request whose `web_fetch` declaration set `options`.
    ///
    /// `max_content_tokens` can only tighten the operator's own
    /// `max_content_bytes`, never loosen it.
    #[must_use]
    pub fn new(cfg: &WebFetchConfig, options: &ServerToolOptions) -> Self {
        let mut cfg = cfg.clone();
        if let Some(tokens) = options.max_content_tokens {
            let bytes = u64::from(tokens).saturating_mul(CHARS_PER_TOKEN);
            cfg.max_content_bytes = Some(cfg.max_content_bytes.map_or(bytes, |own| own.min(bytes)));
        }
        Self {
            cfg,
            max_uses: options
                .max_uses
                .map(|m| usize::try_from(m).unwrap_or(usize::MAX)),
            rules: DomainRules::from_options(options),
            fetches: 0,
            warned: false,
        }
    }

    /// Answer one call. Performs the fetch; touches nothing outside `self`.
    async fn answer(&mut self, call: &Call) -> Answer {
        let id = protocol::new_id();
        let Some(url) = parse_url_argument(&call.arguments) else {
            // Still answered: a skipped call would reach the client as an
            // un-served tool_use.
            return Answer {
                served: Served {
                    blocks: vec![
                        ResponseContentBlock::Raw(protocol::server_tool_use(&id, "")),
                        ResponseContentBlock::Raw(protocol::error_result(
                            &id,
                            protocol::ERROR_CODE_INVALID_TOOL_INPUT,
                        )),
                    ],
                    output: INVALID_INPUT.to_owned(),
                },
                failure: None,
                runaway: false,
            };
        };

        // The client's `max_uses`: a refused call is not a fetch, so it is not
        // counted and nothing is requested.
        if self.max_uses.is_some_and(|max| self.fetches >= max) {
            return Answer {
                served: Served {
                    blocks: vec![
                        ResponseContentBlock::Raw(protocol::server_tool_use(&id, &url)),
                        ResponseContentBlock::Raw(protocol::error_result(
                            &id,
                            protocol::ERROR_CODE_MAX_USES_EXCEEDED,
                        )),
                    ],
                    output: LIMIT_REACHED.to_owned(),
                },
                failure: None,
                runaway: false,
            };
        }

        self.fetches += 1;
        let runaway = self.fetches == RUNAWAY_FETCH_THRESHOLD;
        let use_block = ResponseContentBlock::Raw(protocol::server_tool_use(&id, &url));

        let (result_block, output, failure) =
            match fetch::fetch_within(&url, &self.cfg, &self.rules).await {
                Ok(page) => {
                    let block = protocol::web_fetch_result(
                        &id,
                        &page.final_url,
                        page.title.as_deref(),
                        &page.text,
                    );
                    (block, page.text, None)
                }
                Err(err) => (
                    protocol::error_result(&id, err.code),
                    FETCH_FAILED.to_owned(),
                    Some((err.code, err.detail)),
                ),
            };

        Answer {
            served: Served {
                blocks: vec![use_block, ResponseContentBlock::Raw(result_block)],
                output,
            },
            failure,
            runaway,
        }
    }

    /// Make a failed fetch and a runaway turn visible to the operator.
    fn warn(&mut self, attempt: &Attempt, answer: &Answer) {
        if answer.runaway {
            tracing::warn!(
                fetches = self.fetches,
                "the executor has fetched {RUNAWAY_FETCH_THRESHOLD} URLs in one turn; web_fetch is \
                 unlimited by configuration, so this will keep going until the executor stops asking"
            );
            attempt.state.output.emit(LogLine {
                kind: LogKind::Warning,
                text: format!(
                    "  \u{26a0} web_fetch: {} URLs fetched in one turn (unlimited by configuration)",
                    self.fetches
                ),
            });
        }
        let Some((code, detail)) = &answer.failure else {
            return;
        };
        if std::mem::replace(&mut self.warned, true) {
            return;
        }
        tracing::warn!(
            error_code = code,
            detail = %detail,
            "web_fetch failed; continuing without the page"
        );
        attempt.state.output.emit(LogLine {
            kind: LogKind::Warning,
            text: format!("  \u{26a0} web_fetch failed ({code}): {detail}"),
        });
    }
}

impl ServerTool for WebFetchTool {
    fn executor_name(&self) -> &'static str {
        WEB_FETCH_TOOL_NAME
    }

    fn serve<'a>(&'a mut self, attempt: &'a Attempt, call: &'a Call) -> BoxFuture<'a, Served> {
        Box::pin(async move {
            let answer = self.answer(call).await;
            self.warn(attempt, &answer);
            answer.served
        })
    }

    fn contribute_usage(&self, usage: &mut Usage) {
        if self.fetches > 0 {
            usage
                .server_tool_use
                .get_or_insert_with(ServerToolUseUsage::default)
                .web_fetch_requests = u32::try_from(self.fetches).unwrap_or(u32::MAX);
        }
    }
}

/// Pull the `url` out of the executor's arguments JSON. A call with no usable
/// `url` yields `None` and is answered with an error rather than fetched.
pub(crate) fn parse_url_argument(arguments: &str) -> Option<String> {
    let parsed: Value = serde_json::from_str(arguments).ok()?;
    let url = parsed.get("url")?.as_str()?.trim().to_owned();
    if url.is_empty() {
        return None;
    }
    Some(url)
}

#[cfg(test)]
mod tests;
