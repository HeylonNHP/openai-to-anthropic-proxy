//! Anthropic Messages API types — **inbound** to the proxy.
//!
//! Claude Code sends requests in this format; we parse it, translate to
//! OpenAI, and translate the response back.
//!
//! Reference: <https://platform.claude.com/docs/en/api/messages>
//!
//! ## Scope
//!
//! Covered: text content, tool use, tool results, image content, system
//! prompt, stop reasons,
//! usage, and the full streaming SSE event set.
//!
//! Image blocks (`image`) are parsed and forwarded to the upstream as
//! `input_image` parts: user messages always, `tool_result` blocks per the
//! `tool_result_images` config (see `proxy.json-guide.md`). `image` blocks in
//! assistant turns have no counterpart in the Responses API and are dropped
//! with a warning.
//!
//! `document` blocks are parsed too ([`DocumentBlockParam`]): text sources go
//! upstream as text, PDFs as `input_file` only when `forward_documents` is on,
//! otherwise as a short note.
//!
//! Request fields this module does not declare (`container`,
//! `context_management`, `mcp_servers`, `service_tier`, `speed`, ...) are
//! **silently ignored**: [`CreateMessageRequest`] has no
//! `deny_unknown_fields`, so they neither fail the request nor are
//! forwarded. Fields that are declared but not acted on (`top_k`,
//! `thinking.budget_tokens`, `stop_sequences`) are parsed and then dropped
//! in translation. See `REFERENCE.md` for the full out-of-scope list.
//!
//! Prompt caching and the `web_search` / `web_fetch` / `advisor` server tools
//! **are** handled (see `translate.rs` and `tools/`). So are
//! `output_config.effort` and `thinking.type = "disabled"` (see
//! [`OutputConfig`] / [`ThinkingConfig`]); they drive the upstream
//! reasoning-effort selection.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

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
    /// A PDF or other attachment (`document` block). See [`DocumentBlockParam`].
    Document(DocumentBlockParam),
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
    /// Claude Code echoes the advisor's `server_tool_use` block verbatim on
    /// the next request. It used to land in `Unknown` and be dropped,
    /// taking the paired advice with it. Parse it explicitly so the pair can
    /// be replayed as prior context instead of as an orphan `function_call`.
    ServerToolUse(ServerToolUseBlockParam),
    /// The advisor's paired result block. `content` is a 3-way union, kept
    /// opaque: `advisor_result{text}`, `advisor_redacted_result{
    /// encrypted_content}`, `advisor_tool_result_error{error_code}`.
    AdvisorToolResult(AdvisorToolResultBlockParam),
    /// The `web_fetch` paired result block, echoed by Claude Code on the
    /// next turn: `{"type":"web_fetch_tool_result","tool_use_id":"…",
    /// "content":{…}}`.
    ///
    /// `content` is kept opaque (it is a nested union), but the fields
    /// the translator needs — the URL and the extracted text — are also
    /// flattened into named fields so the replay does not have to dig
    /// through the union.
    WebFetchToolResult(WebFetchToolResultBlockParam),
    /// The `web_search` paired result block, echoed by the client on the next
    /// turn. Its `content` is the list of results (each with an opaque
    /// `encrypted_content`) or an error object; kept opaque here and read by
    /// `tools::web_search::history`.
    WebSearchToolResult(WebSearchToolResultBlockParam),
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

/// Anthropic `server_tool_use` block as echoed back by Claude Code:
/// `{"type":"server_tool_use","id":"srvtoolu_…","name":"advisor","input":{}}`.
///
/// Every field is `#[serde(default)]` and the two text fields are tolerant of
/// a wrong JSON type: a matched tag whose payload fails to deserialize is a
/// hard error that would turn a previously tolerated block into a 400 for the
/// whole request (`#[serde(other)]` only catches UNMATCHED tags). `extra`
/// keeps `cache_control`, `caller` and any future fields so the block can be
/// round-tripped verbatim.
#[derive(Debug, Clone, Deserialize)]
pub struct ServerToolUseBlockParam {
    /// `srvtoolu_…` id; the paired result block carries it as `tool_use_id`.
    #[serde(default, deserialize_with = "tolerant_text")]
    pub id: String,
    /// Server-side tool name. Only `advisor` is understood by the translator.
    #[serde(default, deserialize_with = "tolerant_text")]
    pub name: String,
    /// Tool input. The advisor tool takes no arguments (`{}`); kept as an
    /// opaque `Value` so any future server tool shape still parses.
    #[serde(default)]
    pub input: Value,
    /// All unrecognised fields, including Anthropic's `cache_control` and
    /// `caller`, preserved for verbatim round-tripping.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Anthropic `advisor_tool_result` block as echoed back by Claude Code:
/// `{"type":"advisor_tool_result","tool_use_id":"srvtoolu_…","content":{…}}`.
///
/// `content` is deliberately an opaque `Value` — it is a 3-way union and only
/// `advisor_result{text}` carries plaintext advice. Defaults keep a partial
/// or future-shaped block from failing the entire request.
#[derive(Debug, Clone, Deserialize)]
pub struct AdvisorToolResultBlockParam {
    /// Id of the preceding `server_tool_use` block.
    #[serde(default, deserialize_with = "tolerant_text")]
    pub tool_use_id: String,
    /// Opaque result union: `advisor_result{text}`,
    /// `advisor_redacted_result{encrypted_content}`, or
    /// `advisor_tool_result_error{error_code}` (plus future fields).
    #[serde(default)]
    pub content: Value,
    /// All unrecognised fields, preserved for verbatim round-tripping.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Anthropic `web_fetch_tool_result` block as echoed back by Claude Code.
///
/// The full block is:
/// ```json
/// {
///   "type": "web_fetch_tool_result",
///   "tool_use_id": "srvtoolu_…",
///   "content": {
///     "type": "web_fetch_result",
///     "url": "https://example.com/article",
///     "content": {
///       "type": "document",
///       "source": { "type": "text", "media_type": "text/plain", "data": "…" }
///     },
///     "retrieved_at": "2025-08-25T10:30:00Z"
///   }
/// }
/// ```
///
/// The whole `content` is kept opaque, and the two fields the translator
/// needs (the URL and the text) are dug out of it by
/// `translate::web_fetch_result_fields`. Every field is defaulted, so a
/// partial or future-shaped block never fails the request.
#[derive(Debug, Clone, Deserialize)]
pub struct WebFetchToolResultBlockParam {
    /// Id of the preceding `server_tool_use` block.
    #[serde(default, deserialize_with = "tolerant_text")]
    pub tool_use_id: String,
    /// The opaque `web_fetch_result` union.
    #[serde(default)]
    pub content: Value,
    /// All unrecognised fields, preserved for verbatim round-tripping.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// A `web_search_tool_result` the client echoes back.
#[derive(Debug, Clone, Deserialize)]
pub struct WebSearchToolResultBlockParam {
    /// Id of the preceding `server_tool_use` block.
    #[serde(default, deserialize_with = "tolerant_text")]
    pub tool_use_id: String,
    /// A list of `web_search_result` or a `web_search_tool_result_error`.
    #[serde(default)]
    pub content: Value,
    /// All unrecognised fields, preserved for verbatim round-tripping.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
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

/// Image input (base64 `data:` URIs or passthrough URLs).
///
/// Translated to an `input_image` part upstream: always for user messages,
/// and for `tool_result` blocks per the `tool_result_images` setting
/// (`array` by default). In assistant turns the blocks are dropped, because
/// the Responses API has no assistant-image content part.
#[derive(Debug, Clone, Deserialize)]
pub struct ImageBlockParam {
    pub source: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

/// A `document` block: a PDF, a plain-text file or a nested content list.
///
/// `source` is kept opaque and judged by the translator: `text` and `content`
/// sources become text, `base64` and `url` sources are sent upstream as a file
/// only when `forward_documents` is on, and anything else (such as a Files API
/// `file` reference) is replaced by a note. Every field is defaulted and
/// tolerant of a wrong JSON type, so a malformed block can never fail the
/// whole request the way a matched tag with a bad payload would.
#[derive(Debug, Clone, Deserialize)]
pub struct DocumentBlockParam {
    #[serde(default)]
    pub source: Value,
    #[serde(default, deserialize_with = "tolerant_text")]
    pub title: String,
    #[serde(default, deserialize_with = "tolerant_text")]
    pub context: String,
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

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Tool {
    pub name: String,
    /// The versioned tool type (`web_search_20250305`, `web_fetch_20250910`,
    /// `advisor_20260301`, or `custom` for a client function tool). Absent on
    /// older clients' function tools.
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub input_schema: Option<Value>,
    /// The client's `strict` flag on a function tool. Only an explicit
    /// `false` is acted on: the proxy sends every other tool strict (see
    /// `translate`), because that is what the upstream needs.
    #[serde(default)]
    pub strict: Option<bool>,
    /// Options a client sets on a server tool. Always default for a client
    /// function tool.
    #[serde(flatten)]
    pub options: ServerToolOptions,
}

/// The options Anthropic's server tools accept in their declaration.
///
/// Every field is optional, and a field a given tool does not define is
/// simply left unset: `web_search` takes `max_uses`, `allowed_domains`,
/// `blocked_domains` and `user_location`; `web_fetch` takes `max_uses`,
/// `allowed_domains`, `blocked_domains`, `max_content_tokens` and
/// `citations`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ServerToolOptions {
    /// Most uses of the tool the client allows in one request.
    #[serde(default)]
    pub max_uses: Option<u32>,
    /// Only these domains may be searched or fetched.
    #[serde(default)]
    pub allowed_domains: Option<Vec<String>>,
    /// These domains must not be searched or fetched.
    #[serde(default)]
    pub blocked_domains: Option<Vec<String>>,
    /// The user's approximate location, used to localise search results.
    #[serde(default)]
    pub user_location: Option<Value>,
    /// Most page content (in tokens) `web_fetch` may return.
    #[serde(default)]
    pub max_content_tokens: Option<u32>,
    /// `web_fetch` citation settings, passed through as sent.
    #[serde(default)]
    pub citations: Option<Value>,
}

impl Tool {
    /// Whether this is the SERVER-side tool called `name`, whose versioned
    /// type starts with `type_prefix` (e.g. `"web_search_"`).
    ///
    /// Anthropic server tools never carry an `input_schema`, so the
    /// schema-less declaration is recognised on its name alone (that is the
    /// shape Claude Code sends). A versioned `type` marks one even when a
    /// schema is present. A client function tool that merely shares the name
    /// has a schema and a `custom` (or absent) type, so it is left alone.
    #[must_use]
    pub fn is_server_tool(&self, name: &str, type_prefix: &str) -> bool {
        self.name == name
            && (self.input_schema.is_none()
                || self
                    .kind
                    .as_deref()
                    .is_some_and(|kind| kind.starts_with(type_prefix)))
    }
}

impl CreateMessageRequest {
    /// The options the client set on the server tool called `name` (its
    /// versioned type starts with `type_prefix`); all unset when the client
    /// did not declare it.
    #[must_use]
    pub fn server_tool_options(&self, name: &str, type_prefix: &str) -> ServerToolOptions {
        self.tools
            .iter()
            .flatten()
            .find(|t| t.is_server_tool(name, type_prefix))
            .map(|t| t.options.clone())
            .unwrap_or_default()
    }
}

/// Reject a request Anthropic itself would reject: a search or fetch tool
/// that sets both `allowed_domains` and `blocked_domains`.
///
/// # Errors
///
/// A client-facing message naming the offending tool.
pub fn validate_server_tool_options(tools: &[Tool]) -> Result<(), String> {
    for tool in tools {
        let is_domain_tool = tool.is_server_tool("web_search", "web_search_")
            || tool.is_server_tool("web_fetch", "web_fetch_");
        if is_domain_tool
            && tool.options.allowed_domains.is_some()
            && tool.options.blocked_domains.is_some()
        {
            return Err(format!(
                "tools: `{}` cannot set both `allowed_domains` and `blocked_domains`",
                tool.name
            ));
        }
    }
    Ok(())
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
    /// Anthropic block needs no new variant here.
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
    /// The model declined to answer, whether a safety classifier stopped
    /// it or it declined itself. The client must not run the turn's tools.
    Refusal,
    /// Streaming-only: the model paused mid-turn (rare, round-trip only).
    PauseTurn,
}

/// Server tool usage tracking: how many searches and fetches were made.
/// A count of zero is omitted from the wire, so a request that used only one
/// of the tools reports only that one.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServerToolUseUsage {
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub web_search_requests: u32,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub web_fetch_requests: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
    /// model natively. See [`ResponseContentBlock::Raw`]; `untagged` emits
    /// the `Value` verbatim.
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

/// A single model entry in the Anthropic `GET /v1/models` response.
///
/// The CLI uses `id` when it echoes the model back in `model: ...` fields and
/// `display_name` for the human-facing `/model` picker.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AnthropicModelInfo {
    pub r#type: &'static str,
    pub id: String,
    pub display_name: String,
    pub created_at: String,
}

/// `GET /v1/models` response shape (Anthropic's List Models endpoint).
#[derive(Debug, Clone, serde::Serialize)]
pub struct AnthropicModelsList {
    pub data: Vec<AnthropicModelInfo>,
    pub has_more: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_id: Option<String>,
}

#[cfg(test)]
mod tests;
