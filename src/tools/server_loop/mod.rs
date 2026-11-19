//! The shared loop behind every server-side tool the proxy serves itself.
//!
//! Anthropic's server tools (`web_search`, `web_fetch`, `advisor`) run inside
//! the API: the model calls one, the server satisfies the call, and the model
//! carries on, all within one request. The upstream here has no such tools, so
//! the proxy stands in for the server. This module is that stand-in, written
//! ONCE for all of them:
//!
//! ```text
//! execute ──▶ executor response
//!                │
//!                ├─ no call to any server tool ──▶ done
//!                │
//!                └─ calls (any mix of tools, in the order made)
//!                       ├─ each tool serves its own call
//!                       ├─ the client gets server_tool_use + result blocks
//!                       ├─ its words, calls and outputs → its input
//!                       └─ re-run the executor
//! ```
//!
//! A tool plugs in by implementing [`handler::ServerTool`] and being returned
//! from [`handlers`]; nothing else in the loop knows about it. Removing a tool
//! means deleting its module and its one line in [`handlers`].
//!
//! | module | role |
//! |---|---|
//! | [`handler`] | the [`handler::ServerTool`] interface |
//! | [`run`] | the loop |
//! | [`render`] | assemble the final Anthropic response |
//! | [`stream`] | stream a turn to the client as it happens |

// `tools` is a private module, so these crate-visible items look redundant to
// clippy; they must be `pub(crate)` because they name `proxy::Attempt`.
#![allow(clippy::redundant_pub_crate)]

use crate::error::AppError;
use crate::proxy::Attempt;
use crate::tools::{advisor, web_fetch, web_search};

pub(crate) mod handler;
pub(crate) mod render;
pub(crate) mod run;
pub(crate) mod stream;
pub(crate) mod summary;

use handler::ServerTool;

/// The tools that must serve THIS request: those that are enabled and that
/// the client actually declared. Empty means the request takes the ordinary
/// pipeline, untouched.
///
/// # Errors
///
/// A tool could not be set up (for example its HTTP client could not be
/// built).
pub(crate) fn handlers(attempt: &Attempt) -> Result<Vec<Box<dyn ServerTool>>, AppError> {
    let config = &attempt.state.config;
    let mut tools: Vec<Box<dyn ServerTool>> = Vec::new();
    if web_search::active(&config.web_search, &attempt.req) {
        let options = attempt
            .req
            .server_tool_options(web_search::WEB_SEARCH_TOOL_NAME, "web_search_");
        tools.push(Box::new(web_search::tool::WebSearchTool::live(
            attempt,
            &config.web_search,
            &options,
        )?));
    }
    if web_fetch::active(&config.web_fetch, &attempt.req) {
        let options = attempt
            .req
            .server_tool_options(web_fetch::WEB_FETCH_TOOL_NAME, "web_fetch_");
        tools.push(Box::new(web_fetch::tool::WebFetchTool::new(
            &config.web_fetch,
            &options,
        )));
    }
    if advisor::active(config, &attempt.req) {
        tools.push(Box::new(advisor::tool::AdvisorTool::new(&config.advisor)));
    }
    Ok(tools)
}
