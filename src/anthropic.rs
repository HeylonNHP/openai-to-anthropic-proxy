//! Anthropic Messages API types — **inbound** to the proxy.
//!
//! Claude Code sends requests in this format; we parse it, translate to
//! OpenAI, and translate the response back.
//!
//! Reference: <https://platform.claude.com/docs/en/api/messages>
//!
//! ## Scope
//!
//! Covered: text content, tool use, tool results, system prompt, stop reasons,
//! usage, and the full streaming SSE event set.
//!
//! Out of scope (will produce a `serde` deserialization error if Claude
//! sends them): vision/image blocks, extended thinking budget enforcement,
//! prompt caching, server tools, container reuse, service tier. Note that
//! `output_config.effort` and `thinking.type = "disabled"` **are** parsed
//! (see [`OutputConfig`] / [`ThinkingConfig`]); they drive the upstream
//! reasoning-effort selection rather than being rejected.

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ─── Requests (inbound) ─────────────────────────────────────────────────────

/// `POST /v1/messages` request body. This is what Claude Code sends.
#[derive(Debug, Clone, Deserialize)]
pub struct CreateMessageRequest {
    pub model: String,
    pub max_tokens: u32,
    pub messages: Vec<MessageParam>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<SystemPrompt>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_sequences: Option<Vec<String>>,

    /// `true` for SSE streaming. The proxy branches on this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,

    /// Request-level output configuration (e.g. Claude's adaptive
    /// effort control). Carries the effort value the client selected
    /// (`low`/`medium`/`high`/`xhigh`/`max`). The proxy translates
    /// this into the upstream's `reasoning.effort` via the configured
    /// effort map; it is not forwarded verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_config: Option<OutputConfig>,

    /// Thinking configuration. The proxy specifically cares about
    /// `type: "disabled"`, which takes precedence over
    /// `output_config.effort` and maps to the upstream's no-reasoning
    /// representation. Adaptive/enabled modes are preserved as state
    /// but do not override an explicit effort selection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingConfig>,
}

/// Claude's request-level `output_config` object.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct OutputConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

/// Claude's request-level `thinking` object.
///
/// Kept intentionally loose (string `type`) so newer/unknown modes
/// deserialize without breaking the whole request; only
/// `type == "disabled"` changes proxy behavior.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ThinkingConfig {
    pub r#type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<u32>,
}

/// System prompt — string or list of text blocks.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum SystemPrompt {
    Text(String),
    Blocks(Vec<TextBlockParam>),
}

#[derive(Debug, Clone, Deserialize)]
pub struct Metadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
}

/// One entry in `messages`. Role must be `user` or `assistant` (system is
/// conveyed via the top-level `system` field).
#[derive(Debug, Clone, Deserialize)]
pub struct MessageParam {
    pub role: MessageRole,
    pub content: MessageContent,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    User,
    Assistant,
    /// `system` is accepted for round-tripping; the proxy folds it into the
    /// top-level system prompt.
    System,
}

/// Message content — string or array of blocks.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Blocks(Vec<ContentBlockParam>),
}

// ─── Request content blocks (inbound) ──────────────────────────────────────

/// Request-side content block. Tagged by `type`.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlockParam {
    Text(TextBlockParam),
    Image(ImageBlockParam),
    /// Tool invocation produced by the model in a prior turn.
    ToolUse(ToolUseBlockParam),
    /// Tool result returned to the model in response to a prior `tool_use`.
    ToolResult(ToolResultBlockParam),
    /// Extended-thinking block produced by the model in a prior turn.
    ///
    /// Claude Code echoes these back verbatim on the next request. The proxy
    /// must NOT drop them: `thinking` carries the model's prior reasoning
    /// text and `signature` carries the upstream's encrypted chain-of-thought.
    /// Dropping the block destroys the model's memory of its own reasoning —
    /// see `append_assistant_message` for how the block is replayed upstream.
    Thinking(ThinkingBlockParam),
    /// Redacted extended-thinking block: `{"type":"redacted_thinking","data":"…"}`.
    /// `data` is an opaque Anthropic-encrypted blob with no readable text.
    RedactedThinking(RedactedThinkingBlockParam),
    /// Unknown block types round-trip as raw JSON; the translator drops them
    /// when forwarding to OpenAI (with a tracing warning).
    #[serde(other)]
    Unknown,
}

/// Anthropic `thinking` block: `{"type":"thinking","thinking":"…","signature":"…"}`.
///
/// Every field is `#[serde(default)]` so a block that omits `signature` (or
/// `thinking`) still parses. Serde's `#[serde(other)]` fallback only catches
/// *unmatched tags* — a matched tag with a missing required field is a hard
/// error, which would turn a tolerated block into a 400 for the whole request.
#[derive(Debug, Clone, Deserialize)]
pub struct ThinkingBlockParam {
    /// The model's reasoning text (a summary of its chain of thought).
    #[serde(default, deserialize_with = "tolerant_text")]
    pub thinking: String,
    /// Opaque, provider-issued blob. In this proxy it carries the upstream
    /// Responses API's `reasoning.encrypted_content`.
    #[serde(default, deserialize_with = "tolerant_text")]
    pub signature: String,
}

/// Anthropic `redacted_thinking` block: `{"type":"redacted_thinking","data":"…"}`.
///
/// `data` is an opaque Anthropic-encrypted blob with no readable text. It is
/// *not* the upstream's `encrypted_content`, so it cannot be replayed upstream
/// as one — the translator drops it with a warning.
#[derive(Debug, Clone, Deserialize)]
pub struct RedactedThinkingBlockParam {
    /// Opaque encrypted blob with no readable text.
    #[serde(default, deserialize_with = "tolerant_text")]
    pub data: String,
}

/// Deserialize a text field, tolerating a wrong JSON type.
///
/// Without this, a block like `{"type":"thinking","thinking":123}` aborts
/// the *entire* request with a 400: serde's `#[serde(other)]` fallback on
/// `ContentBlockParam` only fires for an unmatched **tag**, not for a matched
/// tag whose payload fails to deserialize. Before the thinking block was
/// recognised, such a block fell into `Unknown` and was merely dropped.
///
/// A number or an object is rendered as its JSON text so no information is
/// lost; `null` becomes the empty string.
fn tolerant_text<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(match value {
        serde_json::Value::String(s) => s,
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    })
}

#[derive(Debug, Clone, Deserialize)]
pub struct TextBlockParam {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

/// Image input — accepted for deserialization, but the proxy does not
/// forward images to OpenAI (out of scope for MVP).
#[derive(Debug, Clone, Deserialize)]
pub struct ImageBlockParam {
    pub source: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CacheControl {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ToolUseBlockParam {
    pub id: String,
    pub name: String,
    pub input: Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ToolResultBlockParam {
    pub tool_use_id: String,
    /// String or array of blocks. Most callers send a string.
    pub content: Option<ToolResultContent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ToolResultContent {
    Text(String),
    Blocks(Vec<Value>),
}

// ─── Tools & tool choice (request) ─────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct Tool {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub input_schema: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolChoice {
    Auto {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        disable_parallel_tool_use: Option<bool>,
    },
    Any {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        disable_parallel_tool_use: Option<bool>,
    },
    Tool {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        disable_parallel_tool_use: Option<bool>,
    },
    None {},
}

// ─── Response (outbound to Claude Code) ────────────────────────────────────

/// Full Anthropic-shaped message response. The proxy emits one of these
/// from `translate::response::openai_to_anthropic`.
#[derive(Debug, Clone, Serialize)]
pub struct Message {
    pub id: String,
    /// Always `"message"`.
    pub r#type: &'static str,
    pub role: &'static str,
    pub content: Vec<ResponseContentBlock>,
    pub model: String,
    pub stop_reason: Option<StopReason>,
    pub stop_sequence: Option<String>,
    pub usage: Usage,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    /// Thinking content block. Surfaced when the upstream (Responses API)
    /// emits a `reasoning` output item. `thinking` carries the summary
    /// text (concatenated from `response.reasoning_summary_text.delta`
    /// events) and `signature` carries the upstream's `encrypted_content`
    /// blob — opaque to the client but required by the wire format.
    Thinking {
        thinking: String,
        signature: String,
    },
    /// Server-side tool use (e.g. web_search). Injected when the
    /// upstream used a built-in web_search tool so Claude Code's
    /// search counter works.
    ServerToolUse {
        id: String,
        name: String,
        input: Value,
    },
    /// Web search results block. Follows a `ServerToolUse` block.
    WebSearchToolResult {
        #[serde(flatten)]
        block: WebSearchToolResultBlock,
    },
    /// Generic escape hatch for block types the proxy does not model
    /// natively (e.g. advisor `server_tool_use` / `advisor_tool_result`).
    /// `untagged` on an internally-tagged `Serialize` enum emits the
    /// `Value` verbatim, including its own `type` field — so a future
    /// Anthropic block needs no new variant here. See §11.6 of
    /// `ADVISOR_TOOL_RESEARCH.md`.
    #[serde(untagged)]
    Raw(Value),
}

/// A single web search result within a `WebSearchToolResult` block.
#[derive(Debug, Clone, Serialize)]
pub struct WebSearchResult {
    /// NOTE: the field must be `url`, not `uri`. Claude Code's parser
    /// reads `c.url` from each entry of a `web_search_tool_result`
    /// content array; the `uri` spelling (used in some Vertex/Google
    /// docs) is silently dropped client-side, rendering every result
    /// with an empty URL.
    pub url: String,
    pub title: String,
    /// Opaque encrypted content. We don't have the upstream's
    /// encrypted blob, so this is empty — the citation URL and
    /// title are what Claude Code uses for display.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub encrypted_content: String,
}

/// A `web_search_tool_result` content block from Anthropic's server-side web search.
///
/// We inject a synthetic one when the upstream (OpenAI) uses its built-in
/// `web_search` tool so Claude Code's search counter works.
#[derive(Debug, Clone, Serialize)]
pub struct WebSearchToolResultBlock {
    pub tool_use_id: String,
    pub content: Vec<WebSearchResult>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    StopSequence,
    ToolUse,
    /// Anthropic occasionally emits a refusal; we propagate as-is.
    Refusal,
    /// Streaming-only: the model paused mid-turn (rare, round-trip only).
    PauseTurn,
}

/// Server tool usage tracking. `web_search_requests` counts how many
/// web searches were performed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerToolUseUsage {
    pub web_search_requests: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub cache_creation_input_tokens: u32,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub cache_read_input_tokens: u32,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub cache_creation_input_tokens_5m: u32,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub cache_creation_input_tokens_1h: u32,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub thinking_tokens: u32,
    /// Server tool usage (e.g. web search request count).
    /// Only present when a server tool was used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_tool_use: Option<ServerToolUseUsage>,
    /// Per-iteration sampling usage (`{"type":"message",...}` /
    /// `{"type":"advisor_message",...}`). Anthropic reports advisor
    /// tokens here rather than in the top-level totals so the client can
    /// bill each iteration at its own model's rate. Omitted when absent so
    /// every existing response is byte-for-byte unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iterations: Option<Vec<Value>>,
}

const fn is_zero_u32(n: &u32) -> bool {
    *n == 0
}

// ─── Error envelope (response) ─────────────────────────────────────────────

/// Anthropic-shaped error. Returned to Claude Code when the proxy or
/// upstream fails.
#[derive(Debug, Clone, Serialize)]
pub struct ErrorResponse {
    pub r#type: &'static str,
    pub error: ApiErrorBody,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApiErrorBody {
    /// One of: `invalid_request_error`, `authentication_error`,
    /// `permission_error`, `not_found_error`, `request_too_large`,
    /// `rate_limit_error`, `api_error`, `overloaded_error`.
    pub r#type: String,
    pub message: String,
}

impl ErrorResponse {
    /// Convenience constructor.
    pub fn new(kind: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            r#type: "error",
            error: ApiErrorBody {
                r#type: kind.into(),
                message: message.into(),
            },
        }
    }
}

// ─── Streaming SSE events (response) ───────────────────────────────────────

/// One Anthropic streaming event. We tag on the `type` field exactly as the
/// upstream does; SSE framing (`event:` / `data:`) is added by the proxy
/// when it writes to the client.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    MessageStart {
        message: Message,
    },
    ContentBlockStart {
        index: u32,
        content_block: ContentBlockKind,
    },
    ContentBlockDelta {
        index: u32,
        delta: ContentDelta,
    },
    ContentBlockStop {
        index: u32,
    },
    MessageDelta {
        delta: MessageDeltaPayload,
        #[serde(skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
    },
    MessageStop {},
    Ping {},
    Error {
        error: ApiErrorBody,
    },
}

/// A content block as it appears in `content_block_start`. Holds the block
/// shell; deltas follow.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlockKind {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    /// Thinking blocks (extended thinking). The proxy generates these
    /// from the upstream's `reasoning` output item — in streaming mode in
    /// `crate::stream`, and in non-streaming mode in `crate::translate`.
    Thinking {
        thinking: String,
        signature: String,
    },
    /// Server-side tool use (e.g. web_search).
    ServerToolUse {
        id: String,
        name: String,
        input: Value,
    },
    /// Web search results block.
    WebSearchToolResult {
        #[serde(flatten)]
        block: WebSearchToolResultBlock,
    },
    /// Generic escape hatch for content-block shells the proxy does not
    /// model natively. See [`ResponseContentBlock::Raw`] and §11.6 of
    /// `ADVISOR_TOOL_RESEARCH.md`; `untagged` emits the `Value` verbatim.
    #[serde(untagged)]
    Raw(Value),
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentDelta {
    TextDelta { text: String },
    InputJsonDelta { partial_json: String },
    ThinkingDelta { thinking: String },
    SignatureDelta { signature: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct MessageDeltaPayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<StopReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_sequence: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(body: &str) -> CreateMessageRequest {
        serde_json::from_str(body).expect("parse CreateMessageRequest")
    }

    #[test]
    fn parses_output_config_effort() {
        let req = parse(
            r#"{
                "model": "claude-sonnet-5",
                "max_tokens": 100,
                "messages": [{"role": "user", "content": "hi"}],
                "output_config": {"effort": "high"}
            }"#,
        );
        assert_eq!(
            req.output_config.as_ref().and_then(|o| o.effort.as_deref()),
            Some("high")
        );
        assert!(req.thinking.is_none());
    }

    #[test]
    fn parses_thinking_disabled() {
        let req = parse(
            r#"{
                "model": "claude-sonnet-5",
                "max_tokens": 100,
                "messages": [{"role": "user", "content": "hi"}],
                "thinking": {"type": "disabled"}
            }"#,
        );
        let t = req.thinking.as_ref().expect("thinking present");
        assert_eq!(t.r#type, "disabled");
        assert!(req.output_config.is_none());
    }

    #[test]
    fn output_config_and_thinking_absent_by_default() {
        let req = parse(
            r#"{
                "model": "claude-sonnet-5",
                "max_tokens": 100,
                "messages": [{"role": "user", "content": "hi"}]
            }"#,
        );
        assert!(req.output_config.is_none());
        assert!(req.thinking.is_none());
    }

    #[test]
    fn unknown_output_config_fields_are_ignored() {
        // Forward compatibility: Claude may add future keys to
        // output_config. They must not break deserialization.
        let req = parse(
            r#"{
                "model": "claude-sonnet-5",
                "max_tokens": 100,
                "messages": [{"role": "user", "content": "hi"}],
                "output_config": {"effort": "max", "future_field": 1}
            }"#,
        );
        assert_eq!(
            req.output_config.as_ref().and_then(|o| o.effort.as_deref()),
            Some("max")
        );
    }

    #[test]
    fn unknown_thinking_types_are_ignored() {
        let req = parse(
            r#"{
                "model": "claude-sonnet-5",
                "max_tokens": 100,
                "messages": [{"role": "user", "content": "hi"}],
                "thinking": {"type": "adaptive"}
            }"#,
        );
        assert_eq!(
            req.thinking.as_ref().map(|t| t.r#type.as_str()),
            Some("adaptive")
        );
    }

    // ── Thinking blocks (BUG 1) ────────────────────────────────────────────
    //
    // The proxy used to have no `Thinking` variant, so every echoed
    // thinking block deserialized to `Unknown` and was silently dropped.

    fn parse_blocks(body: &str) -> Vec<ContentBlockParam> {
        let req = parse(body);
        match req
            .messages
            .into_iter()
            .next()
            .expect("one message")
            .content
        {
            MessageContent::Blocks(b) => b,
            MessageContent::Text(_) => panic!("expected blocks"),
        }
    }

    #[test]
    fn parses_thinking_block() {
        let blocks = parse_blocks(
            r#"{
                "model": "claude-sonnet-5",
                "max_tokens": 100,
                "messages": [{"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "step 1", "signature": "SIG"}
                ]}]
            }"#,
        );
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            ContentBlockParam::Thinking(t) => {
                assert_eq!(t.thinking, "step 1");
                assert_eq!(t.signature, "SIG");
            }
            other => panic!("expected a Thinking block, got {other:?}"),
        }
    }

    #[test]
    fn parses_redacted_thinking_block() {
        let blocks = parse_blocks(
            r#"{
                "model": "claude-sonnet-5",
                "max_tokens": 100,
                "messages": [{"role": "assistant", "content": [
                    {"type": "redacted_thinking", "data": "BLOB"}
                ]}]
            }"#,
        );
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            ContentBlockParam::RedactedThinking(rt) => assert_eq!(rt.data, "BLOB"),
            other => panic!("expected a RedactedThinking block, got {other:?}"),
        }
    }

    #[test]
    fn thinking_block_tolerates_missing_fields() {
        // Serde's `#[serde(other)]` fallback only catches UNMATCHED tags. A
        // matched tag with a missing required field is a hard error, which
        // would turn a tolerated block into a 400 for the whole request —
        // so every field of the new payload structs must be optional.
        let blocks = parse_blocks(
            r#"{
                "model": "claude-sonnet-5",
                "max_tokens": 100,
                "messages": [{"role": "assistant", "content": [
                    {"type": "thinking"}
                ]}]
            }"#,
        );
        match &blocks[0] {
            ContentBlockParam::Thinking(t) => {
                assert!(t.thinking.is_empty());
                assert!(t.signature.is_empty());
            }
            other => panic!("expected a Thinking block, got {other:?}"),
        }

        let blocks = parse_blocks(
            r#"{
                "model": "claude-sonnet-5",
                "max_tokens": 100,
                "messages": [{"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "only text"}
                ]}]
            }"#,
        );
        match &blocks[0] {
            ContentBlockParam::Thinking(t) => {
                assert_eq!(t.thinking, "only text");
                assert!(t.signature.is_empty());
            }
            other => panic!("expected a Thinking block, got {other:?}"),
        }
    }

    // ── Generic wire extension points (§11.6) ────────────────────────

    #[test]
    fn raw_response_content_block_serializes_verbatim() {
        // An `untagged` Raw variant on this internally-tagged enum must
        // emit the Value byte-for-byte, including its own `type` field —
        // no per-feature enum variants needed for advisor blocks.
        let raw: Value = serde_json::from_str(
            r#"{"type":"advisor_tool_result","tool_use_id":"srvtoolu_1",
                "content":{"type":"advisor_result","text":"advice"}}"#,
        )
        .unwrap();
        let block = ResponseContentBlock::Raw(raw.clone());
        assert_eq!(serde_json::to_value(&block).unwrap(), raw);
        // The native variants must be unaffected: they still carry their
        // snake_case tag from `#[serde(tag = "type")]`.
        assert_eq!(
            serde_json::to_value(ResponseContentBlock::Text { text: "hi".into() }).unwrap(),
            serde_json::json!({"type": "text", "text": "hi"})
        );
    }

    #[test]
    fn raw_content_block_kind_serializes_verbatim() {
        let raw = serde_json::json!({
            "type": "server_tool_use",
            "id": "srvtoolu_2",
            "name": "advisor",
            "input": {}
        });
        assert_eq!(
            serde_json::to_value(ContentBlockKind::Raw(raw.clone())).unwrap(),
            raw
        );
        assert_eq!(
            serde_json::to_value(ContentBlockKind::Text {
                text: String::new(),
            })
            .unwrap(),
            serde_json::json!({"type": "text", "text": ""})
        );
    }

    fn plain_usage() -> Usage {
        Usage {
            input_tokens: 7,
            output_tokens: 3,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens_5m: 0,
            cache_creation_input_tokens_1h: 0,
            thinking_tokens: 0,
            server_tool_use: None,
            iterations: None,
        }
    }

    #[test]
    fn usage_iterations_none_is_omitted() {
        // Zero wire change for every existing response.
        let value = serde_json::to_value(plain_usage()).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"input_tokens": 7, "output_tokens": 3})
        );
        assert!(!value.as_object().unwrap().contains_key("iterations"));
    }

    #[test]
    fn usage_iterations_some_is_forwarded_as_an_array() {
        let mut usage = plain_usage();
        let iterations = vec![serde_json::json!({
            "type": "advisor_message", "model": "beam",
            "input_tokens": 4, "output_tokens": 2
        })];
        usage.iterations = Some(iterations.clone());
        let value = serde_json::to_value(usage).unwrap();
        assert_eq!(value["iterations"], serde_json::Value::Array(iterations));
    }

    #[test]
    fn unknown_block_type_still_maps_to_unknown() {
        // Guards `#[serde(other)] Unknown` remaining the LAST variant.
        let blocks = parse_blocks(
            r#"{
                "model": "claude-sonnet-5",
                "max_tokens": 100,
                "messages": [{"role": "assistant", "content": [
                    {"type": "server_tool_use", "id": "srvtool_1"}
                ]}]
            }"#,
        );
        assert_eq!(blocks.len(), 1);
        assert!(matches!(blocks[0], ContentBlockParam::Unknown));
    }
}
