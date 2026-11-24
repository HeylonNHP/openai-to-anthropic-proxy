//! Advisor feature — detection-triggered second opinion.
//!
//! Anthropic's advisor is a *server-side* tool: Claude Code declares it, the
//! executor model calls it like any other function tool, and the server runs
//! a separate cheap model, feeds its advice back into the executor's prompt,
//! and emits `server_tool_use` + `advisor_tool_result` blocks in ONE assistant
//! message. This module implements that server side.
//!
//! ## Activation (the invariant that matters most)
//!
//! [`active`] is true only when `advisor.enabled` is set in `proxy.json` AND
//! the inbound request actually declares a tool named `advisor`. With the
//! shipped default (`enabled: false`) every request takes the pre-existing
//! pipeline unchanged: no extra upstream POST, no synthesized tool, no
//! `iterations` field, no forced buffering, no new log line.
//!
//! ## Design A — detection, not proactive review
//!
//! Consultations happen ONLY after the executor emits
//! `function_call{name:"advisor"}`, bounded by `AdvisorConfig::max_uses`
//! (unless `max_uses` is `0` = "no limit", which removes the bound
//! entirely — the proxy only WARNs about that, it does not cap it).
//! The executor's request is built with `stream: false`, which is how a
//! buffered client's whole chain runs. For a streaming client the shared loop
//! turns streaming back on per run and forwards each run as it is generated
//! (`crate::tools::server_loop::{live, stream}`, with the event builders in
//! [`sse`]); the advisor's own consultation is never streamed.
//!
//! ## Fail closed
//!
//! An advisor failure (404, timeout, decode error, empty advice, cap hit)
//! NEVER fails the client request: the executor's answer is still delivered,
//! with `error_code: "unavailable"` (or `max_uses_exceeded`). The advisor's
//! own POST bypasses `Attempt::send_success` entirely, so it can never burn
//! the executor's `default_model` fallback or turn the second opinion into a
//! self-review. At most one TUI warning is emitted per turn.
//!
//! ## One assistant message
//!
//! Each consultation contributes a contiguous `server_tool_use` /
//! `advisor_tool_result` pair with equal, non-empty ids; `stop_reason` is
//! `end_turn` (or `tool_use` iff a *client* tool is pending) — never
//! `pause_turn`. Streaming is one `message_start` … one `message_stop`.
//!
//! Modules:
//! - [`prepare`] — synthesize the zero-argument function tool.
//! - [`tool`] — the [`ServerTool`](crate::tools::server_loop::handler::ServerTool)
//!   the shared loop calls: one consultation per call, bounded by `max_uses`.
//! - [`protocol`] — pure wire-block / billing builders.
//! - [`sse`] — `Message` / content blocks → Anthropic SSE events.

use crate::anthropic::CreateMessageRequest;
use crate::config::{AdvisorConfig, Config};
use crate::responses::{ResponsesRequest, ResponsesTool, ToolDefinition};
use serde_json::json;

pub mod protocol;
pub mod sse;
pub mod tool;

/// The inbound name Claude Code uses for the advisor declaration. The
/// translator keys on exactly this string too (`strip_advisor_tool_instructions`
/// preserves the `# Advisor Tool` system section when it is present), so we
/// deliberately do not invent a second name here.
const ADVISOR_TOOL_NAME: &str = "advisor";

/// Tokens that must NEVER reach the client in an error body on the advisor
/// path: Claude Code reads them as "the server does not know the advisor
/// tool" and latches the feature off for the whole process. Applied
/// to every `AppError` body leaving `Attempt::run` on an advisor-active turn.
const FORBIDDEN_ERROR_TOKENS: [&str; 3] =
    ["advisor_20260301", "advisor-tool-2026-03-01", "Input tag"];

/// Replace every advisor-identifying token with a neutral marker.
#[must_use]
pub fn scrub_error_body(body: &str) -> String {
    let mut out = body.to_owned();
    for token in FORBIDDEN_ERROR_TOKENS {
        if out.contains(token) {
            out = out.replace(token, "[redacted]");
        }
    }
    out
}

/// Whether the client declared a tool named `advisor`, whatever its shape.
///
/// The translator uses this NAME-ONLY check to decide whether it may keep
/// the `# Advisor Tool` system section (an ordinary function tool with that
/// name is forwarded upstream and can be called). `build_outbound` also
/// uses it to detect the one case where the translator skipped its strip but
/// the advisor pipeline is NOT active: the schema-less Claude Code
/// declaration (B9).
#[must_use]
pub fn has_inbound_advisor_tool(req: &CreateMessageRequest) -> bool {
    req.tools
        .as_ref()
        .is_some_and(|tools| tools.iter().any(|t| t.name == ADVISOR_TOOL_NAME))
}

/// Whether the client declared the real Claude Code advisor tool: a tool
/// named `advisor` with NO `input_schema` (the three-field, schema-less
/// declaration). An ordinary function tool that happens to be called
/// `advisor` has a schema and must NOT activate this pipeline.
fn has_advisor_tool(req: &CreateMessageRequest) -> bool {
    req.tools.as_ref().is_some_and(|tools| {
        tools
            .iter()
            .any(|t| t.is_server_tool(ADVISOR_TOOL_NAME, "advisor_"))
    })
}

/// Whether this request must take the advisor pipeline. Guarding the whole
/// integration with this predicate is what makes `enabled: false` a strict
/// no-op for every non-advisor request.
pub fn active(config: &Config, req: &CreateMessageRequest) -> bool {
    active_cfg(&config.advisor, req)
}

/// Same predicate with only the advisor settings in scope (testable without
/// building a whole resolved [`Config`]).
fn active_cfg(cfg: &AdvisorConfig, req: &CreateMessageRequest) -> bool {
    cfg.enabled && has_advisor_tool(req)
}

/// Synthesize the ordinary Responses function tool Claude Code's three-field
/// advisor declaration needs, and force buffering for the consultation chain.
///
/// This lives here rather than in `src/translate.rs`'s `filter_map`: the
/// translator stays a pure inbound→outbound mapper, and its
/// `t.input_schema.clone()?` continues to drop the schema-less declaration
/// exactly as before. Returns `true` when the advisor path was prepared.
///
/// Called from `proxy::build_outbound`, which BOTH the first attempt and the
/// `default_model` fallback re-translation go through — so a fallback cannot
/// lose the tool or accidentally re-enable upstream streaming. Each call
/// starts from a freshly translated request, so the tool is never duplicated.
pub fn prepare(
    req: &CreateMessageRequest,
    outbound: &mut ResponsesRequest,
    cfg: &AdvisorConfig,
) -> bool {
    if !cfg.enabled || !has_advisor_tool(req) {
        return false;
    }

    // A function tool named `advisor` may already be present (e.g. an
    // ordinary client tool with a schema, or a previous prepare on the same
    // outbound). Never push a duplicate: OpenAI's strict validators reject
    // two tools with the same name.
    let tools = outbound.tools.get_or_insert_with(Vec::new);
    let already_present = tools.iter().any(|t| match t {
        ToolDefinition::Function(f) => f.name == ADVISOR_TOOL_NAME,
        ToolDefinition::WebSearch(_) => false,
    });
    if !already_present {
        tools.push(ToolDefinition::Function(ResponsesTool {
            kind: "function".to_owned(),
            name: ADVISOR_TOOL_NAME.to_owned(),
            description: Some(
                "Consult a reviewer model before substantive work. Takes no arguments.".to_owned(),
            ),
            strict: true,
            parameters: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false,
            }),
        }));
    }

    // The default for the chain is buffered (a buffered client gets the
    // whole turn at once). A streaming client's loop sets `stream` per run
    // (`server_loop::run::drive`), because `prepare` runs again on a
    // `default_model` fallback and would otherwise reset it.
    outbound.stream = false;
    true
}

#[cfg(test)]
mod tests;
