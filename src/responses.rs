//! OpenAI Responses API types — **outbound** to the upstream.
//!
//! Reference: <https://platform.openai.com/docs/api-reference/responses>
//!
//! This module models the *minimum* set the proxy needs:
//! - Non-streaming request + response (with `output[]` items).
//! - Streaming typed SSE events (response.created, response.output_item.added,
//!   response.output_text.delta, response.function_call_arguments.delta,
//!   response.completed, ...).
//! - Error envelope.
//!
//! The Responses API is a different shape than Chat Completions. Notable
//! differences:
//! - `input` is a typed list of items (messages, function_call,
//!   function_call_output) rather than a list of role-tagged messages.
//! - `tools` require `strict: true` + `additionalProperties: false` on the
//!   JSON Schema, else airia returns 400 `invalid_function_parameters`.
//! - `reasoning` is a nested object (`{ effort: "..." }`) instead of a
//!   top-level `reasoning_effort` string.
//! - The model returns `output[]` items that may include a `reasoning`
//!   item with `encrypted_content` (the model's chain-of-thought) — this
//!   is the field we surface to clients as a Thinking content block.

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ─── Request (outbound) ────────────────────────────────────────────────────

/// OpenAI Responses request body.
#[derive(Debug, Clone, Serialize)]
pub struct ResponsesRequest {
    pub model: String,
    pub input: Input,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ResponsesTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    /// `reasoning: { effort: "..." }` — replaces the legacy top-level
    /// `reasoning_effort` field from Chat Completions. The Responses API
    /// also accepts `summary` and `context` keys, but the proxy only sends
    /// `effort` (which is what the airia gateway inspects).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<TextConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    /// Token cap. Renamed from `max_tokens` to `max_output_tokens` in
    /// the Responses API.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    /// We always pass the full `input` array; `store` is left unset so
    /// the upstream doesn't try to maintain server-side state.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store: Option<bool>,
    /// We don't use server-side state — always leave this unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_response_id: Option<String>,
    /// Anthropic `metadata.user_id` → OpenAI `user`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Always set explicitly by the translator.
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

/// `input` is either a single text string (single-turn shortcut) or a
/// typed list of items. We always emit `Items` because Claude Code's
/// requests are multi-turn.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum Input {
    Text(String),
    Items(Vec<InputItem>),
}

/// One item in the `input` list. Internally tagged on `type`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputItem {
    /// A user / assistant / system message. `role` is one of
    /// `"user" | "assistant" | "system" | "developer"`.
    Message {
        role: String,
        content: Vec<InputContentPart>,
    },
    /// A function call emitted by the assistant in a prior turn.
    /// `arguments` is a JSON-encoded string (NOT a parsed object).
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    /// The result of a function call, fed back to the model. `output` is
    /// always a string.
    FunctionCallOutput { call_id: String, output: String },
    /// Reference to a previously-stored response item. We don't generate
    /// these but we accept them on deserialization for round-tripping.
    ItemReference { id: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputContentPart {
    /// Used in `role: "user"` / `"system"` / `"developer"` messages.
    InputText { text: String },
    /// Used in `role: "assistant"` messages. The Responses API
    /// distinguishes assistant content from user content; sending
    /// `input_text` on an assistant message yields
    /// `Invalid value: 'input_text'. Supported values are: 'output_text' and 'refusal'`.
    OutputText { text: String },
    /// Image content part. The `url` is a data URI
    /// (`data:{media_type};base64,{data}`) or a public HTTP/HTTPS URL.
    /// `detail` is optional (`"low" | "high" | "auto"`).
    ///
    /// NOTE: In the Responses API, `image_url` is a **string** (the URL),
    /// and `detail` is a **sibling field** at the same level as `type`
    /// and `image_url` — NOT nested inside `image_url`.
    InputImage {
        image_url: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    /// Round-trip only; not generated by the proxy.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReasoningConfig {
    /// One of: `"none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"`.
    /// The proxy doesn't enforce the set — it forwards whatever the
    /// operator wrote. The Responses API accepts `"none"` natively.
    pub effort: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TextConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verbosity: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResponsesTool {
    /// Always `"function"`.
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Strict mode is required for the airia gateway — without it,
    /// `gpt-5.6-luna` returns 400 `invalid_function_parameters`. The
    /// translator sets this unconditionally; it cannot be disabled.
    pub strict: bool,
    pub parameters: Value,
}

/// `tool_choice`: a string (`"auto" | "required" | "none"`) or a
/// `{ type: "function", name: "..." }` object. Untagged so serde
/// disambiguates by shape.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum ToolChoice {
    Simple(String), // "none" | "auto" | "required"
    Function {
        #[serde(rename = "type")]
        kind: String, // "function"
        name: String,
    },
}

// ─── Response (non-streaming) ──────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct ResponsesResponse {
    pub id: String,
    /// Always `"response"`.
    pub object: String,
    pub created_at: i64,
    /// `"completed" | "incomplete" | "failed" | "in_progress" | "queued" | "cancelled"`.
    pub status: String,
    pub model: String,
    pub output: Vec<OutputItem>,
    #[serde(default)]
    pub usage: Option<ResponsesUsage>,
    #[serde(default)]
    pub error: Option<ResponsesError>,
    #[serde(default)]
    pub incomplete_details: Option<IncompleteDetails>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputItem {
    Message {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        status: Option<String>,
        /// Always `"assistant"`.
        role: String,
        content: Vec<OutputContentPart>,
    },
    FunctionCall {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        status: Option<String>,
        call_id: String,
        name: String,
        /// JSON-encoded string of the arguments.
        arguments: String,
    },
    Reasoning {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        summary: Vec<ReasoningSummaryText>,
        /// The model's encrypted chain-of-thought. We surface this
        /// as the `signature` field on the Anthropic Thinking block.
        #[serde(default)]
        encrypted_content: String,
    },
    /// Round-trip only; we don't currently generate this.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputContentPart {
    OutputText {
        text: String,
        #[serde(default)]
        annotations: Vec<Value>,
    },
    OutputRefusal {
        refusal: String,
    },
    SummaryText {
        text: String,
    },
    /// Round-trip only.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReasoningSummaryText {
    #[serde(default)]
    pub text: String,
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResponsesUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub total_tokens: u32,
    #[serde(default)]
    pub input_tokens_details: Option<InputTokensDetails>,
    #[serde(default)]
    pub output_tokens_details: Option<OutputTokensDetails>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InputTokensDetails {
    #[serde(default)]
    pub cached_tokens: u32,
    #[serde(default)]
    pub cache_write_tokens: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OutputTokensDetails {
    /// Reasoning tokens spent. The proxy surfaces this as
    /// `usage.thinking_tokens` on the Anthropic side.
    #[serde(default)]
    pub reasoning_tokens: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IncompleteDetails {
    /// `"max_output_tokens" | "content_filter"`.
    pub reason: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResponsesError {
    pub message: String,
    /// e.g. `invalid_request_error`, `server_error`, `rate_limit_error`.
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub code: Option<Value>,
    #[serde(default)]
    pub param: Option<Value>,
}

// ─── Streaming SSE events (response) ───────────────────────────────────────

/// Wire envelope for one Responses SSE event. The `type` field is the
/// `event:` line from the SSE; the `data` payload follows.
///
/// We model only the subset the proxy actually consumes. Unknown event
/// types are deserialized as `Unknown` and dropped.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponsesStreamEvent {
    #[serde(rename = "response.created")]
    Created { response: ResponsesResponse },
    #[serde(rename = "response.in_progress")]
    InProgress { response: ResponsesResponse },
    #[serde(rename = "response.output_item.added")]
    OutputItemAdded { output_index: u32, item: OutputItem },
    #[serde(rename = "response.output_item.done")]
    OutputItemDone { output_index: u32, item: OutputItem },
    #[serde(rename = "response.content_part.added")]
    ContentPartAdded {
        item_id: String,
        output_index: u32,
        content_index: u32,
        part: OutputContentPart,
    },
    #[serde(rename = "response.content_part.done")]
    ContentPartDone {
        item_id: String,
        output_index: u32,
        content_index: u32,
        part: OutputContentPart,
    },
    #[serde(rename = "response.output_text.delta")]
    OutputTextDelta {
        item_id: String,
        output_index: u32,
        content_index: u32,
        delta: String,
    },
    #[serde(rename = "response.output_text.done")]
    OutputTextDone {
        item_id: String,
        output_index: u32,
        content_index: u32,
        text: String,
    },
    #[serde(rename = "response.function_call_arguments.delta")]
    FunctionCallArgumentsDelta {
        item_id: String,
        output_index: u32,
        delta: String,
    },
    #[serde(rename = "response.function_call_arguments.done")]
    FunctionCallArgumentsDone {
        item_id: String,
        output_index: u32,
        arguments: String,
    },
    #[serde(rename = "response.reasoning_summary_text.delta")]
    ReasoningSummaryTextDelta {
        item_id: String,
        output_index: u32,
        summary_index: u32,
        delta: String,
    },
    #[serde(rename = "response.reasoning_summary_text.done")]
    ReasoningSummaryTextDone {
        item_id: String,
        output_index: u32,
        summary_index: u32,
        text: String,
    },
    #[serde(rename = "response.refusal.delta")]
    RefusalDelta {
        item_id: String,
        output_index: u32,
        content_index: u32,
        delta: String,
    },
    #[serde(rename = "response.refusal.done")]
    RefusalDone {
        item_id: String,
        output_index: u32,
        content_index: u32,
        refusal: String,
    },
    #[serde(rename = "response.output_text.annotation.added")]
    OutputTextAnnotationAdded {
        item_id: String,
        output_index: u32,
        content_index: u32,
        annotation_index: u32,
        annotation: Value,
    },
    #[serde(rename = "response.completed")]
    Completed { response: ResponsesResponse },
    #[serde(rename = "response.incomplete")]
    Incomplete { response: ResponsesResponse },
    #[serde(rename = "response.failed")]
    Failed { response: ResponsesResponse },
    /// Top-level `error` event (not nested in a response.failed).
    /// Some upstreams emit this when the request itself errors out.
    #[serde(rename = "error")]
    Error {
        #[serde(default)]
        code: Option<String>,
        message: String,
        #[serde(default)]
        param: Option<String>,
    },
    /// Anything else — round-trip only.
    #[serde(other)]
    Unknown,
}
