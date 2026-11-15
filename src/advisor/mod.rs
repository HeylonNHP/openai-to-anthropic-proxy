//! Advisor feature — detection-triggered second opinion (§§3, 11, 17 of
//! `ADVISOR_TOOL_RESEARCH.md`).
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
//! `function_call{name:"advisor"}`, bounded by `AdvisorConfig::max_uses`.
//! The executor's request is forced to `stream: false` (§11.3): the whole
//! chain is buffered and, if the client asked for streaming, converted to a
//! synthetic Anthropic SSE sequence by [`sse::message_to_events`].
//! `src/stream.rs` is untouched.
//!
//! ## Fail closed
//!
//! An advisor failure (404, timeout, decode error, empty advice, cap hit)
//! NEVER fails the client request: the executor's answer is still delivered,
//! with `error_code: "unavailable"` (or `max_uses_exceeded`). The advisor's
//! own POST bypasses `Attempt::send_success` entirely, so it can never burn
//! the executor's `default_model` fallback or turn the second opinion into a
//! self-review (§11.4). At most one TUI warning is emitted per turn.
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
//! - [`session`] — the bounded consultation loop and advice injection.
//! - [`protocol`] — pure wire-block / billing builders.
//! - [`sse`] — buffered `Message` → synthetic SSE events.

use crate::anthropic::CreateMessageRequest;
use crate::config::{AdvisorConfig, Config};
use crate::responses::{ResponsesRequest, ResponsesTool, ToolDefinition};
use serde_json::json;

pub mod protocol;
pub mod session;
pub mod sse;

/// The inbound name Claude Code uses for the advisor declaration. The
/// translator keys on exactly this string too (`strip_advisor_tool_instructions`
/// preserves the `# Advisor Tool` system section when it is present), so we
/// deliberately do not invent a second name here.
const ADVISOR_TOOL_NAME: &str = "advisor";

/// Whether the client declared the (three-field, schema-less) advisor tool.
fn has_advisor_tool(req: &CreateMessageRequest) -> bool {
    req.tools
        .as_ref()
        .is_some_and(|tools| tools.iter().any(|t| t.name == ADVISOR_TOOL_NAME))
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

    outbound
        .tools
        .get_or_insert_with(Vec::new)
        .push(ToolDefinition::Function(ResponsesTool {
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

    // §11.3: run the whole consultation chain through the existing buffered
    // pipeline, then synthesize SSE for a streaming client. Splicing a second
    // upstream stream into TranslatorStream is deliberately NOT attempted.
    outbound.stream = false;
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anthropic::Tool;
    use crate::config::AdvisorConfig;
    use crate::responses::Input;
    use serde_json::{Value, json};

    fn req_with_advisor_decl() -> CreateMessageRequest {
        serde_json::from_value(json!({
            "model": "gpt-4o",
            "max_tokens": 32,
            "stream": true,
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"type": "advisor_20260301", "name": "advisor", "model": "beam"}]
        }))
        .unwrap()
    }

    fn empty_outbound() -> ResponsesRequest {
        ResponsesRequest {
            model: "gpt-4o".into(),
            input: Input::Items(Vec::new()),
            instructions: None,
            tools: None,
            tool_choice: None,
            reasoning: None,
            text: None,
            temperature: None,
            top_p: None,
            max_output_tokens: Some(32),
            parallel_tool_calls: None,
            store: None,
            previous_response_id: None,
            user: None,
            stream: true,
            metadata: None,
            prompt_cache_key: None,
            prompt_cache_options: None,
        }
    }

    #[test]
    fn disabled_config_prepares_nothing() {
        let req = req_with_advisor_decl();
        let mut outbound = empty_outbound();
        let before = serde_json::to_value(&outbound).unwrap();
        assert!(!prepare(&req, &mut outbound, &AdvisorConfig::default()));
        assert_eq!(serde_json::to_value(&outbound).unwrap(), before);
    }

    #[test]
    fn no_declaration_prepares_nothing_even_when_enabled() {
        let mut req = req_with_advisor_decl();
        req.tools = None;
        let mut outbound = empty_outbound();
        let before = serde_json::to_value(&outbound).unwrap();
        let cfg = AdvisorConfig {
            enabled: true,
            ..AdvisorConfig::default()
        };
        assert!(!prepare(&req, &mut outbound, &cfg));
        assert_eq!(serde_json::to_value(&outbound).unwrap(), before);
    }

    #[test]
    fn prepare_adds_one_zero_argument_tool_and_forces_buffering() {
        let req = req_with_advisor_decl();
        let mut outbound = empty_outbound();
        let cfg = AdvisorConfig {
            enabled: true,
            ..AdvisorConfig::default()
        };
        assert!(prepare(&req, &mut outbound, &cfg));
        assert!(!outbound.stream);
        let tools = outbound.tools.unwrap();
        assert_eq!(tools.len(), 1);
        let v: Value = serde_json::to_value(&tools[0]).unwrap();
        assert_eq!(
            v,
            json!({
                "type": "function",
                "name": "advisor",
                "description": "Consult a reviewer model before substantive work. Takes no arguments.",
                "strict": true,
                "parameters": {
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }
            })
        );
    }

    #[test]
    fn prepare_appends_to_existing_tools() {
        let mut req = req_with_advisor_decl();
        req.tools.as_mut().unwrap().push(Tool {
            name: "Bash".into(),
            description: Some("run a command".into()),
            input_schema: Some(json!({"type": "object", "properties": {}})),
        });
        let mut outbound = empty_outbound();
        outbound.tools = Some(vec![ToolDefinition::Function(ResponsesTool {
            kind: "function".into(),
            name: "Bash".into(),
            description: None,
            strict: true,
            parameters: json!({"type": "object", "properties": {}}),
        })]);
        let cfg = AdvisorConfig {
            enabled: true,
            ..AdvisorConfig::default()
        };
        assert!(prepare(&req, &mut outbound, &cfg));
        let tools = outbound.tools.unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(serde_json::to_value(&tools[0]).unwrap()["name"], "Bash");
        assert_eq!(serde_json::to_value(&tools[1]).unwrap()["name"], "advisor");
    }

    #[test]
    fn active_requires_both_the_flag_and_the_declaration() {
        let req = req_with_advisor_decl();
        assert!(
            !active_cfg(&AdvisorConfig::default(), &req),
            "default config is off"
        );
        let cfg = AdvisorConfig {
            enabled: true,
            ..AdvisorConfig::default()
        };
        assert!(active_cfg(&cfg, &req));
        let mut plain = req_with_advisor_decl();
        plain.tools = None;
        assert!(!active_cfg(&cfg, &plain));
    }
}
