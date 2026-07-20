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
//! sends them): vision/image blocks, extended thinking, prompt caching,
//! server tools, container reuse, service tier, output_config.

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
    /// Unknown block types round-trip as raw JSON; the translator drops them
    /// when forwarding to OpenAI (with a tracing warning).
    #[serde(other)]
    Unknown,
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
    pub input_schema: Value,
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
    /// Thinking blocks (extended thinking). Not generated by the proxy
    /// but accepted in case we ever round-trip them.
    Thinking {
        thinking: String,
        signature: String,
    },
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
