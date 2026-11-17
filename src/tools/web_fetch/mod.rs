//! Anthropic's server-side `web_fetch` tool, implemented by the proxy.
//!
//! ## What this is
//!
//! `web_fetch` is an Anthropic **server** tool: the client declares it in
//! `tools[]` with `"type": "web_fetch_20250910"` and the SERVER does the
//! HTTP fetch, then feeds the extracted page text back to the model as a
//! `web_fetch_tool_result` block. Claude Code never runs the fetch
//! itself.
//!
//! The upstream this proxy talks to is an OpenAI-compatible Responses API,
//! which has no `web_fetch` server tool of its own (it has `web_search`,
//! which the proxy already maps). So the proxy has to BE the server: it
//! performs the fetch itself.
//!
//! ## How a fetch happens
//!
//! Anthropic's `web_fetch` is a *server* tool, so the client declares it
//! without an `input_schema` — exactly like the advisor's three-field
//! declaration. The proxy recognises that shape, synthesises an ordinary
//! Responses **function** tool named `web_fetch` (taking a `url`
//! argument), and lets the executor model call it. When the executor
//! calls it, the proxy fetches the URL, extracts the readable text, and
//! feeds it back as a `function_call_output` so the executor can continue
//! with the page content in context.
//!
//! ## The two paths
//!
//! * **`prepare`** (inbound): synthesises the function tool and forces
//!   `stream: false` on the outbound request, because a fetch means the
//!   executor must be RE-RUN with the page text before the turn can end.
//!   The whole chain is buffered and, for a streaming client, converted to
//!   a synthetic Anthropic SSE sequence — the same shape the advisor uses.
//! * **`session`** (outbound): the loop that runs the executor, intercepts
//!   its `web_fetch` call, performs the fetch, injects the result, and
//!   re-runs the executor.
//!
//! ## Activation
//!
//! The feature is **off by default** (`web_fetch.enabled` in
//! `proxy.json`) and only ever activates when the client actually
//! declares the server-side tool. With it off, every request takes the
//! pre-existing pipeline unchanged: no fetch, no synthesized tool, no
//! forced buffering, no new log line.
//!
//! ## Safety
//!
//! The proxy fetches URLs on the model's behalf, so the fetch layer is
//! hardened against SSRF: only `http`/`https`, no credentials in the URL,
//! every resolved IP checked against the private/reserved ranges, and
//! every redirect hop re-validated. See [`fetch`].

use crate::anthropic::CreateMessageRequest;
use crate::config::WebFetchConfig;
use crate::responses::{ResponsesRequest, ResponsesTool, ToolDefinition};
use serde_json::json;

pub mod extract;
pub mod fetch;
pub mod protocol;
pub mod session;

/// The tool name Anthropic uses for the server-side `web_fetch` tool.
///
/// The client declares the tool with this name (the `type` field varies
/// by version: `web_fetch_20250910`, `web_fetch_20260209`,
/// `web_fetch_20260309`, `web_fetch_20260318`), so the NAME is the stable
/// discriminator. The proxy keys on it exactly like the advisor keys on
/// `"advisor"`.
pub const WEB_FETCH_TOOL_NAME: &str = "web_fetch";

/// The versioned `type` values Anthropic accepts for the server tool.
///
/// The proxy does not distinguish between them: the behaviour it
/// implements (fetch, extract, return the text) is identical for every
/// version, and the client is the one that sends the version. This list
/// exists so the module can DOCUMENT what it accepts and so a test can
/// assert the proxy does not care.
pub const WEB_FETCH_TOOL_TYPES: &[&str] = &[
    "web_fetch_20250910",
    "web_fetch_20260209",
    "web_fetch_20260309",
    "web_fetch_20260318",
];

/// Whether the client declared the SERVER-side `web_fetch` tool.
///
/// The discriminator is the Anthropic server-tool shape: a tool named
/// `web_fetch` with **no** `input_schema`. A client function tool that
/// happens to be named `web_fetch` (with a schema) is an ordinary tool
/// and must NOT activate this pipeline — it would be forwarded upstream
/// like any other function tool.
///
/// This mirrors the advisor's `has_advisor_tool` exactly (§16.1 of
/// `ADVISOR_TOOL_RESEARCH.md`).
#[must_use]
pub fn has_server_side_web_fetch(req: &CreateMessageRequest) -> bool {
    req.tools.as_ref().is_some_and(|tools| {
        tools
            .iter()
            .any(|t| t.name == WEB_FETCH_TOOL_NAME && t.input_schema.is_none())
    })
}

/// Whether this request must take the `web_fetch` pipeline.
///
/// Guarding the whole integration with this predicate is what makes
/// `enabled: false` a strict no-op for every non-`web_fetch` request.
#[must_use]
pub fn active(config: &WebFetchConfig, req: &CreateMessageRequest) -> bool {
    config.enabled && has_server_side_web_fetch(req)
}

/// Synthesise the Responses function tool the executor model calls to
/// request a fetch, and force the outbound request to be buffered.
///
/// The executor must be re-run with the fetched text before the turn can
/// end, so the whole chain is buffered (§11.3 of
/// `ADVISOR_TOOL_RESEARCH.md`): a streaming client gets ONE synthetic
/// Anthropic SSE sequence at the end, produced by
/// [`crate::tools::advisor::sse::message_to_events`].
///
/// Called from `proxy::build_outbound`, which BOTH the first attempt and
/// the `default_model` fallback re-translation go through — so a fallback
/// cannot lose the tool or accidentally re-enable upstream streaming.
/// Each call starts from a freshly translated request, so the tool is
/// never duplicated.
pub fn prepare(
    req: &CreateMessageRequest,
    outbound: &mut ResponsesRequest,
    cfg: &WebFetchConfig,
) -> bool {
    if !cfg.enabled || !has_server_side_web_fetch(req) {
        return false;
    }

    // A function tool named `web_fetch` may already be present (e.g. a
    // client tool with a schema, or a previous prepare on the same
    // outbound). Never push a duplicate: OpenAI's strict validators
    // reject two tools with the same name.
    let tools = outbound.tools.get_or_insert_with(Vec::new);
    let already_present = tools.iter().any(|t| match t {
        ToolDefinition::Function(f) => f.name == WEB_FETCH_TOOL_NAME,
        ToolDefinition::WebSearch(_) => false,
    });
    if !already_present {
        tools.push(ToolDefinition::Function(ResponsesTool {
            kind: "function".to_owned(),
            name: WEB_FETCH_TOOL_NAME.to_owned(),
            description: Some(
                "Fetch a web page over HTTP(S) and return its readable text content. \
                 Takes the URL to fetch as a single 'url' argument."
                    .to_owned(),
            ),
            strict: true,
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "The absolute http:// or https:// URL to fetch.",
                    },
                },
                "required": ["url"],
                "additionalProperties": false,
            }),
        }));
    }

    // The executor must be re-run with the fetched text, so the whole
    // chain is buffered. Splicing a second upstream stream into
    // TranslatorStream is deliberately NOT attempted.
    outbound.stream = false;
    true
}

#[cfg(test)]
mod tests;
