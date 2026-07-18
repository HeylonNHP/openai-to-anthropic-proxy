//! OpenAI Chat Completions API types — **outbound** to the upstream.
//!
//! Reference: <https://platform.openai.com/docs/api-reference/chat>
//!
//! This module models the *minimum* set the proxy needs:
//! - Non-streaming request + response (with `tool_calls`).
//! - Streaming chunk with `delta.tool_calls` and `usage`.
//! - Error envelope.
//!
//! The `stream` field is omitted from the request type — the proxy always
//! sends `stream: true` or `stream: false` explicitly per the request, so
//! the field lives in the request DTO the proxy builds, not in the upstream
//! type. We add it at translation time.

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ─── Request (outbound) ────────────────────────────────────────────────────

/// OpenAI Chat Completions request body.
#[derive(Debug, Clone, Serialize)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    /// Token cap. OpenAI renamed `max_tokens` to `max_completion_tokens`
    /// for the gpt-5 / o-series; older models accept either. The proxy
    /// always sends `max_completion_tokens` because the gpt-5 family
    /// rejects the legacy field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDef>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoiceValue>,
    /// Always set explicitly by the translator — either `true` or `false`.
    pub stream: bool,
    /// Only set when streaming. Asks the upstream to emit a final usage chunk.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
    /// Anthropic `user` metadata → OpenAI `user`. We only set it if the
    /// client supplied `metadata.user_id`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Some upstreams (notably airia-backed reasoning models) reject
    /// function tools when this is unset — they fall back to a default
    /// reasoning effort that is incompatible with tool use. Pass
    /// `"none"` to disable reasoning for tool-use requests. `None`
    /// means omit the field entirely; the upstream chooses its own
    /// default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StreamOptions {
    pub include_usage: bool,
}

/// A message in the OpenAI chat format. Tagged on `role`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum ChatMessage {
    System {
        content: String,
    },
    User {
        content: UserContent,
    },
    Assistant {
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_calls: Option<Vec<ToolCall>>,
    },
    Tool {
        content: String,
        tool_call_id: String,
    },
    /// OpenAI has a `developer` role (newer); we accept and treat as system.
    Developer {
        content: String,
    },
}

/// OpenAI user-side content is either a plain string or an array of parts.
/// The proxy only emits string content — it does not forward images.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UserContent {
    Text(String),
    Parts(Vec<Value>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    /// Always `"function"`. `String` rather than `&'static str` so this
    /// type can derive `Deserialize` (the input isn't `'static`).
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    /// Arguments as a JSON-encoded string. Per the OpenAI spec this is a
    /// string even when the function takes a structured object.
    pub arguments: String,
}

// ─── Tools & tool choice (request) ─────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct ToolDef {
    /// Always `"function"`. Built by the translator, so `String` is fine.
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionDef,
}

#[derive(Debug, Clone, Serialize)]
pub struct FunctionDef {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema. Anthropic's `input_schema` is already JSON Schema, so
    /// we pass it through.
    pub parameters: Value,
}

/// `tool_choice`: string, `"none"`, or a structured form. Tagged on
/// `type` for the structured case; the string case deserializes as
/// `None`/`"auto"`.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum ToolChoiceValue {
    Simple(String), // "none" | "auto" | "required"
    Function {
        #[serde(rename = "type")]
        kind: &'static str, // "function"
        function: ToolChoiceFunction,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolChoiceFunction {
    pub name: String,
}

// ─── Response (non-streaming) ──────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct ChatCompletionResponse {
    pub id: String,
    /// `"chat.completion"` for non-streaming.
    pub object: String,
    pub created: i64,
    pub model: String,
    pub choices: Vec<Choice>,
    #[serde(default)]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Choice {
    pub index: u32,
    pub message: ResponseMessage,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResponseMessage {
    pub role: String,
    /// String for plain replies, `null` when only `tool_calls` is set.
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<ToolCall>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens_details: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_tokens_details: Option<Value>,
}

// ─── Response (streaming) ──────────────────────────────────────────────────

/// One OpenAI streaming chunk. `object` is `"chat.completion.chunk"`.
#[derive(Debug, Clone, Deserialize)]
pub struct ChatCompletionChunk {
    pub id: String,
    pub object: String,
    pub created: i64,
    pub model: String,
    pub choices: Vec<ChunkChoice>,
    /// Present only when `stream_options.include_usage: true` was sent
    /// AND the chunk is the terminal usage chunk.
    #[serde(default)]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChunkChoice {
    pub index: u32,
    pub delta: ChunkDelta,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ChunkDelta {
    /// `assistant` on the very first chunk; `None` afterwards.
    #[serde(default)]
    pub role: Option<String>,
    /// Text fragment. May be `null` even on text-bearing chunks (OpenAI
    /// sometimes emits `""`); we treat both as no-op.
    #[serde(default)]
    pub content: Option<String>,
    /// Tool-call deltas. Each entry can have any combination of `index`,
    /// `id`, `type`, `function.name`, `function.arguments`. The proxy
    /// accumulates `arguments` per index.
    #[serde(default)]
    pub tool_calls: Option<Vec<ChunkToolCall>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChunkToolCall {
    pub index: u32,
    /// `tool_call_id` of the call, present only on the first delta for
    /// a given `index`.
    #[serde(default)]
    pub id: Option<String>,
    /// `"function"` on first delta; absent afterwards.
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub function: Option<ChunkFunction>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ChunkFunction {
    /// Tool name, present only on the first delta for a given index.
    #[serde(default)]
    pub name: Option<String>,
    /// Partial JSON-encoded argument string. Concatenate per index.
    #[serde(default)]
    pub arguments: Option<String>,
}

// ─── Error envelope (response) ─────────────────────────────────────────────

/// OpenAI error response body. Returned by the upstream; the proxy
/// translates this into an Anthropic error before sending to the client.
#[derive(Debug, Clone, Deserialize)]
pub struct ErrorEnvelope {
    pub error: ErrorBody,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ErrorBody {
    pub message: String,
    /// e.g. `invalid_request_error`, `authentication_error`, `rate_limit_error`.
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub param: Option<Value>,
    #[serde(default)]
    pub code: Option<Value>,
}
