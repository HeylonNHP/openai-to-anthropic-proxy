//! A proxy-native implementation of Anthropic's server-side `web_search`
//! tool: DuckDuckGo scraping, result-page fetching and optional compaction.
//!
//! **Read [`README.md`](README.md) in this directory first** — it explains how
//! the tool works, why it is built the way it is, and how to remove it.
//!
//! This module is the integration surface: activation and the inbound
//! request rewrite. The work is split across the sibling modules:
//!
//! | module | role |
//! |---|---|
//! | [`ddg`] | parse and fetch DuckDuckGo result pages |
//! | [`compact`] | keep page text within the compaction threshold |
//! | [`backend`] | the I/O seam ([`backend::Backend`]) and its live implementation |
//! | [`run`] | orchestrate one tool call (search → fetch → fit) |
//! | [`format`] | the text the executor model reads |
//! | [`history`] | carry results into later turns (`encrypted_content`) |
//! | [`protocol`] | the wire blocks the client sees |
//! | [`tool`] | the [`ServerTool`](crate::tools::server_loop::handler::ServerTool) the shared loop calls |

use crate::anthropic::CreateMessageRequest;
use crate::config::WebSearchConfig;
use crate::responses::{ResponsesRequest, ResponsesTool, ToolDefinition};
use serde_json::json;

pub mod backend;
pub mod compact;
pub mod ddg;
pub mod format;
pub mod history;
pub mod protocol;
pub mod run;
pub mod tool;

/// The tool name the CLIENT declares (and sees in `server_tool_use`).
pub const WEB_SEARCH_TOOL_NAME: &str = "web_search";

/// The name of the function tool the EXECUTOR calls.
///
/// Deliberately NOT `web_search`: the translator and stream layers treat any
/// function call named `web_search` as the upstream's built-in search
/// (`translate::extract_web_search_citations`), which would mis-render ours.
pub const EXECUTOR_TOOL_NAME: &str = "proxy_web_search";

/// Whether the client declared the SERVER-side `web_search` tool: the name
/// `web_search` with no `input_schema` (Anthropic's server-tool shape). A
/// client function tool of the same name carries a schema and is left alone.
#[must_use]
pub fn has_server_side_web_search(req: &CreateMessageRequest) -> bool {
    req.tools.as_ref().is_some_and(|tools| {
        tools
            .iter()
            .any(|t| t.is_server_tool(WEB_SEARCH_TOOL_NAME, "web_search_"))
    })
}

/// Whether this request must take the proxy-native `web_search` pipeline.
/// Guarding the whole integration with this predicate is what makes
/// `enabled: false` a strict no-op.
#[must_use]
pub fn active(config: &WebSearchConfig, req: &CreateMessageRequest) -> bool {
    config.enabled && has_server_side_web_search(req)
}

/// Rewrite the outbound request so the executor searches through the proxy.
///
/// * removes the upstream's built-in `web_search` tool (the translator added
///   it) — otherwise the model would have two search tools;
/// * adds the [`EXECUTOR_TOOL_NAME`] function tool taking `queries[]`;
/// * forces `stream: false`: the executor must be re-run with the results,
///   so the chain is buffered (see `web_fetch` for the same reasoning).
///
/// Returns whether it activated. Called from `proxy::build_outbound`, which
/// both the first attempt and the `default_model` fallback go through.
pub fn prepare(
    req: &CreateMessageRequest,
    outbound: &mut ResponsesRequest,
    cfg: &WebSearchConfig,
) -> bool {
    if !active(cfg, req) {
        return false;
    }
    let tools = outbound.tools.get_or_insert_with(Vec::new);
    tools.retain(|t| !matches!(t, ToolDefinition::WebSearch(_)));
    if !tools
        .iter()
        .any(|t| matches!(t, ToolDefinition::Function(f) if f.name == EXECUTOR_TOOL_NAME))
    {
        tools.push(ToolDefinition::Function(executor_tool(cfg)));
    }
    outbound.stream = false;
    true
}

/// The function tool the executor sees.
fn executor_tool(cfg: &WebSearchConfig) -> ResponsesTool {
    let max = cfg.max_queries;
    ResponsesTool {
        kind: "function".to_owned(),
        name: EXECUTOR_TOOL_NAME.to_owned(),
        description: Some(format!(
            "Search the web (DuckDuckGo) and read the top result pages. Pass one or more focused \
             search queries (at most {max}); each is searched and the page text of its top results \
             is returned with source URLs. Use it for current information or anything you are \
             unsure of, and cite the real URLs you rely on."
        )),
        strict: true,
        parameters: json!({
            "type": "object",
            "properties": {
                "queries": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": format!("1 to {max} distinct search queries."),
                },
            },
            "required": ["queries"],
            "additionalProperties": false,
        }),
    }
}

#[cfg(test)]
mod tests;
