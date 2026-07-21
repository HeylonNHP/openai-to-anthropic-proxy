//! Translation between Anthropic Messages API and OpenAI Responses.
//!
//! Three entry points:
//! - [`anthropic_to_responses`] — inbound request, used by the proxy's
//!   `/v1/messages` handler. Converts Claude Code's request into the
//!   shape the upstream expects.
//! - [`responses_to_anthropic`] — non-streaming response. The upstream
//!   returned a full `ResponsesResponse`; we map it to Anthropic's
//!   `Message` and ship it back to the client.
//! - [`StreamTranslator`] (in `crate::stream`) — turns an OpenAI
//!   Responses SSE event stream into Anthropic SSE events.
//!
//! Out-of-scope fields (vision, image inputs, structured outputs,
//! server-side state via `previous_response_id`) are silently dropped on
//! the request side. On the response side, OpenAI's `usage.input_tokens`
//! becomes Anthropic's `input_tokens`, `output_tokens` becomes
//! `output_tokens`, and `output_tokens_details.reasoning_tokens` becomes
//! `thinking_tokens`; cache fields are 0.

use crate::anthropic::{
    CacheControl, ContentBlockParam, CreateMessageRequest, ImageBlockParam, Message,
    MessageContent, MessageParam, MessageRole, ResponseContentBlock, ServerToolUseUsage,
    StopReason, SystemPrompt, ToolChoice, ToolResultContent, Usage as AnthropicUsage,
    WebSearchResult, WebSearchToolResultBlock,
};
use crate::config::PromptCachingConfig;
use crate::responses::{
    Input, InputContentPart, InputItem, PromptCacheBreakpoint, ReasoningConfig, ResponsesRequest,
    ResponsesResponse, ResponsesTool, ToolChoice as ResponsesToolChoice, ToolDefinition,
    WebSearchTool,
};
use anyhow::{Context, Result, anyhow};
use serde_json::Value;

/// Hard cap on the length of the `user` field we send to the upstream
/// OpenAI Responses API.
///
/// The Responses API rejects `user` values longer than 64 characters with
/// `400 string_above_max_length` (param: "user"). The Anthropic-side
/// `metadata.user_id` is unbounded — Claude Code emits a string of the
/// form `<session-uuid>_<account>_<workspace-path>`, which routinely
/// lands at 100–200 characters. We have to shorten it before forwarding.
///
/// **Why truncate rather than hash:**
/// - The `user` field is used by upstream for rate-limit bucketing and
///   abuse tracking. Hashing (e.g. SHA-256 of the user_id) would change
///   that bucket on every build of the proxy, breaking any per-user
///   observability we've already correlated against logs and metrics.
/// - Truncation preserves the leading prefix, which in Claude Code's
///   format is the session UUID — the most operationally meaningful
///   piece. A user looking at upstream logs can still tell sessions
///   apart.
/// - Truncation is reversible in the operator's head ("the user_id
///   started with `<this prefix>`...") for short requests, even if the
///   exact suffix is gone. Hashing is not reversible at all.
/// - It is the smallest change that fixes the 400. The original
///   `metadata_user_id_becomes_user` test (which expects 7 chars
///   unchanged) keeps passing with no other code changes.
///
/// **Why 64 (not e.g. 60) and not char-based truncation:**
/// - The 64 limit is the upstream's published max; we set the constant
///   to match exactly so a future upstream version that raises or
///   lowers it is a one-line change.
/// - We truncate on a byte boundary with `String::truncate`, not on a
///   char boundary with `char_indices`. A `user_id` from a non-ASCII
///   workspace path could have a multi-byte char straddling the cut;
///   `truncate` is a panic on a non-char boundary, which would convert
///   a 400 from upstream into a 500 from us. Byte-truncation can split
///   a UTF-8 sequence, producing technically-invalid UTF-8 in the
///   string, but the `user` field is opaque to the upstream (it's a
///   rate-limit key, not parsed content) so this is harmless in
///   practice. If a future maintainer wants strictly valid UTF-8,
///   switch to `s.char_indices().nth(USER_ID_MAX_LEN).map(|(i, _)| i).unwrap_or(s.len())`
///   — but note that this can let a single multi-byte char push the
///   effective length *above* 64, re-triggering the upstream 400.
///
/// If a future change wants to switch to hashing, replace
/// [`truncate_user_id`] with a SHA-256 (or similar) helper. The test
/// `user_id_over_limit_is_truncated` will need to be updated to assert
/// the hash format. Consider also keeping the original `user_id` in a
/// separate `X-Original-User` header for debugging — the proxy already
/// has a `LAST_SENT_BODY` task-local for request-level introspection
/// if a custom header is too intrusive.
const USER_ID_MAX_LEN: usize = 64;

fn truncate_user_id(s: String) -> String {
    if s.len() <= USER_ID_MAX_LEN {
        s
    } else {
        tracing::warn!(
            user_id_len = s.len(),
            max = USER_ID_MAX_LEN,
            "truncating metadata.user_id to fit Responses API user-field limit; \
             see USER_ID_MAX_LEN doc comment in src/translate.rs for rationale and \
             alternatives (e.g. SHA-256 hashing)",
        );
        let mut s = s;
        s.truncate(USER_ID_MAX_LEN);
        s
    }
}

/// Translate a Claude Code request into the upstream OpenAI Responses shape.
///
/// Translation rules:
/// - `system` (string or text blocks) → `instructions`.
/// - `messages[]` → `input: Items(...)`. Walk in order:
///   - `user` text → one `Message { role: "user", content: [InputTextPart] }`.
///   - `user` block content → one Message per run of text blocks; for each
///     `ToolResult`, emit a `FunctionCallOutput { call_id, output: <text> }`
///     *after* any pending text. Image blocks are dropped with a warn.
///   - `assistant` text → `Message { role: "assistant", content: [OutputTextPart { text }] }`.
///   - `assistant` text + `tool_use` → `Message { content: [OutputTextPart] }` followed by
///     `FunctionCall { call_id: id, name, arguments: json_string }` per tool use, in order.
///   - `assistant` only `tool_use` → same with empty `content: []`.
///   - `role: "system"` messages (legacy) → concatenated into `instructions`.
/// - `tools[]` → `tools[]` with `parameters` lifted from `input_schema`.
///   **The big behavior change:** every tool now carries
///   `strict: true` + `additionalProperties: false` on the parameters
///   schema, which is what the airia gateway requires for function tools
///   on `/v1/responses`.
/// - `tool_choice`: `Auto`→`"auto"`, `Any`→`"required"`, `None`→`"none"`,
///   `Tool{name}`→`{ type: "function", name: "..." }`.
/// - `max_tokens` → `max_output_tokens`.
/// - `temperature`, `top_p` pass through.
/// - `reasoning_effort` (config-supplied) → `reasoning: Some(ReasoningConfig { effort })`.
///   Responses accepts `"none"` natively, so we forward whatever the config
///   says — no per-request "downgrade" like the Chat Completions path did.
/// - `metadata.user_id` → `user`.
/// - `stream: true` → `stream: true` (no `stream_options`; Responses carries
///   usage on the terminal `Completed` event).
/// - `stop_sequences` dropped with a warn (Responses has no `stop` field).
pub fn anthropic_to_responses(
    req: &CreateMessageRequest,
    reasoning_effort: Option<String>,
    prompt_caching: &PromptCachingConfig,
) -> Result<ResponsesRequest> {
    let stream = req.stream.unwrap_or(false);
    let mut items: Vec<InputItem> = Vec::new();

    // Top-level `system` → `instructions`. Legacy `role: "system"`
    // messages are folded in below.
    let mut instructions = req.system.as_ref().map(system_text).unwrap_or_default();

    for msg in &req.messages {
        append_message_translation(&mut items, &mut instructions, msg, prompt_caching)?;
    }

    if instructions.is_empty() {
        instructions.clear();
    }

    let tools = req.tools.as_ref().map(|tools| {
        tools
            .iter()
            .filter_map(|t| {
                // Anthropic server-side web_search tool → OpenAI native web_search.
                // Server tools have no input_schema; we detect them by name.
                if t.name == "web_search" {
                    return Some(ToolDefinition::WebSearch(WebSearchTool {
                        kind: "web_search".to_string(),
                        max_uses: None,
                        search_context_size: Some("medium".to_string()),
                        user_location: None,
                        allowed_domains: None,
                        blocked_domains: None,
                    }));
                }

                let input_schema = t.input_schema.clone()?;
                Some(ToolDefinition::Function(ResponsesTool {
                    kind: "function".to_string(),
                    name: t.name.clone(),
                    description: t.description.clone(),
                    strict: true,
                    parameters: sanitize_tool_schema(input_schema),
                }))
            })
            .collect()
    });

    let tool_choice = req.tool_choice.as_ref().map(map_tool_choice);

    let user = req
        .metadata
        .as_ref()
        .and_then(|m| m.user_id.clone())
        .map(truncate_user_id);

    // Reasoning models don't support stop sequences. Responses has no
    // `stop` field at all; warn so the configuration loss is visible if
    // a future client sets it.
    if let Some(stop) = req.stop_sequences.as_ref()
        && !stop.is_empty()
    {
        tracing::warn!(
            stop_sequences = ?stop,
            "dropping stop_sequences; Responses API does not support a stop parameter"
        );
    }

    Ok(ResponsesRequest {
        model: req.model.clone(),
        input: Input::Items(items),
        instructions: if instructions.is_empty() {
            None
        } else {
            Some(instructions)
        },
        tools,
        tool_choice,
        reasoning: reasoning_effort.map(|effort| ReasoningConfig {
            effort,
            ..ReasoningConfig::default()
        }),
        text: None,
        temperature: req.temperature,
        top_p: req.top_p,
        max_output_tokens: Some(req.max_tokens),
        // Prompt-caching fields are set later by the caller if enabled.
        prompt_cache_key: None,
        prompt_cache_options: None,
        parallel_tool_calls: None,
        store: None,
        previous_response_id: None,
        user,
        stream,
        metadata: None,
    })
}

// ─── helpers ─────────────────────────────────────────────────────────────

fn system_text(system: &SystemPrompt) -> String {
    match system {
        SystemPrompt::Text(s) => s.clone(),
        SystemPrompt::Blocks(blocks) => blocks
            .iter()
            .map(|b| b.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n"),
    }
}

/// Make a tool's `parameters` schema acceptable to the strict Responses
/// API validator. The strict validator is fully recursive: it descends
/// into every `properties.*`, `items`, `additionalProperties` value,
/// `oneOf` / `anyOf` / `allOf` / `not` branch, and applies the same
/// `strict: true` rules at every depth. Earlier versions of this
/// function only ran the reconciliations at the top level of
/// `parameters` — that was wrong, and the validator caught us when
/// Claude Code's `AskUserQuestion` tool emitted
/// `properties.annotations.additionalProperties` (itself an object
/// schema) without a `required` array, yielding
/// `Missing 'notes'` at the `additionalProperties` level.
///
/// The function is now structured as a single recursive walk that
/// applies five reconciliations at every object schema it visits:
///
/// 1. Strip `additionalProperties: {}` (the empty-object form Claude
///    Code emits on tools like `ExitPlanMode`) — strict validators
///    refuse to compile it.
/// 2. Add `additionalProperties: false` if absent. The Responses API
///    requires this when `strict: true` is set, and the rule applies
///    to every nested object schema, not just the top-level one.
/// 3. Inject an empty `properties: {}` if missing — Responses
///    requires at least an empty object for `type: "object"` schemas
///    in strict mode.
/// 4. If `required` is absent, populate it from the keys of
///    `properties`. The strict validator enforces: every key in
///    `properties` must also appear in `required` (or the validator
///    returns `400 invalid_function_parameters: 'required' is required
///    to be supplied and to be an array including every key in
///    properties`).
/// 5. If `required` is present but is a *strict subset* of
///    `properties.keys()`, extend `required` with the missing keys
///    (preserving the client's original order, then appending missing
///    keys in `properties` iteration order). The Responses strict
///    validator is the source of truth: there is no valid use of a
///    `required` array that is a strict subset of `properties`.
/// 6. Drop the `format` keyword. The strict validator only accepts
///    the 9 IETF validation formats `date-time`, `time`, `date`,
///    `duration`, `email`, `hostname`, `ipv4`, `ipv6`, `uuid`; every
///    other format value is rejected with `'<value>' is not a valid
///    format` (live: Claude Code's `WebFetch` tool declares
///    `properties.url.format: "uri"`, which the validator rejects).
///    The model is the *producer* of these strings, not the consumer,
///    so format-based runtime validation isn't load-bearing in this
///    proxy — the schema's `description` field and the model's prompt
///    guide output shape, not `format` hints. If a future operator
///    needs to preserve the accepted formats, swap the unconditional
///    strip in `reconcile_strict` for a check against
///    `ALLOWED_FORMATS` (defined inline); the rest of the function
///    is unchanged.
/// 7. If a schema node is an object missing a `type` key, inject
///    `type: "string"`. The strict validator rejects every node that
///    lacks a `type` (live: Claude Code's `Workflow` tool declares
///    `properties.args: { description: "..." }` with no `type`,
///    producing `schema must have a 'type' key` at the `args` level).
///    We default to `string` because that's the one type the model
///    is guaranteed to produce without further schema shape — the
///    property's `description` and the model's prompt still convey
///    the intended shape (e.g. for `Workflow.args` the description
///    says "Pass arrays/objects as actual JSON values", which the
///    model can honor even though the schema type is `string`).
///    The right upstream fix is a schema that uses
///    `{"type": ["string", "number", "boolean", "array", "object"]}`
///    to express "any JSON value", but that's a Claude Code SDK
///    change we can't make. If a `type` is already present in any
///    form (string or array), we leave it alone.
///
/// The recursion visits in **post-order**: every child is fully
/// reconciled before its parent. That order matters because the
/// `required` reconciliation at the parent level inspects the
/// (already-reconciled) `properties` map.
///
/// The `propertyNames` strip is a separate pre-pass that runs once
/// over the whole tree, with a single aggregated warn for the count.
/// `propertyNames` is a JSON Schema 2019-09+ keyword for constraining
/// the *names* of properties, but the Responses strict validator
/// does not recognize it and rejects any schema that contains it
/// with `propertyNames is not permitted` (live: Claude Code's
/// `AskUserQuestion` tool emits `propertyNames` inside
/// `properties.annotations` and `properties.answers`, which the
/// validator flags as a misplaced keyword). If a future upstream
/// version starts honoring `propertyNames`, removing the strip is a
/// one-line change.
fn sanitize_tool_schema(mut schema: Value) -> Value {
    // Pre-pass: recursive propertyNames strip. Returns the total
    // number of keys removed so we can emit one warn for the whole
    // call rather than one per strip.
    let stripped = strip_property_names(&mut schema);
    if stripped > 0 {
        tracing::warn!(
            count = stripped,
            "stripping `propertyNames` keys from tool schema; the Responses strict \
             validator does not recognize propertyNames and rejects schemas that \
             contain it. See sanitize_tool_schema doc comment in src/translate.rs."
        );
    }

    reconcile_strict(&mut schema);
    schema
}

/// Recursively reconcile a JSON Schema value against the strict
/// Responses API rules. Post-order: children are fully reconciled
/// before the parent is mutated, so the parent's `required`
/// reconciliation observes the (already-reconciled) `properties` map.
///
/// `v` is expected to have been pre-cleaned of `propertyNames` by
/// [`strip_property_names`] before this is called.
fn reconcile_strict(v: &mut Value) {
    match v {
        Value::Object(map) => {
            // Reconcile this object schema FIRST for the
            // self-referential rules (additionalProperties, format,
            // type) before recursing into child schemas. This matters
            // specifically for the `additionalProperties: {}`
            // empty-object form: if we recursed first, rule 7
            // (type-injection) would turn the empty object into
            // `{"type": "string"}`, and the empty-strip rule would
            // then no longer match. By stripping the empty form
            // before the recursion, the empty `{}` is removed
            // cleanly, and the subsequent `additionalProperties: false`
            // add (rule 2) replaces it with the bool.
            if let Some(Value::Object(empty)) = map.get("additionalProperties")
                && empty.is_empty()
            {
                map.remove("additionalProperties");
            }
            if !map.contains_key("additionalProperties") && is_object_schema(map) {
                map.insert("additionalProperties".to_owned(), Value::Bool(false));
            }
            if !map.contains_key("properties") && is_object_schema(map) {
                map.insert("properties".to_owned(), Value::Object(Default::default()));
            }
            if let Some(f) = map.remove("format") {
                tracing::warn!(
                    format = ?f,
                    "stripping `format` keyword from tool schema; the Responses strict \
                     validator only accepts the 9 IETF validation formats (date-time, \
                     time, date, duration, email, hostname, ipv4, ipv6, uuid) and \
                     rejects every other value. See sanitize_tool_schema doc comment in \
                     src/translate.rs."
                );
            }
            if !map.contains_key("type") {
                tracing::warn!(
                    "schema node missing `type` key; defaulting to `string`. The \
                     Responses strict validator rejects type-less schemas. See \
                     sanitize_tool_schema doc comment in src/translate.rs."
                );
                map.insert("type".to_owned(), Value::String("string".into()));
            }

            // Now recurse into child schemas. We do this AFTER the
            // self-referential rules above so the empty-additional
            // strip and the false-add have already settled the
            // parent's `additionalProperties` form (we don't recurse
            // into a freshly-added `false`). The `required`
            // reconciliation runs last, on the post-recurse
            // properties, so the parent's required array is built
            // from the (now-typed) child keys.
            for keyword in RECURSE_KEYWORDS {
                if let Some(child) = map.get_mut(*keyword) {
                    reconcile_strict(child);
                }
            }
            if let Some(Value::Object(props)) = map.get_mut("properties") {
                for (_, child) in props.iter_mut() {
                    reconcile_strict(child);
                }
            }
            reconcile_required(map);
        }
        Value::Array(arr) => {
            for child in arr.iter_mut() {
                reconcile_strict(child);
            }
        }
        _ => {}
    }
}

/// JSON Schema keywords whose value is (or contains) a sub-schema
/// that the strict Responses validator will recursively validate.
/// Listed in the order we recurse; the order doesn't matter for
/// correctness but a stable order makes the diff on `cargo fmt`
/// reproducible.
const RECURSE_KEYWORDS: &[&str] = &[
    "additionalProperties", // the schema form, not the bool form
    "items",
    "oneOf",
    "anyOf",
    "allOf",
    "not",
    "if",
    "then",
    "else",
    "contains",
    "propertyNames", // pre-stripped, but a no-op if absent
    "prefixItems",
    "additionalItems",
    "unevaluatedProperties",
    "unevaluatedItems",
];

/// True if this object schema declares itself to be a JSON `object`.
/// Used to decide whether to inject `additionalProperties: false`
/// and `properties: {}` (those rules only apply to object schemas;
/// injecting them into a `string` schema would be a wire-level bug).
fn is_object_schema(map: &serde_json::Map<String, Value>) -> bool {
    matches!(map.get("type"), Some(Value::String(s)) if s == "object")
}

/// Reconcile the `required` array of an object schema against the
/// keys of its `properties` map. Two outcomes:
/// - If `required` is absent, populate it from the property keys.
/// - If `required` is a strict subset of the property keys, extend
///   it with the missing keys (preserving the client's original
///   order, then appending missing keys in `properties` iteration
///   order).
///
/// If the schema has no `properties` map, or `properties` is empty,
/// do nothing — there is nothing to reconcile against.
fn reconcile_required(map: &mut serde_json::Map<String, Value>) {
    // Snapshot the property keys before taking any mutable borrow on
    // `required` — we cannot read `properties` through `map` while
    // `required` is borrowed mutably.
    let prop_keys: Vec<String> = map
        .get("properties")
        .and_then(|v| v.as_object())
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default();
    if prop_keys.is_empty() {
        return;
    }
    if !map.contains_key("required") {
        let req: Vec<Value> = prop_keys.iter().map(|k| Value::String(k.clone())).collect();
        map.insert("required".to_owned(), Value::Array(req));
        return;
    }
    if let Some(Value::Array(req)) = map.get_mut("required") {
        let missing: Vec<Value> = prop_keys
            .iter()
            .filter(|k| !req.iter().any(|v| v.as_str() == Some(k.as_str())))
            .map(|k| Value::String(k.clone()))
            .collect();
        if !missing.is_empty() {
            let names: Vec<&str> = missing.iter().filter_map(|v| v.as_str()).collect();
            tracing::warn!(
                missing = ?names,
                "extending schema `required` array with keys present in `properties` but missing from the client-supplied `required`; the Responses API strict validator rejects any `required` that is a strict subset of `properties` keys. See sanitize_tool_schema doc comment in src/translate.rs."
            );
            for m in missing {
                req.push(m);
            }
        }
    }
}

/// Recursively walk `v` and remove any key named `propertyNames`.
/// Returns the total number of keys removed across the walk.
fn strip_property_names(v: &mut Value) -> usize {
    let mut count = 0;
    match v {
        Value::Object(map) => {
            if map.remove("propertyNames").is_some() {
                count += 1;
            }
            for child in map.values_mut() {
                count += strip_property_names(child);
            }
        }
        Value::Array(arr) => {
            for child in arr.iter_mut() {
                count += strip_property_names(child);
            }
        }
        _ => {}
    }
    count
}

fn append_message_translation(
    out: &mut Vec<InputItem>,
    instructions: &mut String,
    msg: &MessageParam,
    prompt_caching: &PromptCachingConfig,
) -> Result<()> {
    match msg.role {
        MessageRole::User => append_user_message(out, &msg.content, prompt_caching),
        MessageRole::Assistant => append_assistant_message(out, &msg.content),
        MessageRole::System => append_system_message(instructions, &msg.content),
    }
}

fn append_user_message(
    out: &mut Vec<InputItem>,
    content: &MessageContent,
    prompt_caching: &PromptCachingConfig,
) -> Result<()> {
    match content {
        MessageContent::Text(s) => {
            out.push(InputItem::Message {
                role: "user".into(),
                content: vec![InputContentPart::InputText {
                    text: s.clone(),
                    prompt_cache_breakpoint: None,
                }],
            });
            Ok(())
        }
        MessageContent::Blocks(blocks) => {
            // Walk the block list in order, accumulating text and image
            // content parts into a single `Message` item. When we hit a
            // `ToolResult`, flush the accumulated parts as a user message
            // and emit a `FunctionCallOutput` item.
            //
            // We also track which accumulated part ends an Anthropic
            // `cache_control: {type: "ephemeral"}` block, so we can emit
            // an OpenAI `prompt_cache_breakpoint` marker on it.
            let mut parts: Vec<(InputContentPart, bool)> = Vec::new();
            for block in blocks {
                match block {
                    ContentBlockParam::Text(t) => {
                        let ephemeral = !prompt_caching.models.is_empty() && is_ephemeral(&t.cache_control);
                        // Append text to the last part if it's also text
                        // and extending it won't move a cache breakpoint
                        // past the end of a non-ephemeral block. Otherwise
                        // start a new text part so the breakpoint can sit
                        // on the right content.
                        if let Some((InputContentPart::InputText { text, .. }, last_ephemeral)) =
                            parts.last_mut()
                            && (!*last_ephemeral || ephemeral)
                        {
                            text.push('\n');
                            text.push_str(&t.text);
                            *last_ephemeral = ephemeral;
                            continue;
                        }
                        parts.push((
                            InputContentPart::InputText {
                                text: t.text.clone(),
                                prompt_cache_breakpoint: None,
                            },
                            ephemeral,
                        ));
                    }
                    ContentBlockParam::ToolResult(tr) => {
                        flush_user_parts(out, &mut parts);
                        out.push(InputItem::FunctionCallOutput {
                            call_id: tr.tool_use_id.clone(),
                            output: tool_result_text(&tr.content),
                        });
                    }
                    ContentBlockParam::Image(img) => {
                        if let Some(image_part) = convert_image_block(img) {
                            let ephemeral =
                                !prompt_caching.models.is_empty() && is_ephemeral(&img.cache_control);
                            parts.push((image_part, ephemeral));
                        }
                    }
                    ContentBlockParam::ToolUse(_) => {
                        return Err(anyhow!(
                            "user message contains a tool_use block; the assistant produced this in a prior turn"
                        ));
                    }
                    ContentBlockParam::Unknown => {
                        tracing::warn!("dropping unknown user content block type");
                    }
                }
            }
            flush_user_parts(out, &mut parts);
            Ok(())
        }
    }
}

fn flush_user_parts(out: &mut Vec<InputItem>, parts: &mut Vec<(InputContentPart, bool)>) {
    if !parts.is_empty() {
        let taken = std::mem::take(parts);
        let content = taken
            .into_iter()
            .map(|(mut part, breakpoint)| {
                if breakpoint
                    && let InputContentPart::InputText {
                        prompt_cache_breakpoint,
                        ..
                    }
                    | InputContentPart::InputImage {
                        prompt_cache_breakpoint,
                        ..
                    } = &mut part
                {
                    *prompt_cache_breakpoint = Some(PromptCacheBreakpoint {
                        mode: "explicit".into(),
                    });
                }
                part
            })
            .collect();
        out.push(InputItem::Message {
            role: "user".into(),
            content,
        });
    }
}

/// True if the Anthropic block asks for an ephemeral cache checkpoint.
fn is_ephemeral(cache_control: &Option<CacheControl>) -> bool {
    cache_control
        .as_ref()
        .is_some_and(|c| c.r#type.as_deref() == Some("ephemeral"))
}
///
/// Supports both `base64` and `url` source types:
/// - `base64`: constructs a data URI `data:{media_type};base64,{data}`
/// - `url`: passes the URL through directly
///
/// Returns `None` if the source type is unknown or required fields are missing.
fn convert_image_block(img: &ImageBlockParam) -> Option<InputContentPart> {
    let source_type = img.source.get("type").and_then(Value::as_str)?;
    match source_type {
        "base64" => {
            let media_type = img.source.get("media_type").and_then(Value::as_str)?;
            let data = img.source.get("data").and_then(Value::as_str)?;
            let data_uri = format!("data:{};base64,{}", media_type, data);
            Some(InputContentPart::InputImage {
                image_url: data_uri,
                detail: None,
                prompt_cache_breakpoint: None,
            })
        }
        "url" => {
            let url = img.source.get("url").and_then(Value::as_str)?;
            Some(InputContentPart::InputImage {
                image_url: url.to_string(),
                detail: None,
                prompt_cache_breakpoint: None,
            })
        }
        other => {
            tracing::warn!(
                source_type = %other,
                "dropping image block with unknown source type"
            );
            None
        }
    }
}

fn tool_result_text(content: &Option<ToolResultContent>) -> String {
    match content {
        Some(ToolResultContent::Text(s)) => s.clone(),
        Some(ToolResultContent::Blocks(blocks)) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        None => String::new(),
    }
}

fn append_assistant_message(out: &mut Vec<InputItem>, content: &MessageContent) -> Result<()> {
    match content {
        MessageContent::Text(s) => {
            out.push(InputItem::Message {
                role: "assistant".into(),
                content: vec![InputContentPart::OutputText { text: s.clone() }],
            });
            Ok(())
        }
        MessageContent::Blocks(blocks) => {
            let mut text = String::new();
            let mut calls: Vec<(String, String, String)> = Vec::new(); // (id, name, arguments)
            for block in blocks {
                match block {
                    ContentBlockParam::Text(t) => {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(&t.text);
                    }
                    ContentBlockParam::ToolUse(tu) => {
                        let arguments = serde_json::to_string(&tu.input)
                            .context("serialize tool_use input to JSON string")?;
                        calls.push((tu.id.clone(), tu.name.clone(), arguments));
                    }
                    ContentBlockParam::Image(_)
                    | ContentBlockParam::ToolResult(_)
                    | ContentBlockParam::Unknown => {
                        tracing::warn!("dropping unsupported block from assistant message");
                    }
                }
            }
            if !text.is_empty() || calls.is_empty() {
                // Always emit a Message item for the text (even if empty)
                // so that the assistant turn is well-formed when there
                // are no tool calls.
                out.push(InputItem::Message {
                    role: "assistant".into(),
                    content: if text.is_empty() {
                        Vec::new()
                    } else {
                        vec![InputContentPart::OutputText { text }]
                    },
                });
            }
            for (id, name, arguments) in calls {
                out.push(InputItem::FunctionCall {
                    call_id: id,
                    name,
                    arguments,
                });
            }
            Ok(())
        }
    }
}

fn append_system_message(instructions: &mut String, content: &MessageContent) -> Result<()> {
    let text = match content {
        MessageContent::Text(s) => s.clone(),
        MessageContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlockParam::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    };
    if !text.is_empty() {
        if !instructions.is_empty() {
            instructions.push('\n');
        }
        instructions.push_str(&text);
    }
    Ok(())
}

fn map_tool_choice(choice: &ToolChoice) -> ResponsesToolChoice {
    match choice {
        ToolChoice::Auto { .. } => ResponsesToolChoice::Simple("auto".to_string()),
        ToolChoice::Any { .. } => ResponsesToolChoice::Simple("required".to_string()),
        ToolChoice::None {} => ResponsesToolChoice::Simple("none".to_string()),
        ToolChoice::Tool { name, .. } => {
            // The Responses API distinguishes function tools (with
            // `{type: "function", name: "..."}`) from built-in tools like
            // `web_search` (which use `{type: "web_search"}` with no name).
            // If we forward `{type: "function", name: "web_search"}` while
            // the tools array contains a `web_search` built-in, the
            // upstream rejects with: Tool choice 'function' not found in
            // 'tools' parameter. For built-in server tools, fall back to
            // `"auto"` so the model can decide when to invoke them.
            if name == "web_search" {
                ResponsesToolChoice::Simple("auto".to_string())
            } else {
                ResponsesToolChoice::Function {
                    kind: "function".to_string(),
                    name: name.clone(),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anthropic::{
        CacheControl, ImageBlockParam, MessageParam, TextBlockParam, Tool, ToolResultBlockParam,
        ToolUseBlockParam,
    };
    use crate::config::PromptCachingConfig;
    use serde_json::json;

    /// Build a minimal request with a system prompt, one user message, and
    /// one tool definition. Tests below mutate this to test specific paths.
    fn fixture_request() -> CreateMessageRequest {
        CreateMessageRequest {
            model: "test-model".into(),
            max_tokens: 256,
            system: Some(SystemPrompt::Text("You are helpful.".into())),
            messages: vec![MessageParam {
                role: MessageRole::User,
                content: MessageContent::Text("Hello".into()),
            }],
            tools: Some(vec![Tool {
                name: "get_weather".into(),
                description: Some("Get the weather".into()),
                input_schema: Some(json!({
                    "type": "object",
                    "properties": {"location": {"type": "string"}},
                    "required": ["location"],
                })),
            }]),
            tool_choice: None,
            temperature: Some(0.5),
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: Some(false),
            metadata: None,
        }
    }

    fn unwrap_items(req: ResponsesRequest) -> Vec<InputItem> {
        match req.input {
            Input::Items(items) => items,
            Input::Text(_) => panic!("expected Items, got Text"),
        }
    }

    #[test]
    fn basic_request_translates_cleanly() {
        let req = fixture_request();
        let out = anthropic_to_responses(&req, None, &PromptCachingConfig::default()).unwrap();
        assert_eq!(out.model, "test-model");
        assert_eq!(out.max_output_tokens, Some(256));
        assert!(!out.stream);

        // instructions + user
        assert_eq!(out.instructions.as_deref(), Some("You are helpful."));
        // Snapshot the bits we want to assert on after consuming `out`.
        let temperature = out.temperature;
        let tools = out.tools.clone();
        let items = unwrap_items(out);
        assert_eq!(items.len(), 1);
        match &items[0] {
            InputItem::Message { role, content } => {
                assert_eq!(role, "user");
                match &content[0] {
                    InputContentPart::InputText { text, .. } => assert_eq!(text, "Hello"),
                    _ => panic!("expected input_text part"),
                }
            }
            _ => panic!("expected message item"),
        }

        // Tools lifted, input_schema → parameters + strict: true
        let tools = tools.as_ref().unwrap();
        assert_eq!(tools.len(), 1);
        match &tools[0] {
            ToolDefinition::Function(t) => {
                assert_eq!(t.kind, "function");
                assert_eq!(t.name, "get_weather");
                assert!(t.strict);
                assert_eq!(t.parameters["type"], "object");
                assert_eq!(t.parameters["additionalProperties"], false);
            }
            _ => panic!("expected function tool"),
        }

        // temperature passed through
        assert_eq!(temperature, Some(0.5));
    }

    #[test]
    fn system_as_text_blocks_concatenates() {
        let mut req = fixture_request();
        req.system = Some(SystemPrompt::Blocks(vec![
            TextBlockParam {
                text: "Be concise.".into(),
                cache_control: None,
            },
            TextBlockParam {
                text: "Use examples.".into(),
                cache_control: None,
            },
        ]));
        let out = anthropic_to_responses(&req, None, &PromptCachingConfig::default()).unwrap();
        assert_eq!(
            out.instructions.as_deref(),
            Some("Be concise.\n\nUse examples.")
        );
    }

    #[test]
    fn tool_use_in_assistant_emits_message_then_function_call() {
        let mut req = fixture_request();
        req.messages = vec![
            MessageParam {
                role: MessageRole::User,
                content: MessageContent::Text("What's the weather in SF?".into()),
            },
            MessageParam {
                role: MessageRole::Assistant,
                content: MessageContent::Blocks(vec![
                    ContentBlockParam::Text(TextBlockParam {
                        text: "Let me check.".into(),
                        cache_control: None,
                    }),
                    ContentBlockParam::ToolUse(ToolUseBlockParam {
                        id: "toolu_01".into(),
                        name: "get_weather".into(),
                        input: json!({"location": "SF"}),
                    }),
                ]),
            },
            MessageParam {
                role: MessageRole::User,
                content: MessageContent::Blocks(vec![ContentBlockParam::ToolResult(
                    ToolResultBlockParam {
                        tool_use_id: "toolu_01".into(),
                        content: Some(ToolResultContent::Text("72F sunny".into())),
                        is_error: None,
                    },
                )]),
            },
        ];

        let out = anthropic_to_responses(&req, None, &PromptCachingConfig::default()).unwrap();
        // user, assistant(message+text), assistant(function_call), user(function_call_output)
        let items = unwrap_items(out);
        assert_eq!(items.len(), 4);

        match &items[1] {
            InputItem::Message { role, content } => {
                assert_eq!(role, "assistant");
                match &content[0] {
                    InputContentPart::OutputText { text } => assert_eq!(text, "Let me check."),
                    _ => panic!("expected output_text part for assistant message"),
                }
            }
            _ => panic!("expected assistant message"),
        }
        match &items[2] {
            InputItem::FunctionCall {
                call_id,
                name,
                arguments,
            } => {
                assert_eq!(call_id, "toolu_01");
                assert_eq!(name, "get_weather");
                let parsed: Value = serde_json::from_str(arguments).unwrap();
                assert_eq!(parsed["location"], "SF");
            }
            _ => panic!("expected function_call item"),
        }
        match &items[3] {
            InputItem::FunctionCallOutput { call_id, output } => {
                assert_eq!(call_id, "toolu_01");
                assert_eq!(output, "72F sunny");
            }
            _ => panic!("expected function_call_output item"),
        }
    }

    #[test]
    fn user_mixed_text_and_tool_results_preserves_order() {
        let mut req = fixture_request();
        req.messages = vec![MessageParam {
            role: MessageRole::User,
            content: MessageContent::Blocks(vec![
                ContentBlockParam::Text(TextBlockParam {
                    text: "Here's what I got:".into(),
                    cache_control: None,
                }),
                ContentBlockParam::ToolResult(ToolResultBlockParam {
                    tool_use_id: "toolu_01".into(),
                    content: Some(ToolResultContent::Text("first result".into())),
                    is_error: None,
                }),
                ContentBlockParam::Text(TextBlockParam {
                    text: "and:".into(),
                    cache_control: None,
                }),
                ContentBlockParam::ToolResult(ToolResultBlockParam {
                    tool_use_id: "toolu_02".into(),
                    content: Some(ToolResultContent::Text("second result".into())),
                    is_error: None,
                }),
            ]),
        }];

        let out = anthropic_to_responses(&req, None, &PromptCachingConfig::default()).unwrap();
        let items = unwrap_items(out);
        // user("Here's what I got:") + function_call_output(first) + user("and:") + function_call_output(second)
        assert_eq!(items.len(), 4);
        match &items[0] {
            InputItem::Message { content, .. } => match &content[0] {
                InputContentPart::InputText { text, .. } => assert_eq!(text, "Here's what I got:"),
                _ => panic!(),
            },
            _ => panic!(),
        }
        match &items[1] {
            InputItem::FunctionCallOutput { call_id, output } => {
                assert_eq!(call_id, "toolu_01");
                assert_eq!(output, "first result");
            }
            _ => panic!(),
        }
        match &items[2] {
            InputItem::Message { content, .. } => match &content[0] {
                InputContentPart::InputText { text, .. } => assert_eq!(text, "and:"),
                _ => panic!(),
            },
            _ => panic!(),
        }
        match &items[3] {
            InputItem::FunctionCallOutput { call_id, output } => {
                assert_eq!(call_id, "toolu_02");
                assert_eq!(output, "second result");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn user_with_image_block_is_forwarded() {
        let mut req = fixture_request();
        req.messages = vec![MessageParam {
            role: MessageRole::User,
            content: MessageContent::Blocks(vec![
                ContentBlockParam::Text(TextBlockParam {
                    text: "What is this?".into(),
                    cache_control: None,
                }),
                ContentBlockParam::Image(ImageBlockParam {
                    source: json!({
                        "type": "base64",
                        "media_type": "image/jpeg",
                        "data": "/9j/4AAQSkZJRg=="
                    }),
                    cache_control: None,
                }),
            ]),
        }];
        let out = anthropic_to_responses(&req, None, &PromptCachingConfig::default()).unwrap();
        let items = unwrap_items(out);
        // user(text + image) — both in one message
        assert_eq!(items.len(), 1);
        match &items[0] {
            InputItem::Message { role, content } => {
                assert_eq!(role, "user");
                assert_eq!(content.len(), 2);
                // First part: text
                match &content[0] {
                    InputContentPart::InputText { text, .. } => {
                        assert_eq!(text, "What is this?");
                    }
                    _ => panic!("expected input_text as first content part"),
                }
                // Second part: image
                match &content[1] {
                    InputContentPart::InputImage {
                        image_url, detail, ..
                    } => {
                        assert_eq!(image_url, "data:image/jpeg;base64,/9j/4AAQSkZJRg==");
                        assert!(detail.is_none());
                    }
                    _ => panic!("expected input_image as second content part"),
                }
            }
            _ => panic!("expected message item"),
        }
    }

    #[test]
    fn tool_choice_maps() {
        let mut req = fixture_request();
        req.tool_choice = Some(ToolChoice::Auto {
            disable_parallel_tool_use: None,
        });
        let out = anthropic_to_responses(&req, None, &PromptCachingConfig::default()).unwrap();
        match out.tool_choice.unwrap() {
            ResponsesToolChoice::Simple(s) => assert_eq!(s, "auto"),
            _ => panic!("expected simple tool_choice"),
        }

        req.tool_choice = Some(ToolChoice::Any {
            disable_parallel_tool_use: None,
        });
        let out = anthropic_to_responses(&req, None, &PromptCachingConfig::default()).unwrap();
        match out.tool_choice.unwrap() {
            ResponsesToolChoice::Simple(s) => assert_eq!(s, "required"),
            _ => panic!("expected simple 'required'"),
        }

        req.tool_choice = Some(ToolChoice::None {});
        let out = anthropic_to_responses(&req, None, &PromptCachingConfig::default()).unwrap();
        match out.tool_choice.unwrap() {
            ResponsesToolChoice::Simple(s) => assert_eq!(s, "none"),
            _ => panic!("expected simple 'none'"),
        }

        req.tool_choice = Some(ToolChoice::Tool {
            name: "get_weather".into(),
            disable_parallel_tool_use: None,
        });
        let out = anthropic_to_responses(&req, None, &PromptCachingConfig::default()).unwrap();
        match out.tool_choice.unwrap() {
            ResponsesToolChoice::Function { kind, name } => {
                assert_eq!(kind, "function");
                assert_eq!(name, "get_weather");
            }
            _ => panic!("expected function tool_choice"),
        }
    }

    #[test]
    fn stream_true_sets_flag() {
        let mut req = fixture_request();
        req.stream = Some(true);
        let out = anthropic_to_responses(&req, None, &PromptCachingConfig::default()).unwrap();
        assert!(out.stream);
    }

    #[test]
    fn metadata_user_id_becomes_user() {
        let mut req = fixture_request();
        req.metadata = Some(crate::anthropic::Metadata {
            user_id: Some("user-123".into()),
        });
        let out = anthropic_to_responses(&req, None, &PromptCachingConfig::default()).unwrap();
        assert_eq!(out.user.as_deref(), Some("user-123"));
    }

    /// At the limit (exactly 64 chars), the value is passed through
    /// untouched. The boundary is inclusive on this side: `len() == max`
    /// does not trigger a truncation warn.
    #[test]
    fn user_id_at_limit_passes_through_unchanged() {
        let mut req = fixture_request();
        let user_id = "a".repeat(USER_ID_MAX_LEN);
        req.metadata = Some(crate::anthropic::Metadata {
            user_id: Some(user_id.clone()),
        });
        let out = anthropic_to_responses(&req, None, &PromptCachingConfig::default()).unwrap();
        assert_eq!(out.user.as_deref(), Some(user_id.as_str()));
        assert_eq!(out.user.as_ref().map(String::len), Some(USER_ID_MAX_LEN));
    }

    /// Claude Code's actual `metadata.user_id` is ~150 chars in normal
    /// operation (session UUID + account + workspace path). This is the
    /// regression test for the `400 string_above_max_length` upstream
    /// error the proxy hit live.
    #[test]
    fn user_id_over_limit_is_truncated() {
        let mut req = fixture_request();
        let user_id = "a".repeat(150);
        req.metadata = Some(crate::anthropic::Metadata {
            user_id: Some(user_id.clone()),
        });
        let out = anthropic_to_responses(&req, None, &PromptCachingConfig::default()).unwrap();
        let out_user = out.user.as_deref().expect("user should be present");
        assert_eq!(out_user.len(), USER_ID_MAX_LEN);
        // The leading prefix (the most operationally meaningful part
        // — the session UUID) is preserved. This is the property that
        // motivates truncation over hashing.
        assert_eq!(out_user, &user_id[..USER_ID_MAX_LEN]);
    }

    /// One byte over the limit triggers truncation. This pins the
    /// boundary on the upper side so a future "off by one" change to
    /// `USER_ID_MAX_LEN` (e.g. switching to `<` instead of `<=`) shows
    /// up here rather than as a live 400.
    #[test]
    fn user_id_exactly_one_over_limit_truncates() {
        let mut req = fixture_request();
        let user_id = "a".repeat(USER_ID_MAX_LEN + 1);
        req.metadata = Some(crate::anthropic::Metadata {
            user_id: Some(user_id.clone()),
        });
        let out = anthropic_to_responses(&req, None, &PromptCachingConfig::default()).unwrap();
        let out_user = out.user.as_deref().expect("user should be present");
        assert_eq!(out_user.len(), USER_ID_MAX_LEN);
        assert_eq!(out_user, &user_id[..USER_ID_MAX_LEN]);
    }

    /// No `metadata.user_id` means no `user` field in the outbound
    /// request. The truncation logic must not synthesize a value when
    /// the client provided none.
    #[test]
    fn user_id_absent_stays_none() {
        let req = fixture_request();
        let out = anthropic_to_responses(&req, None, &PromptCachingConfig::default()).unwrap();
        assert!(out.user.is_none());
    }

    /// Direct unit test of the helper. Asserts the pure-function
    /// contract — useful when the integration above is restructured
    /// (e.g. if we add a `User` newtype in the future).
    #[test]
    fn truncate_user_id_helper_is_pure() {
        // Under the limit: identity.
        let short = "x".repeat(USER_ID_MAX_LEN);
        assert_eq!(truncate_user_id(short.clone()), short);
        // Empty string is a valid (if useless) value and must pass
        // through — `len() == 0 <= 64`.
        assert_eq!(truncate_user_id(String::new()), String::new());
        // Over the limit: bounded.
        let long = "y".repeat(USER_ID_MAX_LEN * 3);
        let truncated = truncate_user_id(long.clone());
        assert_eq!(truncated.len(), USER_ID_MAX_LEN);
        assert_eq!(truncated, long[..USER_ID_MAX_LEN]);
    }

    #[test]
    fn stop_sequences_are_dropped() {
        let mut req = fixture_request();
        req.stop_sequences = Some(vec!["\n\nHuman:".into(), "###END###".into()]);
        let out = anthropic_to_responses(&req, None, &PromptCachingConfig::default()).unwrap();
        // stop_sequences has no Responses equivalent; just don't error.
        // (We can't assert against an absent field easily here, but the
        // function completing without error is the contract.)
        let _ = out;
    }

    #[test]
    fn reasoning_effort_passes_through_to_nested_reasoning() {
        let req = fixture_request();
        let out =
            anthropic_to_responses(&req, Some("medium".into()), &PromptCachingConfig::default())
                .unwrap();
        let reasoning = out.reasoning.as_ref().expect("reasoning set");
        assert_eq!(reasoning.effort, "medium");
    }

    #[test]
    fn reasoning_effort_none_omits_field() {
        let req = fixture_request();
        let out = anthropic_to_responses(&req, None, &PromptCachingConfig::default()).unwrap();
        let body = serde_json::to_value(&out).unwrap();
        assert!(
            body.get("reasoning").is_none(),
            "expected reasoning absent, got {:?}",
            body.get("reasoning")
        );
    }

    #[test]
    fn sanitize_tool_schema_strips_empty_additional_properties() {
        let schema = json!({
            "type": "object",
            "properties": {"x": {"type": "string"}},
            "additionalProperties": {},
        });
        let out = sanitize_tool_schema(schema);
        // Empty-object form is removed AND then re-added as `false`
        // by our strict-mode injection below.
        assert_eq!(out["additionalProperties"], Value::Bool(false));
        assert_eq!(out["type"], "object");
        assert_eq!(out["properties"]["x"]["type"], "string");
    }

    #[test]
    fn sanitize_tool_schema_keeps_false_additional_properties() {
        let schema = json!({
            "type": "object",
            "properties": {"x": {"type": "string"}},
            "additionalProperties": false,
        });
        let out = sanitize_tool_schema(schema);
        assert_eq!(out["additionalProperties"], Value::Bool(false));
    }

    #[test]
    fn sanitize_tool_schema_keeps_non_empty_additional_properties() {
        // Non-empty additionalProperties is left alone (Responses
        // would reject it under strict mode, but that's a validation
        // error at the upstream, not our problem here).
        let schema = json!({
            "type": "object",
            "properties": {"x": {"type": "string"}},
            "additionalProperties": {"type": "string"},
        });
        let out = sanitize_tool_schema(schema);
        assert_eq!(out["additionalProperties"]["type"], "string");
    }

    #[test]
    fn sanitize_tool_schema_adds_empty_properties_if_absent() {
        // Strict mode requires `properties` to be present.
        let schema = json!({"type": "object"});
        let out = sanitize_tool_schema(schema);
        assert_eq!(out["properties"], json!({}));
    }

    /// Regression test for the `Agent` tool 400: Claude Code emits
    /// `properties` with keys (e.g. `isolation`) but no `required`
    /// array. Strict mode requires every key in `properties` to also
    /// be in `required`. The auto-population matches the upstream's
    /// own hosted tool list shape.
    #[test]
    fn sanitize_tool_schema_adds_required_from_properties_keys() {
        let schema = json!({
            "type": "object",
            "properties": {
                "description": {"type": "string"},
                "prompt": {"type": "string"},
                "subagent_type": {"type": "string"},
                "isolation": {"type": "string"},
            },
        });
        let out = sanitize_tool_schema(schema);
        let required = out["required"]
            .as_array()
            .expect("required should be an array");
        let mut names: Vec<&str> = required.iter().map(|v| v.as_str().unwrap()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            vec!["description", "isolation", "prompt", "subagent_type"]
        );
    }

    /// If the client provided a `required` that is already a complete
    /// superset of the `properties` keys, the array is unchanged and
    /// no warn fires. This pins the contract for the no-op case.
    #[test]
    fn sanitize_tool_schema_required_already_complete_is_unchanged() {
        let schema = json!({
            "type": "object",
            "properties": {
                "a": {"type": "string"},
                "b": {"type": "string"},
            },
            "required": ["a", "b"],
        });
        let out = sanitize_tool_schema(schema);
        assert_eq!(out["required"], json!(["a", "b"]));
    }

    /// The live regression: Claude Code's `Agent` tool ships with
    /// `required: ["description", "prompt"]` and a `properties` map of
    /// 6 keys (description, isolation, model, prompt, run_in_background,
    /// subagent_type). The strict validator rejects this as `Missing
    /// 'isolation'`. The reconciler extends `required` to cover all
    /// 6 keys.
    #[test]
    fn sanitize_tool_schema_extends_subset_required_with_missing_keys() {
        let schema = json!({
            "type": "object",
            "properties": {
                "description": {"type": "string"},
                "isolation": {"type": "string"},
                "model": {"type": "string"},
                "prompt": {"type": "string"},
                "run_in_background": {"type": "boolean"},
                "subagent_type": {"type": "string"},
            },
            "required": ["description", "prompt"],
        });
        let out = sanitize_tool_schema(schema);
        let required = out["required"].as_array().expect("required is array");
        let names: Vec<&str> = required.iter().map(|v| v.as_str().unwrap()).collect();
        // Existing client order preserved ("description", "prompt"),
        // missing keys appended in `properties` iteration order.
        assert_eq!(
            names,
            vec![
                "description",
                "prompt",
                "isolation",
                "model",
                "run_in_background",
                "subagent_type",
            ]
        );
    }

    /// The reconciler appends missing keys in `properties` order, not
    /// alphabetical. Pin this so a future refactor doesn't quietly
    /// change the wire shape.
    #[test]
    fn sanitize_tool_schema_extends_required_preserves_existing_order() {
        let schema = json!({
            "type": "object",
            "properties": {
                "a": {"type": "string"},
                "b": {"type": "string"},
                "c": {"type": "string"},
            },
            "required": ["a"],
        });
        let out = sanitize_tool_schema(schema);
        assert_eq!(out["required"], json!(["a", "b", "c"]));
    }

    /// Live regression: Claude Code's `AskUserQuestion` tool emits
    /// `propertyNames` at the top of `parameters` next to `properties`.
    /// The Responses strict validator rejects this with
    /// `propertyNames is not permitted`. The strip removes it
    /// regardless of where it sits.
    #[test]
    fn sanitize_tool_schema_strips_property_names_at_top_level() {
        let schema = json!({
            "type": "object",
            "properties": {"x": {"type": "string"}},
            "propertyNames": {"type": "string"},
        });
        let out = sanitize_tool_schema(schema);
        assert!(
            out.get("propertyNames").is_none(),
            "expected propertyNames stripped, got {:?}",
            out.get("propertyNames")
        );
        // The rest of the schema is untouched.
        assert_eq!(out["type"], "object");
        assert_eq!(out["properties"]["x"]["type"], "string");
    }

    /// The live AskUserQuestion regression: `propertyNames` nested
    /// inside an object property. This is the case that actually
    /// fired the upstream 400.
    #[test]
    fn sanitize_tool_schema_strips_property_names_inside_object_property() {
        let schema = json!({
            "type": "object",
            "properties": {
                "annotations": {
                    "type": "object",
                    "additionalProperties": {"type": "string"},
                    "propertyNames": {"type": "string"},
                },
            },
        });
        let out = sanitize_tool_schema(schema);
        assert!(
            out["properties"]["annotations"]
                .get("propertyNames")
                .is_none(),
            "expected propertyNames stripped from inside annotations"
        );
    }

    /// `propertyNames` can also appear inside `items` (array element
    /// schemas). The strip is recursive so this is handled.
    #[test]
    fn sanitize_tool_schema_strips_property_names_inside_array_items() {
        let schema = json!({
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "propertyNames": {"type": "string"},
                        "properties": {"q": {"type": "string"}},
                    },
                },
            },
        });
        let out = sanitize_tool_schema(schema);
        assert!(
            out["properties"]["questions"]["items"]
                .get("propertyNames")
                .is_none()
        );
    }

    /// Pin the recursion depth so a future "stop after one level"
    /// refactor is caught.
    #[test]
    fn sanitize_tool_schema_strips_property_names_recursively_in_nested_objects() {
        let schema = json!({
            "type": "object",
            "properties": {
                "x": {
                    "type": "object",
                    "properties": {
                        "y": {
                            "type": "object",
                            "propertyNames": {"type": "string"},
                        },
                    },
                },
            },
        });
        let out = sanitize_tool_schema(schema);
        assert!(
            out["properties"]["x"]["properties"]["y"]
                .get("propertyNames")
                .is_none()
        );
    }

    /// Sanity: a schema that already passes through cleanly (no
    /// `propertyNames`, complete `required`, `additionalProperties:
    /// false`) is unchanged by the strip step.
    #[test]
    fn sanitize_tool_schema_without_property_names_is_unchanged() {
        let schema = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "a": {"type": "string"},
            },
            "required": ["a"],
        });
        let out = sanitize_tool_schema(schema.clone());
        assert_eq!(out, schema);
    }

    // ─── recursion tests ────────────────────────────────────────────
    //
    // The strict Responses validator recurses into every nested
    // object schema (additionalProperties values, items, oneOf, etc.)
    // and applies the same rules. sanitize_tool_schema has to do the
    // same, otherwise we 400 at a depth the proxy didn't reach.

    /// Live regression: Claude Code's `AskUserQuestion` tool emits
    /// `properties.annotations.additionalProperties` as an object
    /// schema with `properties: { notes, preview }` and no `required`.
    /// The validator's complaint: `In context=('properties',
    /// 'annotations', 'additionalProperties'), 'required' is
    /// required to be supplied and to be an array including every
    /// key in properties. Missing 'notes'.`
    #[test]
    fn sanitize_tool_schema_reconciles_required_in_nested_additional_properties() {
        let schema = json!({
            "type": "object",
            "properties": {
                "annotations": {
                    "type": "object",
                    "additionalProperties": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {
                            "notes": {"type": "string"},
                            "preview": {"type": "string"},
                        },
                    },
                },
            },
        });
        let out = sanitize_tool_schema(schema);
        let ap = &out["properties"]["annotations"]["additionalProperties"];
        // The nested object's `required` is now populated.
        assert_eq!(ap["required"], json!(["notes", "preview"]));
        // And `additionalProperties: false` is preserved (we don't
        // strip a non-empty value).
        assert_eq!(ap["additionalProperties"], false);
    }

    /// `properties.x.items` is an object schema; the same rules apply.
    /// Pins the `items` recursion path (covers arrays of objects).
    /// Note: `serde_json::Map` is alphabetically ordered by default
    /// (the `preserve_order` feature is off), so the property keys
    /// appear in sorted order in the output, not the JSON literal
    /// order of the test fixture.
    #[test]
    fn sanitize_tool_schema_reconciles_required_in_nested_items() {
        let schema = json!({
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "q": {"type": "string"},
                            "a": {"type": "string"},
                        },
                    },
                },
            },
        });
        let out = sanitize_tool_schema(schema);
        let items = &out["properties"]["questions"]["items"];
        assert_eq!(items["required"], json!(["a", "q"]));
        assert_eq!(items["additionalProperties"], false);
    }

    /// `propertyNames` inside a nested `additionalProperties` schema
    /// must also be stripped. The pre-pass walk handles this because
    /// it traverses every object regardless of depth.
    #[test]
    fn sanitize_tool_schema_strips_property_names_in_nested_additional_properties() {
        let schema = json!({
            "type": "object",
            "properties": {
                "annotations": {
                    "type": "object",
                    "additionalProperties": {
                        "type": "object",
                        "properties": {"x": {"type": "string"}},
                        "propertyNames": {"type": "string"},
                    },
                },
            },
        });
        let out = sanitize_tool_schema(schema);
        assert!(
            out["properties"]["annotations"]["additionalProperties"]
                .get("propertyNames")
                .is_none()
        );
    }

    /// An object schema deep in the tree that is missing
    /// `additionalProperties` gets it added — same rule as the
    /// top-level schema, applied recursively.
    #[test]
    fn sanitize_tool_schema_injects_additional_properties_false_nested() {
        let schema = json!({
            "type": "object",
            "properties": {
                "outer": {
                    "type": "object",
                    "properties": {
                        "inner": {
                            "type": "object",
                            "properties": {
                                "deep": {"type": "string"},
                            },
                        },
                    },
                },
            },
        });
        let out = sanitize_tool_schema(schema);
        let deep = &out["properties"]["outer"]["properties"]["inner"];
        assert_eq!(deep["additionalProperties"], false);
        assert_eq!(deep["required"], json!(["deep"]));
    }

    /// The empty-object `additionalProperties: {}` form can appear
    /// nested too. It gets stripped at every level — and since the
    /// surrounding schema is `type: "object"`, the `false`-add rule
    /// immediately replaces it with `additionalProperties: false`.
    #[test]
    fn sanitize_tool_schema_strips_empty_additional_properties_nested() {
        let schema = json!({
            "type": "object",
            "properties": {
                "outer": {
                    "type": "object",
                    "additionalProperties": {},
                },
            },
        });
        let out = sanitize_tool_schema(schema);
        // After the strip and the false-add, the nested `outer` has
        // `additionalProperties: false`.
        assert_eq!(out["properties"]["outer"]["additionalProperties"], false);
    }

    /// `oneOf` / `anyOf` / `allOf` arrays carry sub-schemas that the
    /// strict validator will recurse into. We must too.
    #[test]
    fn sanitize_tool_schema_reconciles_required_inside_one_of_branches() {
        let schema = json!({
            "type": "object",
            "properties": {
                "discriminated": {
                    "oneOf": [
                        {
                            "type": "object",
                            "properties": {
                                "kind": {"type": "string"},
                                "value_a": {"type": "string"},
                            },
                        },
                        {
                            "type": "object",
                            "properties": {
                                "kind": {"type": "string"},
                                "value_b": {"type": "integer"},
                            },
                        },
                    ],
                },
            },
        });
        let out = sanitize_tool_schema(schema);
        let branches = out["properties"]["discriminated"]["oneOf"]
            .as_array()
            .unwrap();
        assert_eq!(branches[0]["required"], json!(["kind", "value_a"]));
        assert_eq!(branches[1]["required"], json!(["kind", "value_b"]));
    }

    /// Sanity for the nested case: a schema that already has
    /// `required` populated at every object level (matching the
    /// strict rules) is unchanged by `sanitize_tool_schema`.
    /// Note: the top-level and intermediate object schemas must
    /// include `required` in the fixture — the reconciler populates
    /// missing `required` arrays at every depth, so a schema that
    /// omits them will be enriched, not unchanged.
    #[test]
    fn sanitize_tool_schema_nested_clean_schema_is_unchanged() {
        let schema = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "outer": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["inner"],
                    "properties": {
                        "inner": {
                            "type": "object",
                            "additionalProperties": false,
                            "properties": {
                                "a": {"type": "string"},
                            },
                            "required": ["a"],
                        },
                    },
                },
            },
            "required": ["outer"],
        });
        let out = sanitize_tool_schema(schema.clone());
        assert_eq!(out, schema);
    }

    // ─── format-strip tests ─────────────────────────────────────────
    //
    // The strict Responses validator only accepts the 9 IETF
    // validation formats (date-time, time, date, duration, email,
    // hostname, ipv4, ipv6, uuid); every other format value is
    // rejected with `'<value>' is not a valid format`. We strip
    // unconditionally because the model is the producer of these
    // strings and the format hint is not load-bearing.

    /// Live regression: Claude Code's `WebFetch` tool declares
    /// `properties.url.format: "uri"` which the strict validator
    /// rejects. The strip removes the format but preserves
    /// `type: "string"`.
    #[test]
    fn sanitize_tool_schema_strips_format_at_top_level() {
        let schema = json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "format": "uri",
                },
            },
        });
        let out = sanitize_tool_schema(schema);
        let url = &out["properties"]["url"];
        assert!(url.get("format").is_none(), "format should be stripped");
        // type: "string" is preserved.
        assert_eq!(url["type"], "string");
    }

    /// The format keyword is dropped at any depth, not just at the
    /// top of a property. Pinned to a nested `additionalProperties`
    /// schema because that's where the cyanheads/git-mcp-server
    /// issue surfaced the same error.
    #[test]
    fn sanitize_tool_schema_strips_format_nested() {
        let schema = json!({
            "type": "object",
            "properties": {
                "endpoints": {
                    "type": "object",
                    "additionalProperties": {
                        "type": "string",
                        "format": "uri",
                    },
                },
            },
        });
        let out = sanitize_tool_schema(schema);
        assert!(
            out["properties"]["endpoints"]["additionalProperties"]
                .get("format")
                .is_none()
        );
    }

    /// Pin the recursion depth so a future "stop at the property
    /// level" refactor is caught.
    #[test]
    fn sanitize_tool_schema_strips_format_recursively() {
        let schema = json!({
            "type": "object",
            "properties": {
                "a": {
                    "type": "object",
                    "properties": {
                        "b": {
                            "type": "object",
                            "properties": {
                                "c": {
                                    "type": "string",
                                    "format": "uri",
                                },
                            },
                        },
                    },
                },
            },
        });
        let out = sanitize_tool_schema(schema);
        let deep = &out["properties"]["a"]["properties"]["b"]["properties"]["c"];
        assert!(deep.get("format").is_none());
    }

    /// Pin the contract: accepted formats are still stripped. The
    /// proxy drops ALL `format` keywords regardless of value, on the
    /// assumption that the model is the producer and the hint isn't
    /// load-bearing. If a future maintainer wants to preserve e.g.
    /// `date-time` for downstream consumers, the doc comment points
    /// at the allowlist check that should replace the unconditional
    /// strip.
    #[test]
    fn sanitize_tool_schema_strips_format_even_when_accepted() {
        let schema = json!({
            "type": "object",
            "properties": {
                "when": {
                    "type": "string",
                    "format": "date-time",
                },
                "id": {
                    "type": "string",
                    "format": "uuid",
                },
            },
        });
        let out = sanitize_tool_schema(schema);
        assert!(out["properties"]["when"].get("format").is_none());
        assert!(out["properties"]["id"].get("format").is_none());
    }

    // ─── type-injection tests ───────────────────────────────────────
    //
    // The strict Responses validator rejects every schema node that
    // lacks a `type` key. We default typeless property schemas to
    // `string` because that's the one type the model is guaranteed
    // to produce without further schema shape.

    /// Live regression: Claude Code's `Workflow` tool declares
    /// `properties.args: { description: "..." }` with no `type`. The
    /// strict validator rejects this with `schema must have a 'type'
    /// key`. The injection adds `type: "string"`.
    #[test]
    fn sanitize_tool_schema_injects_type_string_for_typeless_property() {
        let schema = json!({
            "type": "object",
            "properties": {
                "args": {
                    "description": "Optional input value exposed to the script as the global `args`.",
                },
            },
        });
        let out = sanitize_tool_schema(schema);
        let args = &out["properties"]["args"];
        assert_eq!(args["type"], "string");
        // Description is preserved.
        assert!(
            args.get("description").is_some(),
            "description should be preserved"
        );
    }

    /// `type` already present in any form (string OR array) is left
    /// alone. JSON Schema allows `type: ["string", "null"]` and the
    /// strict validator accepts that form.
    #[test]
    fn sanitize_tool_schema_does_not_overwrite_existing_type() {
        // string form
        let schema = json!({
            "type": "object",
            "properties": {
                "x": {"type": "number"},
            },
        });
        let out = sanitize_tool_schema(schema);
        assert_eq!(out["properties"]["x"]["type"], "number");

        // array form (union type)
        let schema = json!({
            "type": "object",
            "properties": {
                "y": {"type": ["string", "null"]},
            },
        });
        let out = sanitize_tool_schema(schema);
        assert_eq!(out["properties"]["y"]["type"], json!(["string", "null"]));
    }

    /// Pin the recursion: a typeless schema at any depth gets
    /// `type: "string"` injected. Pinned at three levels deep so a
    /// future "stop at the property level" refactor is caught.
    #[test]
    fn sanitize_tool_schema_injects_type_in_nested_property() {
        let schema = json!({
            "type": "object",
            "properties": {
                "outer": {
                    "type": "object",
                    "properties": {
                        "inner": {
                            "description": "no type here",
                        },
                    },
                },
            },
        });
        let out = sanitize_tool_schema(schema);
        let inner = &out["properties"]["outer"]["properties"]["inner"];
        assert_eq!(inner["type"], "string");
    }

    /// `additionalProperties` values that are typeless object
    /// schemas also get the injection. Same as the property case,
    /// but in the `additionalProperties` slot, which the strict
    /// validator recurses into.
    #[test]
    fn sanitize_tool_schema_injects_type_for_typeless_additional_properties() {
        let schema = json!({
            "type": "object",
            "properties": {
                "endpoints": {
                    "type": "object",
                    "additionalProperties": {
                        "description": "any value",
                    },
                },
            },
        });
        let out = sanitize_tool_schema(schema);
        let ap = &out["properties"]["endpoints"]["additionalProperties"];
        assert_eq!(ap["type"], "string");
    }

    /// No `properties` and no `required` → no synthesized `required`.
    /// The earlier "inject empty properties" branch handles the
    /// `properties` case; `required` only fills from a non-empty
    /// `properties` map.
    #[test]
    fn sanitize_tool_schema_no_required_when_no_properties() {
        let schema = json!({"type": "object"});
        let out = sanitize_tool_schema(schema);
        assert!(
            out.get("required").is_none(),
            "expected no required, got {:?}",
            out.get("required")
        );
    }

    #[test]
    fn prompt_caching_disabled_ignores_cache_control() {
        let mut req = fixture_request();
        req.messages = vec![MessageParam {
            role: MessageRole::User,
            content: MessageContent::Blocks(vec![ContentBlockParam::Text(TextBlockParam {
                text: "Hello".into(),
                cache_control: Some(CacheControl {
                    r#type: Some("ephemeral".into()),
                }),
            })]),
        }];
        let out = anthropic_to_responses(&req, None, &PromptCachingConfig::default()).unwrap();
        let items = unwrap_items(out);
        match &items[0] {
            InputItem::Message { content, .. } => match &content[0] {
                InputContentPart::InputText {
                    prompt_cache_breakpoint,
                    ..
                } => assert!(prompt_cache_breakpoint.is_none()),
                _ => panic!("expected input_text"),
            },
            _ => panic!("expected message"),
        }
    }

    #[test]
    fn prompt_caching_translates_ephemeral_text_to_breakpoint() {
        let mut req = fixture_request();
        req.messages = vec![MessageParam {
            role: MessageRole::User,
            content: MessageContent::Blocks(vec![ContentBlockParam::Text(TextBlockParam {
                text: "Hello".into(),
                cache_control: Some(CacheControl {
                    r#type: Some("ephemeral".into()),
                }),
            })]),
        }];
        let caching = PromptCachingConfig {
            models: vec!["test-model".to_string()],
            ..PromptCachingConfig::default()
        };
        let out = anthropic_to_responses(&req, None, &caching).unwrap();
        let items = unwrap_items(out);
        match &items[0] {
            InputItem::Message { content, .. } => match &content[0] {
                InputContentPart::InputText {
                    prompt_cache_breakpoint,
                    ..
                } => {
                    assert_eq!(
                        prompt_cache_breakpoint.as_ref().map(|b| b.mode.as_str()),
                        Some("explicit")
                    );
                }
                _ => panic!("expected input_text"),
            },
            _ => panic!("expected message"),
        }
    }

    #[test]
    fn prompt_caching_flushes_ephemeral_before_non_ephemeral_text() {
        // If an ephemeral text block is followed by a non-ephemeral
        // text block, they must not be merged into a single content part
        // with a trailing breakpoint — the breakpoint belongs at the
        // end of the first block's content.
        let mut req = fixture_request();
        req.messages = vec![MessageParam {
            role: MessageRole::User,
            content: MessageContent::Blocks(vec![
                ContentBlockParam::Text(TextBlockParam {
                    text: "cached".into(),
                    cache_control: Some(CacheControl {
                        r#type: Some("ephemeral".into()),
                    }),
                }),
                ContentBlockParam::Text(TextBlockParam {
                    text: "fresh".into(),
                    cache_control: None,
                }),
            ]),
        }];
        let caching = PromptCachingConfig {
            models: vec!["test-model".to_string()],
            ..PromptCachingConfig::default()
        };
        let out = anthropic_to_responses(&req, None, &caching).unwrap();
        let items = unwrap_items(out);
        match &items[0] {
            InputItem::Message { content, .. } => {
                assert_eq!(content.len(), 2, "expected two separate content parts");
                // First part ends at the cache breakpoint.
                match &content[0] {
                    InputContentPart::InputText {
                        text,
                        prompt_cache_breakpoint,
                        ..
                    } => {
                        assert_eq!(text, "cached");
                        assert!(prompt_cache_breakpoint.is_some());
                    }
                    _ => panic!("expected input_text"),
                }
                // Second part is fresh, no breakpoint.
                match &content[1] {
                    InputContentPart::InputText {
                        text,
                        prompt_cache_breakpoint,
                        ..
                    } => {
                        assert_eq!(text, "fresh");
                        assert!(prompt_cache_breakpoint.is_none());
                    }
                    _ => panic!("expected input_text"),
                }
            }
            _ => panic!("expected message"),
        }
    }

    #[test]
    fn prompt_caching_non_ephemeral_followed_by_ephemeral_merges() {
        // A non-ephemeral block followed by an ephemeral block can be
        // merged; the breakpoint just moves to the end of the part.
        let mut req = fixture_request();
        req.messages = vec![MessageParam {
            role: MessageRole::User,
            content: MessageContent::Blocks(vec![
                ContentBlockParam::Text(TextBlockParam {
                    text: "prefix".into(),
                    cache_control: None,
                }),
                ContentBlockParam::Text(TextBlockParam {
                    text: "cached".into(),
                    cache_control: Some(CacheControl {
                        r#type: Some("ephemeral".into()),
                    }),
                }),
            ]),
        }];
        let caching = PromptCachingConfig {
            models: vec!["test-model".to_string()],
            ..PromptCachingConfig::default()
        };
        let out = anthropic_to_responses(&req, None, &caching).unwrap();
        let items = unwrap_items(out);
        match &items[0] {
            InputItem::Message { content, .. } => {
                assert_eq!(content.len(), 1);
                match &content[0] {
                    InputContentPart::InputText {
                        text,
                        prompt_cache_breakpoint,
                        ..
                    } => {
                        assert_eq!(text, "prefix\ncached");
                        assert!(prompt_cache_breakpoint.is_some());
                    }
                    _ => panic!("expected input_text"),
                }
            }
            _ => panic!("expected message"),
        }
    }
}

// ─── Response translation (non-streaming) ─────────────────────────────────

use crate::responses::{OutputContentPart, OutputItem};

/// Translate an OpenAI `ResponsesResponse` into an Anthropic `Message`.
///
/// Translation rules:
/// - `id` is prefixed with `msg_` if it doesn't already start that way.
/// - `output[]` is walked in order:
///   - `Message { content: [OutputTextPart, ...] }` → for each
///     `OutputTextPart`, emit `ResponseContentBlock::Text { text }`.
///     `OutputRefusal` parts become a single text block prefixed with
///     `[refusal] ` (Anthropic has no refusal kind on the response
///     side today).
///   - `FunctionCall { call_id, name, arguments }` →
///     `ResponseContentBlock::ToolUse { id: call_id, name, input: parsed_args_or_raw_fallback }`.
///   - `Reasoning { encrypted_content, summary }` → a single
///     `ResponseContentBlock::Thinking { thinking: summary_text, signature: encrypted_content }`.
/// - `stop_reason` mapping:
///   - `status == "completed"` → `EndTurn` (or `ToolUse` if the response
///     contains a tool call).
///   - `status == "incomplete"` with `reason: "max_output_tokens"` → `MaxTokens`.
///   - `status == "incomplete"` with `reason: "content_filter"` → `EndTurn`.
///   - `status == "failed"` → `EndTurn`; the inner error message is
///     surfaced as a `[error] ` text block so the client sees it.
///   - `status == "cancelled"` → `EndTurn`.
/// - `usage`: `input_tokens` → `input_tokens`, `output_tokens` →
///   `output_tokens`, `output_tokens_details.reasoning_tokens` →
///   `thinking_tokens`. Cache fields are 0.
#[must_use]
pub fn responses_to_anthropic(resp: &ResponsesResponse) -> Message {
    let (mut content, web_search_used) = build_content_blocks(&resp.output);

    // If the response failed AND we have no output blocks of our own,
    // surface the upstream error message as a `[error] ...` text block
    // so the client sees what went wrong (instead of an empty message
    // body). If the response produced output before failing, we keep
    // the output and skip the synthetic error block — the model's
    // reply is more useful than the failure message in that case.
    if resp.status == "failed"
        && content.is_empty()
        && let Some(err) = &resp.error
    {
        content.push(ResponseContentBlock::Text {
            text: format!("[error] {}", err.message),
        });
    }

    let has_tool_use = content
        .iter()
        .any(|b| matches!(b, ResponseContentBlock::ToolUse { .. }));

    let stop_reason = map_status_to_stop_reason(&resp.status, resp.incomplete_details.as_ref());
    let stop_reason = if stop_reason == Some(StopReason::EndTurn) && has_tool_use {
        Some(StopReason::ToolUse)
    } else {
        stop_reason
    };

    let usage = resp
        .usage
        .as_ref()
        .map(|u| AnthropicUsage {
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
            cache_creation_input_tokens: u
                .input_tokens_details
                .as_ref()
                .map_or(0, |d| d.cache_write_tokens),
            cache_read_input_tokens: u
                .input_tokens_details
                .as_ref()
                .map_or(0, |d| d.cached_tokens),
            cache_creation_input_tokens_5m: 0,
            cache_creation_input_tokens_1h: 0,
            thinking_tokens: u
                .output_tokens_details
                .as_ref()
                .map_or(0, |d| d.reasoning_tokens),
            server_tool_use: if web_search_used {
                Some(ServerToolUseUsage { web_search_requests: 1 })
            } else {
                None
            },
        })
        .unwrap_or(AnthropicUsage {
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens_5m: 0,
            cache_creation_input_tokens_1h: 0,
            thinking_tokens: 0,
            server_tool_use: None,
        });

    Message {
        id: ensure_msg_prefix(&resp.id),
        r#type: "message",
        role: "assistant",
        content,
        model: resp.model.clone(),
        stop_reason,
        stop_sequence: None,
        usage,
    }
}

fn ensure_msg_prefix(id: &str) -> String {
    if id.starts_with("msg_") {
        id.to_owned()
    } else {
        format!("msg_{id}")
    }
}

fn map_status_to_stop_reason(
    status: &str,
    incomplete: Option<&crate::responses::IncompleteDetails>,
) -> Option<StopReason> {
    match status {
        "completed" | "cancelled" => Some(StopReason::EndTurn),
        "incomplete" => Some(match incomplete.map(|d| d.reason.as_str()).unwrap_or("") {
            "max_output_tokens" => StopReason::MaxTokens,
            "content_filter" => StopReason::EndTurn,
            _ => StopReason::EndTurn,
        }),
        "failed" => Some(StopReason::EndTurn),
        _ => {
            tracing::warn!(status = %status, "unknown Responses status; defaulting to EndTurn");
            Some(StopReason::EndTurn)
        }
    }
}

/// Extract web search citations from OpenAI `url_citation` annotations,
/// `web_search_call` output items, and `FunctionCall` items whose name
/// is `WebSearch`/`web_search`.
///
/// Returns a vector of (url, title) pairs. The vector is non-empty if
/// any web search signal is detected — this is what drives the
/// `web_search_used` flag in [`build_content_blocks`] and the
/// `server_tool_use` usage in the final message.
///
/// The primary detection signal is the upstream's `web_search_call`
/// output item (type: "web_search_call"), which the OpenAI Responses
/// API emits when the model uses its built-in web search. A secondary
/// signal is a `FunctionCall` whose name is `WebSearch` or `web_search`
/// (the model sometimes invokes web search as a function tool instead
/// of via the built-in). The tertiary signal — kept as a fallback — is
/// `url_citation` annotations on `OutputText` parts, which is what the
/// non-streaming response path populates once content is fully
/// materialized.
fn extract_web_search_citations(items: &[OutputItem]) -> Vec<(String, String)> {
    let mut citations = Vec::new();
    let mut saw_web_search_call = false;
    let mut saw_web_search_function_call = false;

    for item in items {
        match item {
            OutputItem::WebSearchCall { .. } => {
                tracing::debug!("detected web_search_call output item");
                saw_web_search_call = true;
            }
            OutputItem::FunctionCall { name, .. } => {
                if name == "WebSearch" || name == "web_search" {
                    tracing::debug!(name = %name, "detected web search FunctionCall");
                    saw_web_search_function_call = true;
                }
            }
            OutputItem::Message { content, .. } => {
                for part in content {
                    if let OutputContentPart::OutputText { text, annotations } = part {
                        tracing::debug!(
                            text_len = text.len(),
                            annotation_count = annotations.len(),
                            annotations = ?annotations,
                            "OutputText part with annotations"
                        );
                        for ann in annotations {
                            if ann.get("type").and_then(|v| v.as_str()) == Some("url_citation") {
                                if let (Some(url), Some(title)) = (
                                    ann.get("url").and_then(|v| v.as_str()),
                                    ann.get("title").and_then(|v| v.as_str()),
                                ) {
                                    citations.push((url.to_string(), title.to_string()));
                                }
                            }
                        }
                    }
                }
            }
            OutputItem::Unknown => {
                tracing::debug!("dropping unknown OutputItem (may be web_search_call)");
            }
            _ => {}
        }
    }

    // If we detected a web_search_call or a WebSearch function call but
    // have no url_citation annotations (the streaming path often has
    // none at this point, and some upstreams don't return citations on
    // the non-streaming response either), synthesize a placeholder
    // citation so `web_search_used` is still true and the
    // server_tool_use usage is reported. The placeholder URL/title are
    // not load-bearing — Claude Code's search counter only needs the
    // `server_tool_use` + `web_search_tool_result` block pair to exist.
    if citations.is_empty() && (saw_web_search_call || saw_web_search_function_call) {
        tracing::info!(
            saw_web_search_call,
            saw_web_search_function_call,
            "web search detected via output item; synthesizing placeholder citation"
        );
        citations.push(("https://www.openai.com".to_string(), "Web search".to_string()));
    }

    tracing::info!(
        citations_found = citations.len(),
        saw_web_search_call,
        saw_web_search_function_call,
        "web search citation extraction"
    );
    citations
}

fn build_content_blocks(items: &[OutputItem]) -> (Vec<ResponseContentBlock>, bool) {
    let citations = extract_web_search_citations(items);
    let web_search_used = !citations.is_empty();

    let mut blocks = Vec::new();

    // If web search was used, inject server_tool_use + web_search_tool_result
    // blocks before the text content so Claude Code's search counter works.
    if web_search_used {
        let tool_use_id = "stoolu_web_search_01".to_string();
        blocks.push(ResponseContentBlock::ServerToolUse {
            id: tool_use_id.clone(),
            name: "web_search".to_string(),
            input: serde_json::json!({"query": "Web search"}),
        });
        blocks.push(ResponseContentBlock::WebSearchToolResult {
            block: WebSearchToolResultBlock {
                tool_use_id,
                content: citations
                    .iter()
                    .map(|(url, title)| WebSearchResult {
                        uri: url.clone(),
                        title: title.clone(),
                        encrypted_content: String::new(),
                    })
                    .collect(),
            },
        });
    }
    for item in items {
        match item {
            OutputItem::Message { content, .. } => {
                for part in content {
                    match part {
                        OutputContentPart::OutputText { text, .. } => {
                            if !text.is_empty() {
                                blocks.push(ResponseContentBlock::Text { text: text.clone() });
                            }
                        }
                        OutputContentPart::OutputRefusal { refusal } => {
                            // No Anthropic refusal kind on the response side
                            // — surface as a text block with a clear prefix.
                            if !refusal.is_empty() {
                                blocks.push(ResponseContentBlock::Text {
                                    text: format!("[refusal] {refusal}"),
                                });
                            }
                        }
                        OutputContentPart::SummaryText { text } => {
                            if !text.is_empty() {
                                blocks.push(ResponseContentBlock::Text { text: text.clone() });
                            }
                        }
                        OutputContentPart::Unknown => {}
                    }
                }
            }
            OutputItem::FunctionCall {
                call_id,
                name,
                arguments,
                ..
            } => {
                let input = serde_json::from_str(arguments)
                    .unwrap_or_else(|_| serde_json::json!({ "_raw_arguments": arguments }));
                blocks.push(ResponseContentBlock::ToolUse {
                    id: call_id.clone(),
                    name: name.clone(),
                    input,
                });
            }
            OutputItem::Reasoning {
                summary,
                encrypted_content,
                ..
            } => {
                let thinking = summary
                    .iter()
                    .map(|s| s.text.as_str())
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join("");
                blocks.push(ResponseContentBlock::Thinking {
                    thinking,
                    signature: encrypted_content.clone(),
                });
            }
            // The web_search_call item itself doesn't translate to an
            // Anthropic content block — its presence is what drives
            // the `server_tool_use` + `web_search_tool_result` block
            // injection above (via `extract_web_search_citations`).
            // Skip it here.
            OutputItem::WebSearchCall { .. } => {}
            OutputItem::Unknown => {}
        }
    }
    (blocks, web_search_used)
}

#[cfg(test)]
mod response_tests {
    use super::*;
    use crate::responses::{
        IncompleteDetails, InputTokensDetails, OutputContentPart, OutputItem, OutputTokensDetails,
        ResponsesError, ResponsesResponse, ResponsesUsage,
    };
    use serde_json::json;

    fn fixture_response() -> ResponsesResponse {
        ResponsesResponse {
            id: "resp_abc123".into(),
            object: "response".into(),
            created_at: 1_700_000_000,
            status: "completed".into(),
            model: "gpt-5.6-luna".into(),
            output: vec![OutputItem::Message {
                id: Some("msg_x".into()),
                status: Some("completed".into()),
                role: "assistant".into(),
                content: vec![OutputContentPart::OutputText {
                    text: "Hello there!".into(),
                    annotations: vec![],
                }],
            }],
            usage: Some(ResponsesUsage {
                input_tokens: 12,
                output_tokens: 3,
                total_tokens: 15,
                input_tokens_details: None,
                output_tokens_details: None,
            }),
            error: None,
            incomplete_details: None,
        }
    }

    #[test]
    fn text_response_maps_to_message() {
        let out = responses_to_anthropic(&fixture_response());
        assert_eq!(out.id, "msg_resp_abc123");
        assert_eq!(out.r#type, "message");
        assert_eq!(out.role, "assistant");
        assert_eq!(out.model, "gpt-5.6-luna");
        assert_eq!(out.stop_reason, Some(StopReason::EndTurn));
        assert_eq!(out.usage.input_tokens, 12);
        assert_eq!(out.usage.output_tokens, 3);
        assert_eq!(out.content.len(), 1);
        match &out.content[0] {
            ResponseContentBlock::Text { text } => assert_eq!(text, "Hello there!"),
            _ => panic!("expected text block"),
        }
    }

    #[test]
    fn function_call_becomes_tool_use_block() {
        let mut resp = fixture_response();
        resp.output = vec![
            OutputItem::Message {
                id: Some("msg_x".into()),
                status: Some("completed".into()),
                role: "assistant".into(),
                content: vec![OutputContentPart::OutputText {
                    text: "Calling tool".into(),
                    annotations: vec![],
                }],
            },
            OutputItem::FunctionCall {
                id: Some("fc_x".into()),
                status: Some("completed".into()),
                call_id: "call_xyz".into(),
                name: "get_weather".into(),
                arguments: r#"{"location":"SF"}"#.into(),
            },
        ];
        let out = responses_to_anthropic(&resp);
        assert_eq!(out.stop_reason, Some(StopReason::ToolUse));
        assert_eq!(out.content.len(), 2);
        match &out.content[0] {
            ResponseContentBlock::Text { text } => assert_eq!(text, "Calling tool"),
            _ => panic!("expected text block first"),
        }
        match &out.content[1] {
            ResponseContentBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "call_xyz");
                assert_eq!(name, "get_weather");
                assert_eq!(input["location"], "SF");
            }
            _ => panic!("expected tool_use block second"),
        }
    }

    #[test]
    fn tool_only_response_has_no_text_block() {
        let mut resp = fixture_response();
        resp.output = vec![OutputItem::FunctionCall {
            id: Some("fc_x".into()),
            status: Some("completed".into()),
            call_id: "call_1".into(),
            name: "noop".into(),
            arguments: "{}".into(),
        }];
        let out = responses_to_anthropic(&resp);
        // Just the tool_use; no empty text block.
        assert_eq!(out.content.len(), 1);
        assert!(matches!(
            out.content[0],
            ResponseContentBlock::ToolUse { .. }
        ));
    }

    #[test]
    fn malformed_tool_arguments_fall_back_to_raw() {
        let mut resp = fixture_response();
        resp.output = vec![OutputItem::FunctionCall {
            id: Some("fc_x".into()),
            status: Some("completed".into()),
            call_id: "call_bad".into(),
            name: "bad".into(),
            arguments: "not valid json".into(),
        }];
        let out = responses_to_anthropic(&resp);
        match &out.content[0] {
            ResponseContentBlock::ToolUse { input, .. } => {
                assert!(input.get("_raw_arguments").is_some());
            }
            _ => panic!("expected tool_use block"),
        }
    }

    #[test]
    fn reasoning_item_becomes_thinking_block() {
        let mut resp = fixture_response();
        resp.output = vec![
            OutputItem::Reasoning {
                id: Some("rs_x".into()),
                summary: vec![crate::responses::ReasoningSummaryText {
                    text: "thinking out loud".into(),
                    kind: Some("summary_text".into()),
                }],
                encrypted_content: "opaque-sig-blob".into(),
            },
            OutputItem::Message {
                id: Some("msg_x".into()),
                status: Some("completed".into()),
                role: "assistant".into(),
                content: vec![OutputContentPart::OutputText {
                    text: "answer".into(),
                    annotations: vec![],
                }],
            },
        ];
        let out = responses_to_anthropic(&resp);
        assert_eq!(out.content.len(), 2);
        match &out.content[0] {
            ResponseContentBlock::Thinking {
                thinking,
                signature,
            } => {
                assert_eq!(thinking, "thinking out loud");
                assert_eq!(signature, "opaque-sig-blob");
            }
            _ => panic!("expected thinking block first"),
        }
        match &out.content[1] {
            ResponseContentBlock::Text { text } => assert_eq!(text, "answer"),
            _ => panic!("expected text block second"),
        }
    }

    #[test]
    fn incomplete_response_max_tokens_maps_to_max_tokens() {
        let mut resp = fixture_response();
        resp.status = "incomplete".into();
        resp.incomplete_details = Some(IncompleteDetails {
            reason: "max_output_tokens".into(),
        });
        let out = responses_to_anthropic(&resp);
        assert_eq!(out.stop_reason, Some(StopReason::MaxTokens));
    }

    #[test]
    fn incomplete_response_content_filter_maps_to_end_turn() {
        let mut resp = fixture_response();
        resp.status = "incomplete".into();
        resp.incomplete_details = Some(IncompleteDetails {
            reason: "content_filter".into(),
        });
        let out = responses_to_anthropic(&resp);
        assert_eq!(out.stop_reason, Some(StopReason::EndTurn));
    }

    #[test]
    fn failed_response_surfaces_error_message_in_text_block() {
        let mut resp = fixture_response();
        resp.status = "failed".into();
        resp.error = Some(ResponsesError {
            message: "upstream rejected".into(),
            kind: "server_error".into(),
            code: None,
            param: None,
        });
        resp.output = vec![];
        let out = responses_to_anthropic(&resp);
        assert_eq!(out.stop_reason, Some(StopReason::EndTurn));
        assert_eq!(out.content.len(), 1);
        match &out.content[0] {
            ResponseContentBlock::Text { text } => {
                assert!(text.contains("[error]"));
                assert!(text.contains("upstream rejected"));
            }
            _ => panic!("expected error text block"),
        }
    }

    #[test]
    fn usage_thinking_tokens_surfaces() {
        let mut resp = fixture_response();
        resp.usage = Some(ResponsesUsage {
            input_tokens: 100,
            output_tokens: 50,
            total_tokens: 150,
            input_tokens_details: Some(InputTokensDetails {
                cached_tokens: 10,
                cache_write_tokens: 0,
            }),
            output_tokens_details: Some(OutputTokensDetails {
                reasoning_tokens: 14,
            }),
        });
        let out = responses_to_anthropic(&resp);
        assert_eq!(out.usage.thinking_tokens, 14);
    }

    #[test]
    fn cache_usage_fields_map_from_input_details() {
        let mut resp = fixture_response();
        resp.usage = Some(ResponsesUsage {
            input_tokens: 100,
            output_tokens: 50,
            total_tokens: 150,
            input_tokens_details: Some(InputTokensDetails {
                cached_tokens: 20,
                cache_write_tokens: 7,
            }),
            output_tokens_details: None,
        });
        let out = responses_to_anthropic(&resp);
        assert_eq!(out.usage.input_tokens, 100);
        assert_eq!(out.usage.output_tokens, 50);
        assert_eq!(out.usage.cache_read_input_tokens, 20);
        assert_eq!(out.usage.cache_creation_input_tokens, 7);
    }

    #[test]
    fn missing_usage_zeros_tokens() {
        let mut resp = fixture_response();
        resp.usage = None;
        let out = responses_to_anthropic(&resp);
        assert_eq!(out.usage.input_tokens, 0);
        assert_eq!(out.usage.output_tokens, 0);
    }

    #[test]
    fn id_without_msg_prefix_gets_one() {
        let resp = fixture_response();
        let out = responses_to_anthropic(&resp);
        assert!(out.id.starts_with("msg_"));
    }

    #[test]
    fn id_already_prefixed_is_not_double_prefixed() {
        let mut resp = fixture_response();
        resp.id = "msg_already".into();
        let out = responses_to_anthropic(&resp);
        assert_eq!(out.id, "msg_already");
    }

    #[test]
    fn output_refusal_becomes_text_with_prefix() {
        let mut resp = fixture_response();
        resp.output = vec![OutputItem::Message {
            id: Some("msg_x".into()),
            status: Some("completed".into()),
            role: "assistant".into(),
            content: vec![OutputContentPart::OutputRefusal {
                refusal: "I won't do that".into(),
            }],
        }];
        let out = responses_to_anthropic(&resp);
        assert_eq!(out.content.len(), 1);
        match &out.content[0] {
            ResponseContentBlock::Text { text } => {
                assert!(text.starts_with("[refusal] "));
                assert!(text.contains("I won't do that"));
            }
            _ => panic!("expected text block"),
        }
    }

    #[test]
    fn failed_status_with_existing_output_keeps_output() {
        // If `status == "failed"` AND there is output (the model
        // produced something before the error), the output blocks are
        // kept and stop_reason is EndTurn.
        let mut resp = fixture_response();
        resp.status = "failed".into();
        resp.error = Some(ResponsesError {
            message: "post-output error".into(),
            kind: "server_error".into(),
            code: None,
            param: None,
        });
        let out = responses_to_anthropic(&resp);
        assert_eq!(out.stop_reason, Some(StopReason::EndTurn));
        assert_eq!(out.content.len(), 1);
        assert!(matches!(out.content[0], ResponseContentBlock::Text { .. }));
    }

    #[test]
    fn web_search_tool_choice_translates_to_auto() {
        // Claude Code's WebSearch tool is sent as a built-in server
        // tool (no input_schema). The proxy translates it to OpenAI's
        // native `{type: "web_search"}` tool. If the inbound
        // `tool_choice` is `{type: "tool", name: "web_search"}`, the
        // Responses API would reject `{type: "function", name:
        // "web_search"}` because the tools array contains a web_search
        // built-in, not a function. Verify the proxy falls back to
        // `"auto"` for this case.
        let choice = ToolChoice::Tool {
            name: "web_search".into(),
            disable_parallel_tool_use: None,
        };
        let mapped = map_tool_choice(&choice);
        match mapped {
            ResponsesToolChoice::Simple(s) => assert_eq!(s, "auto"),
            other => panic!("expected Simple(\"auto\"), got {other:?}"),
        }
    }

    #[test]
    fn function_tool_choice_keeps_function_form() {
        // Regular function tools (with input_schema) must still be
        // translated to `{type: "function", name: "..."}` so the
        // Responses API routes the choice to the right function.
        let choice = ToolChoice::Tool {
            name: "get_weather".into(),
            disable_parallel_tool_use: None,
        };
        let mapped = map_tool_choice(&choice);
        match mapped {
            ResponsesToolChoice::Function { kind, name } => {
                assert_eq!(kind, "function");
                assert_eq!(name, "get_weather");
            }
            other => panic!("expected Function, got {other:?}"),
        }
    }

    /// Regression: when the upstream emits a `web_search_call` output
    /// item (the OpenAI Responses API's signal that the model used its
    /// built-in web search), the proxy must inject
    /// `server_tool_use` + `web_search_tool_result` blocks into the
    /// Anthropic response and report `server_tool_use` usage, so
    /// Claude Code's search counter registers the search. Previously
    /// these items deserialized as `OutputItem::Unknown` and were
    /// dropped, causing Claude Code to show "Did 0 searches".
    #[test]
    fn web_search_call_output_item_injects_blocks_and_usage() {
        let mut resp = fixture_response();
        resp.output = vec![
            OutputItem::WebSearchCall {
                id: Some("ws_01".into()),
                status: Some("completed".into()),
            },
            OutputItem::Message {
                id: Some("msg_x".into()),
                status: Some("completed".into()),
                role: "assistant".into(),
                content: vec![OutputContentPart::OutputText {
                    text: "Based on my search...".into(),
                    annotations: vec![],
                }],
            },
        ];
        let out = responses_to_anthropic(&resp);

        // server_tool_use + web_search_tool_result + text = 3 blocks.
        assert_eq!(out.content.len(), 3, "expected 3 content blocks");
        match &out.content[0] {
            ResponseContentBlock::ServerToolUse { id, name, input } => {
                assert_eq!(id, "stoolu_web_search_01");
                assert_eq!(name, "web_search");
                assert_eq!(input["query"], "Web search");
            }
            other => panic!("expected ServerToolUse, got {other:?}"),
        }
        match &out.content[1] {
            ResponseContentBlock::WebSearchToolResult { block } => {
                assert_eq!(block.tool_use_id, "stoolu_web_search_01");
                // Placeholder citation synthesized because no
                // url_citation annotations were present.
                assert_eq!(block.content.len(), 1);
                assert!(!block.content[0].uri.is_empty());
            }
            other => panic!("expected WebSearchToolResult, got {other:?}"),
        }
        match &out.content[2] {
            ResponseContentBlock::Text { text } => {
                assert_eq!(text, "Based on my search...");
            }
            other => panic!("expected Text, got {other:?}"),
        }

        // server_tool_use usage must be present so the search counter works.
        let stu = out
            .usage
            .server_tool_use
            .as_ref()
            .expect("server_tool_use usage should be present");
        assert_eq!(stu.web_search_requests, 1);
    }

    /// Regression: when the upstream emits a `FunctionCall` whose name is
    /// `WebSearch` (the model invoking web search as a function tool
    /// instead of via the built-in `web_search_call` item), the proxy
    /// must also inject the web search blocks and report usage. The
    /// function call's own `tool_use` block is still emitted afterward.
    #[test]
    fn web_search_function_call_injects_blocks_and_keeps_tool_use() {
        let mut resp = fixture_response();
        resp.output = vec![
            OutputItem::FunctionCall {
                id: Some("fc_x".into()),
                status: Some("completed".into()),
                call_id: "call_ws".into(),
                name: "WebSearch".into(),
                arguments: r#"{"query":"rust async"}"#.into(),
            },
            OutputItem::Message {
                id: Some("msg_x".into()),
                status: Some("completed".into()),
                role: "assistant".into(),
                content: vec![OutputContentPart::OutputText {
                    text: "Result".into(),
                    annotations: vec![],
                }],
            },
        ];
        let out = responses_to_anthropic(&resp);

        // server_tool_use + web_search_tool_result (injected) +
        // tool_use (the WebSearch function call) + text = 4 blocks.
        assert_eq!(out.content.len(), 4, "expected 4 content blocks");
        assert!(matches!(&out.content[0], ResponseContentBlock::ServerToolUse { .. }));
        assert!(matches!(&out.content[1], ResponseContentBlock::WebSearchToolResult { .. }));
        match &out.content[2] {
            ResponseContentBlock::ToolUse { id, name, .. } => {
                assert_eq!(id, "call_ws");
                assert_eq!(name, "WebSearch");
            }
            other => panic!("expected ToolUse for the function call, got {other:?}"),
        }
        assert!(matches!(&out.content[3], ResponseContentBlock::Text { .. }));

        // stop_reason is ToolUse because a FunctionCall is present.
        assert_eq!(out.stop_reason, Some(StopReason::ToolUse));
        // server_tool_use usage present.
        assert!(out.usage.server_tool_use.is_some());
    }

    /// The `web_search` lowercase function name variant is also
    /// detected. Pin this so a future rename doesn't quietly break the
    /// lowercase form.
    #[test]
    fn web_search_function_call_lowercase_name_is_detected() {
        let mut resp = fixture_response();
        resp.output = vec![OutputItem::FunctionCall {
            id: Some("fc_x".into()),
            status: Some("completed".into()),
            call_id: "call_ws".into(),
            name: "web_search".into(),
            arguments: "{}".into(),
        }];
        let out = responses_to_anthropic(&resp);
        // server_tool_use + web_search_tool_result + tool_use = 3 blocks.
        assert_eq!(out.content.len(), 3);
        assert!(matches!(&out.content[0], ResponseContentBlock::ServerToolUse { .. }));
        assert!(matches!(&out.content[1], ResponseContentBlock::WebSearchToolResult { .. }));
        assert!(out.usage.server_tool_use.is_some());
    }
}
