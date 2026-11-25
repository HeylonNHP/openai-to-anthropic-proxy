//! Translation between Anthropic Messages API and OpenAI Responses.
//!
//! Three entry points:
//! - [`anthropic_to_responses_with`] — inbound request, used by the proxy's
//!   `/v1/messages` handler. Converts Claude Code's request into the
//!   shape the upstream expects. (`anthropic_to_responses` is a thin
//!   convenience wrapper used by the unit tests.)
//! - [`responses_to_anthropic`] — non-streaming response. The upstream
//!   returned a full `ResponsesResponse`; we map it to Anthropic's
//!   `Message` and ship it back to the client.
//! - [`StreamTranslator`] (in `crate::stream`) — turns an OpenAI
//!   Responses SSE event stream into Anthropic SSE events.
//!
//! Note: image blocks in *user* messages are translated to `input_image`
//! parts (`base64` data URIs or passthrough URLs). Image blocks inside a
//! `tool_result` are handled per [`ToolResultImages`]: forwarded inline as
//! `input_image` parts in the `function_call_output` array form (default),
//! lifted into a follow-up user message for gateways that reject array
//! outputs, or dropped. Images in *assistant* turns have no valid home in
//! the Responses API and are dropped with a warning.
//!
//! On the response side, OpenAI's `usage.input_tokens` (gross, cache included)
//! becomes Anthropic's `input_tokens` NET of the cache counts, because
//! Anthropic's `input_tokens` excludes them and clients add the three fields
//! together; `cached_tokens` / `cache_write_tokens` become
//! `cache_read_input_tokens` / `cache_creation_input_tokens`.
//! `output_tokens` stays `output_tokens`, and
//! `output_tokens_details.reasoning_tokens` becomes `thinking_tokens`.

use crate::anthropic::{
    CacheControl, ContentBlockParam, CreateMessageRequest, DocumentBlockParam, ImageBlockParam,
    Message, MessageContent, MessageParam, MessageRole, ResponseContentBlock, ServerToolUseUsage,
    StopReason, SystemPrompt, ToolChoice, ToolResultBlockParam, ToolResultContent,
    Usage as AnthropicUsage, WebSearchResult, WebSearchToolResultBlock,
};
use crate::config::{
    PromptCachingConfig, ReasoningDecision, ReasoningReplay, ReasoningReplayMode,
    SystemMessagePlacement, ToolResultImages,
};
use crate::responses::{
    FunctionCallOutputBody, Input, InputContentPart, InputItem, PromptCacheBreakpoint,
    ReasoningConfig, ReasoningSummaryPart, ReasoningTextPart, ResponsesRequest, ResponsesResponse,
    ResponsesTool, ToolChoice as ResponsesToolChoice, ToolDefinition, WebSearchTool,
};
use crate::schema_refs::resolve_local_refs;
use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use serde_json::{Value, json};

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
/// **Why 64 (not e.g. 60):**
/// - The 64 limit is the upstream's published max; we set the constant
///   to match exactly so a future upstream version that raises or lowers it
///   is a one-line change.
/// - We truncate at the last valid UTF-8 boundary at or before byte 64.
///   This preserves the leading prefix without panicking on non-ASCII
///   workspace paths.
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
        let cutoff = (0..=USER_ID_MAX_LEN)
            .rev()
            .find(|&idx| s.is_char_boundary(idx))
            .expect("0 is always a UTF-8 character boundary");
        s[..cutoff].to_owned()
    }
}

/// Translate a Claude Code request into the upstream OpenAI Responses shape.
///
/// Translation rules:
/// - `system` (string or text blocks) → `instructions`.
/// - `messages[]` → `input: Items(...)`. Walk in order:
///   - `user` text → one `Message { role: "user", content: [InputTextPart] }`.
///   - `user` block content → one Message per run of text blocks; for each
///     `ToolResult`, emit a `FunctionCallOutput { call_id, output }` *after*
///     any pending text. Text-only results keep the string form; results
///     carrying images use the array-of-parts form per `tool_result_images`.
///   - `assistant` text → `Message { role: "assistant", content: [OutputTextPart { text }] }`.
///   - `assistant` text + `tool_use` → `Message { content: [OutputTextPart] }` followed by
///     `FunctionCall { call_id: id, name, arguments: json_string }` per tool use, in order.
///   - `assistant` only `tool_use` → same with empty `content: []`.
///   - `role: "system"` messages (legacy) → concatenated into `instructions`.
/// - `tools[]` → `tools[]` with `parameters` lifted from `input_schema`.
///   **The big behavior change:** every tool now carries
///   `strict: true` + `additionalProperties: false` on the parameters
///   schema, which is what the airia gateway requires for function tools
///   on `/v1/responses`. A tool the client explicitly marks `strict: false`
///   is sent `strict: false` (with the same sanitized schema), and so does
///   not count against the strict property budget.
/// - `tool_choice`: `Auto`→`"auto"`, `Any`→`"required"`, `None`→`"none"`,
///   `Tool{name}`→`{ type: "function", name: "..." }`.
/// - `max_tokens` → `max_output_tokens`.
/// - `temperature`, `top_p` pass through.
/// - `reasoning_decision` (config-resolved) → `reasoning: Some(ReasoningConfig { effort })`
///   when it carries an effort, or `reasoning: None` when it is [`ReasoningDecision::Omit`].
///   Responses accepts `"none"` natively, so we forward whatever the decision
///   says — no per-request "downgrade" like the Chat Completions path did.
/// - `metadata.user_id` → `user`.
/// - `stream: true` → `stream: true` (no `stream_options`; Responses carries
///   usage on the terminal `Completed` event).
/// - `stop_sequences` dropped with a warn (Responses has no `stop` field).
/// - `thinking` blocks in an assistant turn → replayed upstream according to
///   `replay`. In [`ReasoningReplayMode::Text`] the reasoning text is folded
///   into the assistant message's single `output_text` part (reasoning
///   first, then the visible answer); in [`ReasoningReplayMode::Native`] a
///   Responses `reasoning` input item is emitted immediately before the
///   assistant message; in [`ReasoningReplayMode::Off`] the block is dropped.
///
/// Unit-test convenience wrapper around [`anthropic_to_responses_with`]; the
/// proxy itself calls `_with` with the configured `tool_result_images` mode.
#[allow(dead_code)]
pub fn anthropic_to_responses(
    req: &CreateMessageRequest,
    reasoning_decision: Option<ReasoningDecision>,
    prompt_caching: &PromptCachingConfig,
    replay: &ReasoningReplay,
) -> Result<(ResponsesRequest, crate::repair::ToolSchemaRegistry)> {
    anthropic_to_responses_with(
        req,
        reasoning_decision,
        prompt_caching,
        replay,
        ToolResultImages::default(),
        false,
        SystemMessagePlacement::default(),
    )
}

/// Map the client's `web_search` declaration onto OpenAI's built-in search
/// tool, carrying over the options OpenAI has an equivalent for.
///
/// * `allowed_domains` → `filters.allowed_domains`
/// * `user_location` → `user_location` (the two shapes agree)
///
/// OpenAI has no equivalent for `blocked_domains` or `max_uses`; the former
/// is a real loss (the model may be shown a site the client blocked), so it
/// is logged. Only options the client actually set are sent, so a plain
/// declaration produces exactly the tool it always did.
fn native_web_search_tool(tool: &crate::anthropic::Tool) -> WebSearchTool {
    let options = &tool.options;
    if options.blocked_domains.is_some() {
        tracing::warn!(
            "web_search `blocked_domains` is not supported by the upstream's built-in search \
             and was not applied"
        );
    }
    if options.max_uses.is_some() {
        tracing::debug!("web_search `max_uses` has no upstream equivalent and was not applied");
    }
    WebSearchTool {
        kind: "web_search".to_string(),
        search_context_size: Some("medium".to_string()),
        user_location: options.user_location.clone(),
        filters: options
            .allowed_domains
            .clone()
            .map(|allowed_domains| crate::responses::WebSearchFilters { allowed_domains }),
    }
}

/// As [`anthropic_to_responses`], with an explicit `tool_result_images`
/// mode (the proxy passes the operator's configured value).
pub fn anthropic_to_responses_with(
    req: &CreateMessageRequest,
    reasoning_decision: Option<ReasoningDecision>,
    prompt_caching: &PromptCachingConfig,
    replay: &ReasoningReplay,
    tool_result_images: ToolResultImages,
    forward_documents: bool,
    system_messages: SystemMessagePlacement,
) -> Result<(ResponsesRequest, crate::repair::ToolSchemaRegistry)> {
    let stream = req.stream.unwrap_or(false);
    let mut items: Vec<InputItem> = Vec::new();
    let has_advisor_tool = req
        .tools
        .as_ref()
        .is_some_and(|tools| tools.iter().any(|t| t.name == "advisor"));

    // Top-level `system` → `instructions`. Legacy `role: "system"`
    // messages are folded in below.
    let mut instructions = req.system.as_ref().map(system_text).unwrap_or_default();

    for msg in &req.messages {
        // A system message in the middle of the conversation stays where it
        // is, so the start of the prompt does not change (see
        // `SystemMessagePlacement`).
        if msg.role == MessageRole::System && system_messages == SystemMessagePlacement::Inline {
            let mut text = system_message_text(&msg.content);
            if !has_advisor_tool {
                strip_advisor_tool_instructions(&mut text);
            }
            if !text.is_empty() {
                items.push(InputItem::Message {
                    role: "system".to_owned(),
                    content: vec![InputContentPart::InputText {
                        text,
                        prompt_cache_breakpoint: None,
                    }],
                });
            }
            continue;
        }
        append_message_translation(
            &mut items,
            &mut instructions,
            msg,
            prompt_caching,
            replay,
            &tool_result_images,
            forward_documents,
        )?;
    }

    if instructions.is_empty() {
        instructions.clear();
    }

    // Claude Code may include a `# Advisor Tool` system section describing
    // the Anthropic server-side advisor tool. The schema-less declaration
    // has no `input_schema` and is dropped by the filter_map below, so
    // forwarding the instructions would tell the model to call a tool that
    // does not exist. Strip the section (and only the section) when no
    // advisor tool is actually being sent upstream. This is exactly the
    // single-section strip introduced at 17f8329, and MUST stay that way
    // for byte identity. The proxy re-applies this strip AFTER translation
    // only when the translator skipped it but the pipeline is inactive
    // (`enabled:false` + schema-less declaration, B9).
    if !has_advisor_tool {
        strip_advisor_tool_instructions(&mut instructions);
    }

    // Remember the *original* (client-supplied) tool schemas so the
    // response path can invert the strict sanitization below
    // (`crate::repair`). The registry is returned alongside the request
    // and threaded through both response paths by the proxy.
    let mut registry = crate::repair::ToolSchemaRegistry::new();
    let mut tools: Option<Vec<ToolDefinition>> = req.tools.as_ref().map(|tools| {
        tools
            .iter()
            .filter_map(|t| {
                // Anthropic server-side web_search tool → OpenAI native web_search.
                // Claude Code's declaration has no input_schema (a versioned
                // `type` marks one too); a client function tool that merely
                // shares the name has a schema, so it stays a function tool.
                if t.is_server_tool("web_search", "web_search_") {
                    return Some(ToolDefinition::WebSearch(native_web_search_tool(t)));
                }

                // Resolve `$ref`s first so the sanitizer below and the
                // repair registry both see the real, fully-typed schema.
                let input_schema = resolve_local_refs(t.input_schema.clone()?);
                registry.insert(t.name.clone(), input_schema.clone());
                let original = registry.get(&t.name);
                Some(ToolDefinition::Function(ResponsesTool {
                    kind: "function".to_string(),
                    name: t.name.clone(),
                    description: t.description.clone(),
                    // Strict unless the client explicitly asked for non-strict.
                    // Anthropic's own default is non-strict, so an absent flag
                    // (Claude Code sends none) must stay strict: that is what
                    // the upstream needs. The schema is sanitized either way,
                    // as it is when the budget below demotes a tool.
                    strict: t.strict != Some(false),
                    parameters: sanitize_tool_schema(input_schema, original),
                }))
            })
            .collect()
    });
    if let Some(list) = tools.as_mut() {
        enforce_strict_property_budget(list, STRICT_PROPERTY_BUDGET);
    }

    // `tool_choice` naming `web_search` is downgraded to `auto` only when the
    // upstream gets the built-in search; a client function tool of that name
    // is a normal function and can be forced.
    let native_web_search = req.tools.as_ref().is_some_and(|tools| {
        tools
            .iter()
            .any(|t| t.is_server_tool("web_search", "web_search_"))
    });
    let (tool_choice, parallel_tool_calls) = req.tool_choice.as_ref().map_or((None, None), |c| {
        let (tc, disable) = map_tool_choice(c, native_web_search);
        // Anthropic's `disable_parallel_tool_use: true` means "the
        // model may call at most one tool per turn." On Responses,
        // that's `parallel_tool_calls: false`. We only forward the
        // field when the client explicitly set the Anthropic side
        // — `None` leaves it to the upstream's default.
        let parallel = disable.map(|d| !d);
        (Some(tc), parallel)
    });

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

    let outbound = ResponsesRequest {
        model: req.model.clone(),
        input: Input::Items(items),
        instructions: if instructions.is_empty() {
            None
        } else {
            Some(instructions)
        },
        tools,
        tool_choice,
        reasoning: reasoning_decision
            .and_then(|d| d.upstream_effort().map(str::to_owned))
            .map(|effort| ReasoningConfig {
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
        parallel_tool_calls,
        store: None,
        previous_response_id: None,
        user,
        stream,
        metadata: None,
    };
    Ok((outbound, registry))
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

/// Remove the FIRST Claude Code `# Advisor Tool` section from the assembled
/// `instructions`, leaving every byte before and after it untouched.
///
/// Claude Code injects this section into the system prompt whenever the
/// advisor is enabled. It is a markdown section ending immediately before
/// the next `<system-reminder>`, or — in the variant where Claude Code has
/// already inlined the skills reminder as plain text — before the line
/// `The following skills are available for use with the Skill tool:`.
/// It is never the last thing in the prompt in real traffic, but we also
/// handle a section that runs to the end of the string.
///
/// Exactly ONE section is removed per call: that is the `17f8329` behaviour
/// the `enabled:false` / no-declaration path must keep byte-for-byte. Callers
/// that need the B9 schema-less-declaration case invoke it ONCE themselves
/// after translation; no caller invokes it twice on the same string. With a
/// single-section prompt that makes the call idempotent in practice (the
/// second call finds no heading), and a malformed bare heading is left in
/// place rather than causing an infinite loop.
///
/// We deliberately do NOT replace it with the "advisor tool is no longer
/// available" notice Claude Code itself uses: the executor never saw a
/// working advisor through this proxy, so there is no earlier instruction
/// to retract. Deleting the section is the least surprising outcome.
pub fn strip_advisor_tool_instructions(instructions: &mut String) {
    const HEADING: &str = "# Advisor Tool";
    const SKILLS: &str = "The following skills are available for use with the Skill tool:";

    // Find a heading that starts a line. Prose quoting the string inline
    // must not be treated as a section boundary.
    let mut search_from = 0;
    let mut start = None;
    while let Some(rel) = instructions[search_from..].find(HEADING) {
        let at = search_from + rel;
        if at == 0 || instructions.as_bytes()[at - 1] == b'\n' {
            start = Some(at);
            break;
        }
        search_from = at + HEADING.len();
    }
    let Some(start) = start else {
        return;
    };

    let tail = &instructions[start..];
    // The section ends where the next injected block begins. In real
    // traffic that is a `<system-reminder>`; in the variant where Claude
    // Code has already inlined the skills reminder it is the SKILLS line.
    let end_rel = tail
        .find("<system-reminder>")
        .or_else(|| tail.find(SKILLS))
        .unwrap_or(tail.len());
    let end = start + end_rel;
    if end <= start + HEADING.len() {
        return; // heading only / malformed — leave it alone
    }
    let removed = end - start;
    instructions.replace_range(start..end, "");
    tracing::debug!(
        removed_bytes = removed,
        "stripped the '# Advisor Tool' instructions; this proxy does not offer the advisor server tool"
    );
}

/// Total object properties, across every strict tool in one request, that
/// the upstream is asked to compile.
///
/// Strict mode makes the upstream compile each tool's schema, and it caps the
/// total. OpenAI documents "up to 5000 object properties total". Past the
/// cap this gateway does not return a schema error: it answers `status:
/// "incomplete"`, `incomplete_details.reason: "max_output_tokens"`, an empty
/// `output` and all-zero `usage`, which Claude Code renders as "response
/// exceeded the output token maximum". Measured live (Airia gateway,
/// `gpt-5.6-luna`, counting every key of every `properties` map): requests
/// up to 6,164 properties completed and requests from 6,412 failed
/// deterministically. 5,000 is the documented figure and leaves that
/// observed cliff about 20% of headroom.
///
/// A budget on the *total* is what matters: the 207 MCP tools of one server
/// (5,531 properties) work alone, and 182 ordinary tools work alone, but the
/// two together (6,665) do not, although no single tool is at fault.
const STRICT_PROPERTY_BUDGET: usize = 5_000;

/// Count every key of every `properties` map in a schema (the unit the
/// upstream's limit is expressed in). `enum`, `const` and `default` hold
/// values rather than schemas, so they are not walked.
fn count_schema_properties(schema: &Value) -> usize {
    match schema {
        Value::Object(map) => map
            .iter()
            .map(|(key, child)| match key.as_str() {
                "enum" | "const" | "default" => 0,
                "properties" => child.as_object().map_or(0, |props| {
                    props.len() + props.values().map(count_schema_properties).sum::<usize>()
                }),
                _ => count_schema_properties(child),
            })
            .sum(),
        Value::Array(items) => items.iter().map(count_schema_properties).sum(),
        _ => 0,
    }
}

/// Keep the strict tool schemas of one request within `budget` properties by
/// sending the heaviest tools non-strict (`strict: false`).
///
/// A non-strict tool is not compiled by the upstream, so it stops counting
/// against the budget, but it is not abandoned: the model still sees the
/// full schema, and the proxy validates its tool calls against the client's
/// original schema (`crate::repair`) exactly as it does for strict tools.
/// What is lost is only the upstream's *guarantee* that the arguments match,
/// and only for the fewest tools that bring the request back under budget.
/// Demoting the heaviest first minimizes how many tools lose it. Tools that
/// are already non-strict, and server tools, are left alone.
fn enforce_strict_property_budget(tools: &mut [ToolDefinition], budget: usize) {
    let mut weights: Vec<(usize, usize)> = tools
        .iter()
        .enumerate()
        .filter_map(|(index, tool)| match tool {
            ToolDefinition::Function(function) if function.strict => {
                Some((index, count_schema_properties(&function.parameters)))
            }
            _ => None,
        })
        .collect();
    let mut total: usize = weights.iter().map(|(_, weight)| weight).sum();
    if total <= budget {
        return;
    }

    // Heaviest first; ties keep request order so the choice is stable.
    weights.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let before = total;
    let mut demoted: Vec<String> = Vec::new();
    for (index, weight) in weights {
        if total <= budget {
            break;
        }
        if let ToolDefinition::Function(function) = &mut tools[index] {
            function.strict = false;
            total -= weight;
            demoted.push(format!("{} ({weight})", function.name));
        }
    }
    tracing::warn!(
        before,
        after = total,
        budget,
        demoted = ?demoted,
        "strict tool schemas exceed the upstream's compile budget; sending the heaviest tools \
         non-strict so the request is not answered with an empty `incomplete` response. See \
         STRICT_PROPERTY_BUDGET in src/translate.rs."
    );
}

/// Make a tool's `parameters` schema acceptable to the strict Responses
/// API validator. The strict validator is fully recursive: it descends
/// into every `properties.*`, `items`, `additionalProperties` value and
/// `anyOf` branch, and applies the same
/// `strict: true` rules at every depth. Earlier versions of this
/// function only ran the reconciliations at the top level of
/// `parameters` — that was wrong, and the validator caught us when
/// Claude Code's `AskUserQuestion` tool emitted
/// `properties.annotations.additionalProperties` (itself an object
/// schema) without a `required` array, yielding
/// `Missing 'notes'` at the `additionalProperties` level.
///
/// The function is now structured as a single recursive walk that
/// applies a set of reconciliations at every object schema it visits:
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
///    Combined with rule 4 this means every property is `required` on
///    the wire — the strict validator has no notion of optionality.
///    Optionality is instead *encoded*, not dropped: properties the
///    original schema does not list as required are made nullable
///    (`type: ["X", "null"]`, or an added `{"type": "null"}` `anyOf`
///    branch) so the model can signal "omitted" with `null` instead of
///    fabricating a value. `crate::repair` inverts this on the response
///    side: null sentinels on optional properties are stripped before
///    the input is shipped to the client, which validates against the
///    *original* schema and would reject a fabricated value.
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
///    One exception, and it matters: a node that carries `anyOf` (or
///    `oneOf`, which rule 9 rewrites to `anyOf`) already has a
///    complete type expression, so `type` is dropped from it and rule
///    7 is skipped for it. A `type` that agrees with the branches is
///    merely redundant; a `type` DISJOINT from every branch is fatal,
///    because the upstream does not answer with a schema error - it
///    gives up, returning `status: "incomplete"`,
///    `incomplete_details.reason: "max_output_tokens"`, an empty
///    `output` and an all-zero `usage`. Claude Code renders that as
///    "response exceeded the ... output token maximum" (live: Claude
///    Code's `Artifact` tool declares `files` as
///    `type: ["string", "null"]` alongside an `anyOf` of an array
///    branch and an object branch - disjoint from `string`, so every
///    request carrying that tool died with 0 in / 0 out). `allOf`
///    deliberately does not count here: rule 10 removes it, so an
///    `allOf`-only node still needs its injected `type`.
/// 8. If a schema node declares (or includes) `type: "array"`, make
///    sure it carries an object-valued `items`. The strict validator
///    requires `items` on every array schema — `array schema missing
///    items` (live: Claude Code's `ArtifactData` tool declares
///    `properties.query.properties.where.items` as a draft-2020-12
///    fixed-length tuple, i.e. `type: "array"` with `prefixItems` and
///    no `items`; that is legal JSON Schema but not legal strict).
///    `prefixItems` is deliberately preserved — the gateway accepts it
///    alongside `items` (verified against the live upstream), so the
///    client's positional constraints survive and `items` governs only
///    the elements beyond the tuple, exactly as the client intended.
///    When there is no usable element schema (`items` absent, or a
///    non-object `items` such as the draft-04 `items: [...]` list or a
///    boolean), a permissive schema is synthesised, because the strict
///    validator rejects every "empty" alternative: `items: {}` gives
///    `schema must have a 'type' key`, and `items: true` gives `array
///    schema items is not an object`.
/// 9. Rewrite `oneOf` as `anyOf`. The strict validator refuses `oneOf`
///    outright (`'oneOf' is not permitted`) while `anyOf` is permitted,
///    and the only semantic difference is cardinality (exactly one vs
///    at least one) - "at least one" is the safe relaxation, and it
///    keeps the branch constraints that dropping the keyword outright
///    would lose. When a node carries both, the `oneOf` branches are
///    appended to the existing `anyOf`.
/// 10. Drop the keywords the strict validator refuses outright (see
///     [`UNSUPPORTED_KEYWORDS`] for which were confirmed live and which
///     follow OpenAI's documented subset). `allOf`, `if`, `contains`,
///     `unevaluatedItems`, `unevaluatedProperties` and `minProperties`
///     all give `'<keyword>' is not permitted`, and `not` gives
///     `Unsupported keywords ('not',)`. Every one of them is *restrictive*, so
///     dropping them relaxes the schema - it can never start rejecting a
///     value the client's own schema accepted. That is the same
///     trade-off already made for `format` and `propertyNames`. The
///     branches are deliberately NOT merged into the parent: a
///     conjunction has no strict-mode equivalent, and merging
///     conflicting branches (two different `pattern`s, say) would
///     fabricate a constraint the client never asked for.
/// 11. Drop a `pattern` whose regex the strict validator cannot
///     compile. Two classes are rejected: lookaround (`(?=`, `(?!`,
///     `(?<=`, `(?<!`) gives `regex lookaround is not supported`, and a
///     Unicode property escape (`\p{...}`, `\P{...}`) gives
///     `'<pattern>' is not a 'regex'` (live: Claude Code's `ArtifactData`
///     declares its path patterns with negative lookahead).
///     Backreferences, possessive quantifiers and atomic groups ARE
///     accepted, so this is a targeted predicate rather than a
///     `regex::Regex::new` compile test: the Rust engine also rejects
///     constructs the upstream takes (which would drop working
///     constraints) and accepts `\p{...}`, which the upstream rejects
///     (which would miss a real failure). As with `format`, the model is
///     the *producer* of these strings and the property's `description`
///     still guides the shape. If a future upstream widens its dialect,
///     narrow `pattern_is_unsupported` rather than removing the strip.
/// 12. Drop `examples`. It is annotation-only, so dropping it cannot change
///     what the model may send; the live gateway has not been shown to
///     accept it, and MCP generators emit it on every tool.
///
/// `$ref`s are NOT handled here. They are inlined before this function
/// runs (`crate::schema_refs::resolve_local_refs`), so the walk sees the
/// real definition at every use site. The one exception is a *recursive*
/// definition, which stays behind as a bare `{"$ref": ...}` into a root
/// `$defs` entry; the walk treats such a node as opaque and reconciles the
/// retained `$defs` entries like any other schema.
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
fn sanitize_tool_schema(mut schema: Value, original: Option<&Value>) -> Value {
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

    // Flatten `allOf` object intersections into their parent object
    // schema. Azure OpenAI and the Responses strict validator both reject
    // `allOf` anywhere in tool parameter schemas even though the JSON
    // Schema spec permits it. Merging an *object intersection*'s branches
    // into the parent preserves semantics (merged properties + unioned
    // required) while eliminating the forbidden keyword. A *scalar
    // refinement* (e.g. `{"type":"string","allOf":[{"pattern":…}]}`)
    // has nothing to merge and is left for `reconcile_strict`'s rule 10 to
    // drop, so the node keeps its own `type` instead of being coerced to an
    // object.
    flatten_allof(&mut schema);

    reconcile_strict(&mut schema, original);
    schema
}

/// True when an `allOf` node is an **object intersection** that may be
/// flattened into its parent, rather than a **scalar refinement** (e.g. a
/// string node whose branches carry only `pattern`) that must keep its own
/// `type`.
///
/// The node qualifies when it - or any branch - is object-like: it declares
/// `type: "object"`, or carries any object-only keyword (`properties`,
/// `required`, `patternProperties`, `additionalProperties`). A branch such
/// as `{"pattern": …}` or a bare `{"type": "string"}` is not object-like,
/// so its `allOf` is left in place for rule 10 to drop.
fn all_of_is_object_intersection(map: &serde_json::Map<String, Value>, branches: &[Value]) -> bool {
    // A node that declares an explicit non-object type is a scalar (or
    // array) refinement; never merge an object's `properties` into it.
    if matches!(map.get("type"), Some(Value::String(s)) if s != "object") {
        return false;
    }
    fn object_like(v: &Value) -> bool {
        let Value::Object(m) = v else {
            return false;
        };
        matches!(m.get("type"), Some(Value::String(s)) if s == "object")
            || m.contains_key("properties")
            || m.contains_key("required")
            || m.contains_key("patternProperties")
            || m.contains_key("additionalProperties")
    }
    matches!(map.get("type"), Some(Value::String(s)) if s == "object")
        || map.contains_key("properties")
        || map.contains_key("required")
        || map.contains_key("patternProperties")
        || map.contains_key("additionalProperties")
        || branches.iter().any(object_like)
}

/// Recursively flatten an `allOf` **object intersection** into its parent
/// object schema, and leave a **scalar refinement**'s `allOf` in place for
/// `reconcile_strict`'s rule 10 to drop.
///
/// `allOf` denotes an intersection of constraints. When the subject node or
/// one of its branches is an object schema, the intersection is
/// semantically equivalent to merging every branch's `properties` and
/// unioning their `required` arrays onto the parent, which eliminates the
/// forbidden keyword without changing what the model may send.
///
/// It is NOT always an object intersection. Claude Code's
/// `SendMessage.properties.to` - the schema that produced the live
/// `'allOf' is not permitted` 400 - is a *scalar refinement*:
/// `{"type":"string","allOf":[{"pattern":…},{"pattern":…}]}`, the shape
/// Zod's `toJSONSchema` emits for a string carrying more than one
/// `.regex()`. There are no `properties` to merge, so flattening it would
/// only inject a bogus empty `properties: {}` and (via `is_object_schema`)
/// coerce the string node into an object. Those nodes are left untouched
/// here; rule 10 drops the `allOf` and the node keeps its own `type`.
///
/// Runs before `reconcile_strict`, which reconciles whatever remains.
fn flatten_allof(v: &mut Value) {
    match v {
        Value::Object(map) => {
            // Only an object intersection is flattened. A scalar refinement
            // is left untouched so rule 10 drops the keyword without
            // disturbing the node's own `type`.
            let is_intersection = map
                .get("allOf")
                .and_then(Value::as_array)
                .is_some_and(|branches| all_of_is_object_intersection(map, branches));
            if is_intersection && let Some(Value::Array(branches)) = map.remove("allOf") {
                // Merge properties from each branch.
                let mut merged_props: serde_json::Map<String, Value> = map
                    .get_mut("properties")
                    .and_then(|p| p.as_object_mut())
                    .cloned()
                    .unwrap_or_default();
                for branch in &branches {
                    if let Value::Object(bobj) = branch
                        && let Some(Value::Object(bprops)) = bobj.get("properties")
                    {
                        for (key, val) in bprops {
                            // Parent wins on conflict;\ otherwise keep the first seen.
                            merged_props.entry(key.clone()).or_insert(val.clone());
                        }
                    }
                }
                // This branch is only reached for an object intersection
                // (guarded above). Insert `properties` even when empty so
                // the later `reconcile_strict` pass treats this node as a
                // typed object rather than defaulting it to `string`.
                map.insert("properties".to_owned(), Value::Object(merged_props));
                // Collect required from all branches.
                let mut merged_required: Vec<String> = vec![];
                for branch in &branches {
                    if let Value::Object(bobj) = branch
                        && let Some(Value::Array(req)) = bobj.get("required")
                    {
                        for r in req {
                            if let Some(s) = r.as_str()
                                && !merged_required.iter().any(|x| x == s)
                            {
                                merged_required.push(s.to_owned());
                            }
                        }
                    }
                }
                if !merged_required.is_empty() {
                    let existing_req = map.get_mut("required").and_then(|r| r.as_array_mut());
                    if let Some(arr) = existing_req {
                        // Append missing keys while preserving order.
                        for r in &merged_required {
                            if !arr.iter().any(|v| v.as_str() == Some(r.as_str())) {
                                arr.push(Value::String(r.clone()));
                            }
                        }
                    } else {
                        let arr: Vec<Value> =
                            merged_required.into_iter().map(Value::String).collect();
                        map.insert("required".to_owned(), Value::Array(arr));
                    }
                }
                // Propagate simple keywords like `description` from branches
                // if the parent doesn't already have them.
                for branch in &branches {
                    if let Value::Object(bobj) = branch
                        && let Some(Value::String(desc)) = bobj.get("description")
                        && !map.contains_key("description")
                    {
                        map.insert("description".to_owned(), Value::String(desc.clone()));
                    }
                }
            }
            // Recurse into all child schemas.
            for child in map.values_mut() {
                flatten_allof(child);
            }
        }
        Value::Array(arr) => {
            for child in arr.iter_mut() {
                flatten_allof(child);
            }
        }
        _ => {}
    }
}

/// Recursively reconcile a JSON Schema value against the strict
/// Responses API rules. Post-order: children are fully reconciled
/// before the parent is mutated, so the parent's `required`
/// reconciliation observes the (already-reconciled) `properties` map.
///
/// `v` is expected to have been pre-cleaned of `propertyNames` by
/// [`strip_property_names`] before this is called.
fn reconcile_strict(v: &mut Value, original: Option<&Value>) {
    reconcile_strict_inner(v, false, original);
}

/// Internal worker for [`reconcile_strict`]. `force_object` is true when
/// the caller is recursing into an `anyOf` branch (the only combinator
/// that can reach the wire - rule 9 rewrites `oneOf` into it and rule 10
/// drops `allOf`). The OpenAI Responses strict validator treats
/// every such branch as an object schema, so a typeless or
/// `properties`-less branch must be coerced into an object schema with
/// `additionalProperties: false`. Without this, a branch like
/// `{ "description": "An ADF document..." }` would fall through to
/// `type: "string"` and the validator rejects the request with
/// `additionalProperties is required to be supplied and to be false`
/// pointing at that branch.
/// `original` is the matching node of the *untouched* client schema,
/// threaded down the walk so optionality decisions (`make_nullable`)
/// consult the real `required` arrays instead of the mutated ones.
fn reconcile_strict_inner(v: &mut Value, force_object: bool, original: Option<&Value>) {
    // A bare `{"$ref": ...}` is a retained recursive reference (every other
    // `$ref` was inlined by `schema_refs::resolve_local_refs` before this
    // walk). It is opaque: giving it a `type`, `properties` or
    // `additionalProperties` would contradict the definition it points at
    // and add the sibling keywords the validator refuses.
    if let Value::Object(map) = v
        && map.contains_key("$ref")
    {
        return;
    }

    if force_object && let Value::Object(map) = v {
        // Coerce a typeless or otherwise un-typed combinator branch
        // into an object schema. We don't strip existing keys (the
        // client may carry semantic info like `description`,
        // `markdownDescription`, `enum`, etc.) - we just ensure the
        // object-keyword requirements are satisfied.
        if !map.contains_key("additionalProperties") {
            map.insert("additionalProperties".to_owned(), Value::Bool(false));
        }
        if !map.contains_key("properties") {
            map.insert("properties".to_owned(), Value::Object(Default::default()));
        }
        if !map.contains_key("type") {
            map.insert("type".to_owned(), Value::String("object".into()));
        }
    }

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
            // OpenAI's strict validator rejects schemas that say
            // `additionalProperties: true` for object-like schemas; it
            // wants the literal `false`. Coerce it.
            if let Some(Value::Bool(b)) = map.get("additionalProperties")
                && *b
                && is_object_schema(map)
            {
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
            // Rules 7a + 7: `type` and combinators.
            //
            // `anyOf` is itself a complete type expression, and `oneOf`
            // becomes one under rule 9, so a node carrying either must not
            // also carry a `type`:
            //
            // - A `type` DISJOINT from every branch breaks the upstream
            //   compiler outright. It does not return a schema error, it
            //   gives up: `status: "incomplete"`,
            //   `incomplete_details.reason: "max_output_tokens"`, an empty
            //   `output` and an all-zero `usage`. The client renders that
            //   as "response exceeded the ... output token maximum" (live:
            //   Claude Code's `Artifact` tool sends `files` as
            //   `type: ["string", "null"]` alongside an `anyOf` whose
            //   branches are an array and an object - disjoint from
            //   `string`, so every request carrying that tool died).
            // - A `type` that AGREES with the branches is merely redundant.
            //
            // So a client-sent `type` is dropped here and the injection
            // below is skipped. `allOf` deliberately does NOT count: rule
            // 10 removes it, so a node whose only combinator is `allOf`
            // still needs a `type` and would become invalid without one.
            let combinator_expresses_type = ["anyOf", "oneOf"]
                .iter()
                .any(|keyword| map.contains_key(*keyword));
            if combinator_expresses_type && let Some(dropped) = map.remove("type") {
                tracing::warn!(
                    type_keyword = ?dropped,
                    "dropping `type` from a schema node that carries `anyOf`/`oneOf`; the \
                     combinator already expresses the type, and a `type` disjoint from every \
                     branch makes the upstream answer with an empty `incomplete` response \
                     instead of a schema error. See sanitize_tool_schema doc comment in \
                     src/translate.rs."
                );
            }
            if !map.contains_key("type") && !combinator_expresses_type {
                if is_object_schema(map) {
                    tracing::warn!(
                        "schema node missing `type` key but has `properties`; defaulting to `object`. The \
                         Responses strict validator rejects type-less object schemas. See \
                         sanitize_tool_schema doc comment in src/translate.rs."
                    );
                    map.insert("type".to_owned(), Value::String("object".into()));
                } else {
                    tracing::warn!(
                        "schema node missing `type` key; defaulting to `string`. The \
                         Responses strict validator rejects type-less schemas. See \
                         sanitize_tool_schema doc comment in src/translate.rs."
                    );
                    map.insert("type".to_owned(), Value::String("string".into()));
                }
            }

            // Rule 8: `items` is mandatory on array schemas. Runs
            // after the type-injection rule so every node has a
            // resolvable `type` by this point, and before the recursion
            // so a synthesised `items` is walked (and repaired, if
            // needed) like any other child schema.
            if is_array_schema(map) && !matches!(map.get("items"), Some(Value::Object(_))) {
                match map.get("items") {
                    Some(existing) => tracing::warn!(
                        items = ?existing,
                        "replacing non-object `items` on an array schema; the Responses strict \
                         validator requires `items` to be an object schema. See \
                         sanitize_tool_schema doc comment in src/translate.rs."
                    ),
                    None => tracing::warn!(
                        "adding a permissive `items` to an array schema that declares none; the \
                         Responses strict validator rejects any `type: \"array\"` node without \
                         `items`. See sanitize_tool_schema doc comment in src/translate.rs."
                    ),
                }
                map.insert("items".to_owned(), permissive_array_items());
            }

            // Rule 9: `oneOf` is not permitted upstream, but `anyOf` is.
            if let Some(one_of) = map.remove("oneOf") {
                let branches = match one_of {
                    Value::Array(branches) => branches,
                    other => vec![other],
                };
                let branch_count = branches.len();
                match map.get_mut("anyOf") {
                    Some(Value::Array(existing)) => existing.extend(branches),
                    _ => {
                        map.insert("anyOf".to_owned(), Value::Array(branches));
                    }
                }
                tracing::warn!(
                    branches = branch_count,
                    "rewriting `oneOf` as `anyOf` in tool schema; the Responses strict validator \
                     refuses `oneOf` ('oneOf' is not permitted) but permits `anyOf`. See \
                     sanitize_tool_schema doc comment in src/translate.rs."
                );
            }

            // Rule 10: keywords the strict validator refuses outright.
            // Dropped BEFORE the walks below so neither the combinator
            // walk nor the recurse walk descends into a branch that is
            // about to disappear (which is what used to turn an `allOf`
            // branch of `pattern`s into a bogus `type: "object"`).
            for keyword in UNSUPPORTED_KEYWORDS {
                if map.remove(*keyword).is_some() {
                    tracing::warn!(
                        keyword = *keyword,
                        "dropping a keyword the Responses strict validator refuses from a tool \
                         schema; the keyword is restrictive, so dropping it only relaxes the \
                         schema. See sanitize_tool_schema doc comment in src/translate.rs."
                    );
                }
            }

            // Rule 12: annotation-only keywords the strict validator has not
            // been shown to accept. `examples` is documentation with no
            // effect on validation, so dropping it cannot change what the
            // model may send. Debug level: MCP generators emit it on every
            // tool, so a warn would be noise.
            for keyword in ANNOTATION_ONLY_UNSUPPORTED_KEYWORDS {
                if map.remove(*keyword).is_some() {
                    tracing::debug!(
                        keyword = *keyword,
                        "dropping an annotation-only keyword from a tool schema; it has no \
                         effect on validation. See sanitize_tool_schema doc comment in \
                         src/translate.rs."
                    );
                }
            }

            // Rule 11: `pattern` regexes the strict validator cannot compile.
            if map
                .get("pattern")
                .and_then(Value::as_str)
                .is_some_and(pattern_is_unsupported)
                && let Some(Value::String(pattern)) = map.remove("pattern")
            {
                tracing::warn!(
                    pattern = %pattern,
                    "dropping an unsupported `pattern` from a tool schema; the Responses strict \
                     validator rejects regex lookaround and Unicode property escapes. See \
                     sanitize_tool_schema doc comment in src/translate.rs."
                );
            }

            // Now recurse into child schemas. We do this AFTER the
            // self-referential rules above so the empty-additional
            // strip and the false-add have already settled the
            // parent's `additionalProperties` form (we don't recurse
            // into a freshly-added `false`). The `required`
            // reconciliation runs last, on the post-recurse
            // properties, so the parent's required array is built
            // from the (now-typed) child keys.
            //
            // `force_object` is set for combinator-array elements so
            // OpenAI's strict validator gets `additionalProperties:
            // false` even on branches that lack `properties`.
            for keyword in COMBINATOR_KEYWORDS {
                if let Some(Value::Array(arr)) = map.get_mut(*keyword) {
                    let orig_branches = original
                        .and_then(|o| o.get(*keyword))
                        .and_then(Value::as_array);
                    for (i, child) in arr.iter_mut().enumerate() {
                        let child_original = orig_branches.and_then(|b| b.get(i));
                        reconcile_strict_inner(child, true, child_original);
                    }
                }
            }
            for keyword in RECURSE_KEYWORDS {
                if let Some(child) = map.get_mut(*keyword) {
                    // `additionalProperties` may carry a sub-schema;
                    // recurse into it but do NOT force-object it -
                    // that field really can be a string/array schema.
                    let force = false;
                    let child_original = original.and_then(|o| o.get(*keyword));
                    reconcile_strict_inner(child, force, child_original);
                }
            }
            // Retained recursive definitions live in `$defs` / `definitions`
            // (see `schema_refs`); each is a schema the validator checks
            // like any other, so reconcile them too.
            for container in ["$defs", "definitions"] {
                if let Some(Value::Object(defs)) = map.get_mut(container) {
                    let orig_defs = original
                        .and_then(|o| o.get(container))
                        .and_then(Value::as_object);
                    for (name, child) in defs.iter_mut() {
                        let child_original = orig_defs.and_then(|d| d.get(name));
                        reconcile_strict_inner(child, false, child_original);
                    }
                }
            }
            if let Some(Value::Object(props)) = map.get_mut("properties") {
                let orig_props = original
                    .and_then(|o| o.get("properties"))
                    .and_then(Value::as_object);
                for (key, child) in props.iter_mut() {
                    let child_original = orig_props.and_then(|p| p.get(key));
                    reconcile_strict_inner(child, false, child_original);
                    // Nullable encoding: properties the *original*
                    // schema does not list as required are still added
                    // to `required` below (strict mode demands it), but
                    // we mark them nullable so the model can signal
                    // "omitted" with `null` instead of fabricating a
                    // value. `crate::repair` strips the null sentinels
                    // on the response side. Runs AFTER the recursion so
                    // the child is fully reconciled (typed) first.
                    if !original_lists_required(original, key) {
                        make_nullable(child);
                    }
                }
            }
            reconcile_required(map);
        }
        Value::Array(arr) => {
            // Top-level array reached via the bare `reconcile_strict`
            // wrapper (e.g. `anyOf` at the schema root). Force-object
            // each element so the strict validator sees
            // `additionalProperties: false` on every branch.
            for (i, child) in arr.iter_mut().enumerate() {
                let child_original = original.and_then(|o| o.as_array()).and_then(|a| a.get(i));
                reconcile_strict_inner(child, true, child_original);
            }
        }
        _ => {}
    }
}

/// True if the *original* (pre-sanitization) schema node lists `key`
/// in its `required` array. When no original node is available (e.g.
/// direct sanitizer tests), every property is treated as required —
/// the legacy behaviour, which keeps the existing test-suite
/// semantics for `sanitize_tool_schema(schema, None)`.
fn original_lists_required(original: Option<&Value>, key: &str) -> bool {
    let Some(node) = original else {
        return true;
    };
    node.get("required")
        .and_then(Value::as_array)
        .is_some_and(|req| req.iter().any(|r| r.as_str() == Some(key)))
}

/// Rewrite a property sub-schema so it also accepts JSON `null`, using
/// the union forms the OpenAI strict validator compiles:
/// `"type": ["X", "null"]` for single-typed nodes, and an added
/// `{"type": "null"}` `anyOf` branch for everything else. Already-
/// nullable nodes are left untouched.
fn make_nullable(v: &mut Value) {
    let Value::Object(map) = v else {
        return;
    };
    // Already nullable in some form?
    if let Some(Value::Array(ts)) = map.get("type")
        && ts.iter().any(|t| t.as_str() == Some("null"))
    {
        return;
    }
    if let Some(arr) = map.get("anyOf").and_then(Value::as_array) {
        let has_null = arr.iter().any(|b| {
            b.as_object()
                .and_then(|o| o.get("type"))
                .is_some_and(|t| t.as_str() == Some("null"))
        });
        if has_null {
            return;
        }
    }
    match map.get("type") {
        Some(Value::String(t)) => {
            // `"type": "X"` → `["X", "null"]`.
            map.insert(
                "type".to_owned(),
                Value::Array(vec![Value::String(t.clone()), Value::String("null".into())]),
            );
        }
        Some(Value::Array(ts)) => {
            let mut ts = ts.clone();
            ts.push(Value::String("null".into()));
            map.insert("type".to_owned(), Value::Array(ts));
        }
        _ => {
            // Typeless / combinator node: add a null branch via anyOf.
            // The strict validator accepts anyOf only for nullable
            // unions, which is exactly what this is.
            let mut branches = match map.remove("anyOf") {
                Some(Value::Array(a)) => a,
                other => {
                    // Move the node's own constraints into a branch so
                    // semantics are preserved.
                    let mut branch = serde_json::Map::new();
                    for (k, val) in map.iter() {
                        if k != "description" {
                            branch.insert(k.clone(), val.clone());
                        }
                    }
                    let mut arr = if branch.is_empty() {
                        Vec::new()
                    } else {
                        vec![Value::Object(branch)]
                    };
                    if let Some(Value::Array(extra)) = other {
                        arr.extend(extra);
                    }
                    arr
                }
            };
            branches.push(serde_json::json!({ "type": "null" }));
            // The node now expresses its type via anyOf; drop the
            // duplicated keywords (description stays as a hint).
            for k in [
                "$ref",
                "type",
                "properties",
                "required",
                "additionalProperties",
                "items",
                "oneOf",
                "allOf",
                "enum",
                "pattern",
            ] {
                map.remove(k);
            }
            map.insert("anyOf".to_owned(), Value::Array(branches));
        }
    }
}

/// Combinators whose branches the OpenAI Responses strict validator
/// treats as object schemas, requiring `additionalProperties: false`.
/// These are walked separately so each branch is force-typed as an
/// object before the ordinary [`RECURSE_KEYWORDS`] walk runs.
///
/// `anyOf` is the only combinator that can reach this walk: rule 9
/// rewrites `oneOf` into it, and rule 10 drops `allOf` entirely.
const COMBINATOR_KEYWORDS: &[&str] = &["anyOf"];

/// JSON Schema keywords whose value is (or contains) a sub-schema
/// that the strict Responses validator will recursively validate.
/// Listed in the order we recurse; the order doesn't matter for
/// correctness but a stable order makes the diff on `cargo fmt`
/// reproducible.
///
/// `oneOf`, `allOf`, `not`, `if`, `then`, `else`, `contains`,
/// `unevaluatedProperties` and `unevaluatedItems` are absent by
/// design: the strict validator refuses them, so rule 10 (or rule 9)
/// removes them from every node before this walk runs.
const RECURSE_KEYWORDS: &[&str] = &[
    "additionalProperties", // the schema form, not the bool form
    "items",
    "anyOf",
    "propertyNames", // pre-stripped, but a no-op if absent
    "prefixItems",
    "additionalItems",
];

/// Keywords the strict validator refuses outright, so they must never
/// reach the wire (rule 10). Confirmed against the live upstream:
/// `allOf`, `if`, `contains`, `unevaluatedItems`, `unevaluatedProperties`
/// and `minProperties` all return `'<keyword>' is not permitted`, and
/// `not` returns `Unsupported keywords ('not',)`. `maxProperties` and
/// `uniqueItems` have not been seen refused live: they are dropped on the
/// strength of OpenAI's documented subset (object and array keywords are
/// limited to `minItems` / `maxItems`) and as `minProperties`' sibling.
/// All of them are restrictive, so dropping them can only relax the schema.
const UNSUPPORTED_KEYWORDS: &[&str] = &[
    "allOf",
    "not",
    "if",
    "then",
    "else",
    "contains",
    "unevaluatedItems",
    "unevaluatedProperties",
    "minProperties",
    "maxProperties",
    "uniqueItems",
];

/// Annotation-only keywords dropped from every schema node (rule 12).
/// Unlike [`UNSUPPORTED_KEYWORDS`] these were not confirmed refused by the
/// live gateway; they are dropped because they carry no validation meaning,
/// so the drop is free.
const ANNOTATION_ONLY_UNSUPPORTED_KEYWORDS: &[&str] = &["examples"];

/// True if a JSON Schema `pattern` uses a regex construct the upstream
/// strict validator cannot compile (rule 11).
///
/// Two classes are confirmed against the live upstream: lookaround
/// groups (`(?=`, `(?!`, `(?<=`, `(?<!`) are rejected with `regex
/// lookaround is not supported`, and Unicode property escapes
/// (`\p{...}`, `\P{...}`) with `'<pattern>' is not a 'regex'`.
///
/// This is deliberately NOT a `regex::Regex::new` compile test. That
/// engine also rejects backreferences, possessive quantifiers and
/// atomic groups - all of which the upstream accepts, so the test
/// would drop working constraints - and it accepts `\p{...}`, which
/// the upstream rejects, so it would miss a real failure.
fn pattern_is_unsupported(pattern: &str) -> bool {
    const LOOKAROUND: [&str; 4] = ["(?=", "(?!", "(?<=", "(?<!"];
    LOOKAROUND.iter().any(|group| pattern.contains(group))
        || pattern.contains("\\p{")
        || pattern.contains("\\P{")
}

/// True if this object schema declares itself to be a JSON `object`.
/// Used to decide whether to inject `additionalProperties: false`
/// and `properties: {}` (those rules only apply to object schemas;
/// injecting them into a `string` schema would be a wire-level bug).
///
/// A schema is treated as an object if either it carries the
/// conventional `"type": "object"` tag, **or** it carries a
/// `properties` map without an explicit type. The latter form is
/// valid JSON Schema but the OpenAI Responses API strict validator
/// rejects any nested object that does not declare either of those
/// or `additionalProperties`, so we must reconcile it here.
fn is_object_schema(map: &serde_json::Map<String, Value>) -> bool {
    matches!(map.get("type"), Some(Value::String(s)) if s == "object")
        || (map.get("type").is_none() && map.contains_key("properties"))
}

/// True if this schema node declares the JSON `array` type, either
/// directly (`"type": "array"`) or as one member of a type union
/// (`"type": ["array", "null"]`, the nullable form rule 5's
/// `make_nullable` produces). The strict validator requires an
/// object-valued `items` keyword on every such node (rule 8).
fn is_array_schema(map: &serde_json::Map<String, Value>) -> bool {
    match map.get("type") {
        Some(Value::String(s)) => s == "array",
        Some(Value::Array(types)) => types.iter().any(|t| t.as_str() == Some("array")),
        _ => false,
    }
}

/// The `items` schema rule 8 injects when the client supplies none.
///
/// It cannot be empty: the strict validator rejects `items: {}` with
/// `schema must have a 'type' key` and `items: true` with `array schema
/// items is not an object` (both confirmed against the live upstream).
/// The member types of the union bring their own obligations under
/// strict mode, so this value is built self-supporting at every level
/// rather than relying on the recursive walk to repair it: the `object`
/// member carries `additionalProperties: false` (the validator demands
/// it on any object member of a union) and each `array` member carries
/// its own `items`.
///
/// Two levels of nesting covers what a model realistically emits into
/// an unconstrained array slot; below that the element degrades to
/// `string`, the same fallback the type-injection rule already uses for
/// untyped nodes. Note the unavoidable limitation: OpenAI's strict mode
/// has no permissive `additionalProperties` form, so an object element
/// can only be the empty object. That is still strictly better than the
/// alternative, which is a guaranteed 400 for the whole request.
fn permissive_array_items() -> Value {
    json!({
        "type": ["string", "number", "boolean", "object", "array", "null"],
        "properties": {},
        "additionalProperties": false,
        "items": {
            "type": ["string", "number", "boolean", "object", "array", "null"],
            "properties": {},
            "additionalProperties": false,
            "items": {"type": "string"}
        }
    })
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
    replay: &ReasoningReplay,
    tool_result_images: &ToolResultImages,
    forward_documents: bool,
) -> Result<()> {
    match msg.role {
        MessageRole::User => append_user_message(
            out,
            &msg.content,
            prompt_caching,
            tool_result_images,
            forward_documents,
        ),
        MessageRole::Assistant => append_assistant_message(out, &msg.content, replay),
        MessageRole::System => append_system_message(instructions, &msg.content),
    }
}

fn append_user_message(
    out: &mut Vec<InputItem>,
    content: &MessageContent,
    prompt_caching: &PromptCachingConfig,
    tool_result_images: &ToolResultImages,
    forward_documents: bool,
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
            // Images lifted out of `tool_result` blocks (`lift` mode),
            // waiting for the run of consecutive tool results to end.
            let mut lifted: Vec<InputContentPart> = Vec::new();
            for block in blocks {
                match block {
                    ContentBlockParam::Text(t) => {
                        flush_lifted_images(out, &mut lifted);
                        let ephemeral =
                            !prompt_caching.models.is_empty() && is_ephemeral(&t.cache_control);
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
                        let translated = tool_result_output(
                            tr,
                            tool_result_images,
                            prompt_caching,
                            forward_documents,
                        );
                        out.push(InputItem::FunctionCallOutput {
                            call_id: tr.tool_use_id.clone(),
                            output: translated.output,
                        });
                        if !translated.lifted_images.is_empty() {
                            // The upstream cannot carry images inside
                            // `function_call_output`, so the images go in a
                            // follow-up user message. They are NOT emitted
                            // here: with several tool results in one turn
                            // that would wedge a user message between the
                            // `function_call_output` items, which strict
                            // gateways reject. They are held and emitted
                            // once the run of tool results ends, each
                            // group labelled with the call it belongs to.
                            lifted.push(InputContentPart::InputText {
                                text: format!("Image(s) returned by tool call {}:", tr.tool_use_id),
                                prompt_cache_breakpoint: None,
                            });
                            lifted.extend(translated.lifted_images);
                        }
                    }
                    ContentBlockParam::Image(img) => {
                        flush_lifted_images(out, &mut lifted);
                        if let Some(image_part) = convert_image_block(img) {
                            let ephemeral = !prompt_caching.models.is_empty()
                                && is_ephemeral(&img.cache_control);
                            parts.push((image_part, ephemeral));
                        }
                    }
                    ContentBlockParam::Document(doc) => {
                        flush_lifted_images(out, &mut lifted);
                        let converted = convert_document_block(doc, forward_documents);
                        let last = converted.len().saturating_sub(1);
                        let ephemeral =
                            !prompt_caching.models.is_empty() && is_ephemeral(&doc.cache_control);
                        for (i, part) in converted.into_iter().enumerate() {
                            parts.push((part, ephemeral && i == last));
                        }
                    }
                    ContentBlockParam::ToolUse(_) => {
                        return Err(anyhow!(
                            "user message contains a tool_use block; the assistant produced this in a prior turn"
                        ));
                    }
                    ContentBlockParam::Thinking(th) => {
                        // A `thinking` block belongs in an assistant turn.
                        // Folding the model's own reasoning into the *user's*
                        // voice would make the model attribute its prior
                        // chain-of-thought to the user, so drop it.
                        tracing::warn!(
                            chars = th.thinking.len(),
                            "dropping thinking block from user message \
                             (thinking blocks belong in assistant turns)"
                        );
                    }
                    ContentBlockParam::RedactedThinking(rt) => {
                        tracing::warn!(
                            bytes = rt.data.len(),
                            "dropping redacted_thinking block from user message"
                        );
                    }
                    ContentBlockParam::ServerToolUse(_)
                    | ContentBlockParam::AdvisorToolResult(_)
                    | ContentBlockParam::WebFetchToolResult(_)
                    | ContentBlockParam::WebSearchToolResult(_) => {
                        // Server tool echo blocks belong to an assistant
                        // turn. Folding them into the user's voice would
                        // attribute the prior consultation or the fetched
                        // page to the user, so drop them just as the old
                        // `Unknown` fallback did.
                        tracing::warn!(
                            "dropping server tool echo block from user message \
                             (server tool blocks belong in assistant turns)"
                        );
                    }
                    ContentBlockParam::Unknown => {
                        tracing::warn!("dropping unknown user content block type");
                    }
                }
            }
            flush_lifted_images(out, &mut lifted);
            flush_user_parts(out, &mut parts);
            Ok(())
        }
    }
}

/// Emit the images lifted out of tool results as one user message, then
/// clear the buffer. A no-op when nothing was lifted.
fn flush_lifted_images(out: &mut Vec<InputItem>, lifted: &mut Vec<InputContentPart>) {
    if !lifted.is_empty() {
        out.push(InputItem::Message {
            role: "user".into(),
            content: std::mem::take(lifted),
        });
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

/// Translate a `document` block into upstream content parts.
///
/// - `text` source → one `input_text` part (the file's text).
/// - `content` source → its text blocks as `input_text`, its image blocks as
///   `input_image`.
/// - `base64` / `url` source → an `input_file` part when `forward_files` is
///   on; otherwise a short note, so the model knows a file existed.
/// - Anything else (a Files API `file_id`, an unknown type) → the note, since
///   the proxy has no way to resolve it.
///
/// `title` and `context` lead the text of the document as a header.
fn convert_document_block(doc: &DocumentBlockParam, forward_files: bool) -> Vec<InputContentPart> {
    let text_part = |text: String| InputContentPart::InputText {
        text,
        prompt_cache_breakpoint: None,
    };
    let title = doc.title.trim();
    let source_type = doc.source.get("type").and_then(Value::as_str);

    let mut header = if title.is_empty() {
        "[attached document]".to_owned()
    } else {
        format!("[attached document: {title}]")
    };
    if !doc.context.trim().is_empty() {
        header.push('\n');
        header.push_str(doc.context.trim());
    }

    let note = || {
        let media = doc
            .source
            .get("media_type")
            .and_then(Value::as_str)
            .unwrap_or("");
        let label = match (title.is_empty(), media.is_empty()) {
            (false, false) => format!("{title} ({media})"),
            (false, true) => title.to_owned(),
            (true, false) => media.to_owned(),
            (true, true) => "untitled".to_owned(),
        };
        tracing::warn!(
            document = label,
            "document block not forwarded upstream (forward_documents is off or the source \
             cannot be resolved)"
        );
        vec![text_part(format!("[attached document omitted: {label}]"))]
    };

    match source_type {
        Some("text") => {
            let Some(data) = doc.source.get("data").and_then(Value::as_str) else {
                return note();
            };
            vec![text_part(format!("{header}\n{data}"))]
        }
        Some("content") => {
            let mut parts = vec![text_part(header)];
            match doc.source.get("content") {
                Some(Value::String(s)) => parts.push(text_part(s.clone())),
                Some(Value::Array(blocks)) => {
                    for block in blocks {
                        match block.get("type").and_then(Value::as_str) {
                            Some("text") => {
                                if let Some(t) = block.get("text").and_then(Value::as_str) {
                                    parts.push(text_part(t.to_owned()));
                                }
                            }
                            Some("image") => {
                                if let Some(part) = ImageBlockParam::deserialize(block)
                                    .ok()
                                    .and_then(|img| convert_image_block(&img))
                                {
                                    parts.push(part);
                                }
                            }
                            _ => {}
                        }
                    }
                }
                _ => return note(),
            }
            parts
        }
        Some("base64") if forward_files => {
            let media = doc.source.get("media_type").and_then(Value::as_str);
            let data = doc.source.get("data").and_then(Value::as_str);
            let (Some(media), Some(data)) = (media, data) else {
                return note();
            };
            vec![InputContentPart::InputFile {
                filename: Some(if title.is_empty() {
                    "document.pdf".to_owned()
                } else {
                    title.to_owned()
                }),
                file_data: Some(format!("data:{media};base64,{data}")),
                file_url: None,
            }]
        }
        Some("url") if forward_files => {
            let Some(url) = doc.source.get("url").and_then(Value::as_str) else {
                return note();
            };
            vec![InputContentPart::InputFile {
                filename: None,
                file_data: None,
                file_url: Some(url.to_owned()),
            }]
        }
        _ => note(),
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

/// What the translation of one `tool_result` block produced.
struct TranslatedToolResult {
    /// The value for `FunctionCallOutput.output`. Text-only results keep
    /// the plain-string form (byte-identical to the pre-multimode
    /// behaviour); results carrying images use the array form.
    output: FunctionCallOutputBody,
    /// Images lifted out of the result for [`ToolResultImages::Lift`]:
    /// the caller emits these as a follow-up `user` message after the
    /// `FunctionCallOutput`. Empty in the other modes.
    lifted_images: Vec<InputContentPart>,
}

/// Translate a `tool_result` block into its wire form. When
/// `is_error: true` the text is prefixed with `[TOOL_ERROR]` so the model
/// can distinguish a tool failure from a successful result (the Responses
/// API has no first-class error field).
///
/// Images inside the tool result (`image` blocks) are carried per
/// `tool_result_images`:
/// - [`ToolResultImages::Array`] (default): `output` becomes an array of
///   `input_text` / `input_image` parts, the shape the Responses API
///   defines for tool outputs.
/// - [`ToolResultImages::Lift`]: text stays in `output`; the images are
///   returned in `lifted_images` so the caller can append a user message
///   carrying them (for upstreams that reject array-form tool outputs).
/// - [`ToolResultImages::Drop`]: images are dropped with a warning (the
///   historical behaviour).
fn tool_result_output(
    tr: &ToolResultBlockParam,
    mode: &ToolResultImages,
    prompt_caching: &PromptCachingConfig,
    forward_documents: bool,
) -> TranslatedToolResult {
    let mut ordered: Vec<InputContentPart> = Vec::new();
    let mut text_parts: Vec<String> = Vec::new();
    let mut images: Vec<InputContentPart> = Vec::new();
    let mut image_count = 0usize;

    let mut push_text = |text: String, ordered: &mut Vec<InputContentPart>| {
        text_parts.push(text.clone());
        // An empty `input_text` part carries nothing and some upstreams
        // reject it; the joined text above still sees it, so the text-only
        // form is unchanged.
        if !text.is_empty() {
            ordered.push(InputContentPart::InputText {
                text,
                prompt_cache_breakpoint: None,
            });
        }
    };

    match &tr.content {
        Some(ToolResultContent::Text(s)) => {
            push_text(s.clone(), &mut ordered);
        }
        Some(ToolResultContent::Blocks(blocks)) => {
            for block in blocks {
                let is_image = block.get("type").and_then(Value::as_str) == Some("image");
                if is_image {
                    // `&Value` implements `Deserializer`, so the block is
                    // decoded in place without cloning the base64 payload.
                    match ImageBlockParam::deserialize(block)
                        .ok()
                        .and_then(|img| convert_image_block(&img).map(|p| (img, p)))
                    {
                        Some((img, mut part)) => {
                            // Honour an ephemeral cache breakpoint on the
                            // image block, exactly as the user-message
                            // path does, but only when the operator has
                            // allowlisted the model for caching.
                            if !prompt_caching.models.is_empty()
                                && let InputContentPart::InputImage {
                                    prompt_cache_breakpoint,
                                    ..
                                } = &mut part
                                && is_ephemeral(&img.cache_control)
                            {
                                *prompt_cache_breakpoint = Some(PromptCacheBreakpoint {
                                    mode: "explicit".into(),
                                });
                            }
                            image_count += 1;
                            // Route the part by mode; no intermediate clones.
                            match mode {
                                ToolResultImages::Array => ordered.push(part),
                                ToolResultImages::Lift => images.push(part),
                                ToolResultImages::Drop => {}
                            }
                        }
                        None => {
                            tracing::warn!(
                                "dropping image block in tool_result that could not be decoded"
                            );
                        }
                    }
                    continue;
                }
                if block.get("type").and_then(Value::as_str) == Some("document") {
                    // A PDF the tool returned (Claude Code's Read tool does
                    // this). Only the array form can carry a file part, so
                    // `lift` and `drop` modes get the note instead.
                    let forward = forward_documents && matches!(mode, ToolResultImages::Array);
                    for part in DocumentBlockParam::deserialize(block)
                        .map_or_else(|_| Vec::new(), |doc| convert_document_block(&doc, forward))
                    {
                        match part {
                            InputContentPart::InputText { text, .. } => {
                                push_text(text, &mut ordered);
                            }
                            // A file or image part makes the output the
                            // array form, like an image does.
                            other => {
                                image_count += 1;
                                match mode {
                                    ToolResultImages::Array => ordered.push(other),
                                    ToolResultImages::Lift => images.push(other),
                                    ToolResultImages::Drop => {}
                                }
                            }
                        }
                    }
                    continue;
                }
                // Any other block with a string `text` contributes it,
                // matching the previous behaviour (which kept text from
                // blocks of any type).
                match block.get("text").and_then(Value::as_str) {
                    Some(t) => push_text(t.to_owned(), &mut ordered),
                    None => {
                        let block_type = block
                            .get("type")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown");
                        tracing::warn!(
                            block_type,
                            "dropping non-text, non-image block in tool_result"
                        );
                    }
                }
            }
        }
        None => {}
    }

    let joined = text_parts.join("\n");
    let is_error = tr.is_error == Some(true);

    // The `[TOOL_ERROR]` marker must reach the model in every mode.
    if is_error {
        // Only a text part that LEADS the array can carry the marker: if an
        // image comes first, prefixing a later text part would put the
        // marker after the image, so lead with a marker part instead.
        match ordered.first_mut() {
            Some(InputContentPart::InputText { text, .. }) => {
                *text = format!("[TOOL_ERROR]\n{text}");
            }
            _ => ordered.insert(
                0,
                InputContentPart::InputText {
                    text: "[TOOL_ERROR]\n".to_string(),
                    prompt_cache_breakpoint: None,
                },
            ),
        }
    }
    let text = if is_error {
        format!("[TOOL_ERROR]\n{joined}")
    } else {
        joined
    };

    if image_count == 0 {
        return TranslatedToolResult {
            output: FunctionCallOutputBody::Text(text),
            lifted_images: Vec::new(),
        };
    }

    match mode {
        ToolResultImages::Array => TranslatedToolResult {
            output: FunctionCallOutputBody::Parts(ordered),
            lifted_images: Vec::new(),
        },
        ToolResultImages::Lift => TranslatedToolResult {
            // An image-only result would otherwise be an empty-string
            // output, which some upstreams reject (and which tells the model
            // nothing). Point it at the follow-up message instead.
            output: FunctionCallOutputBody::Text(if text.is_empty() {
                "(the tool returned image output; see the following message)".to_string()
            } else {
                text
            }),
            lifted_images: images,
        },
        ToolResultImages::Drop => {
            tracing::warn!(
                count = image_count,
                "dropping image(s) in tool_result (tool_result_images = \"drop\")"
            );
            TranslatedToolResult {
                output: FunctionCallOutputBody::Text(text),
                lifted_images: Vec::new(),
            }
        }
    }
}

/// Separator placed around replayed reasoning when it is folded into the
/// assistant message (`ReasoningReplayMode::Text`). The markers make the
/// boundary between the model's prior reasoning and its visible answer
/// explicit to the upstream.
pub const THINKING_PREFIX: &str = "<thinking>\n";
pub const THINKING_SUFFIX: &str = "\n</thinking>\n\n";

/// Delimiters for replayed advisor advice folded into an assistant
/// `output_text` part. Follow-up turns must see prior advice as prior context,
/// not as words the assistant is saying now.
const ADVISOR_ADVICE_PREFIX: &str = "[Advisor advice from a previous turn: ";
const ADVISOR_ADVICE_SUFFIX: &str = "]\n";

/// Delimiters around the page text replayed from an echoed
/// `web_fetch_tool_result` block.
///
/// The text is the CONTENT OF A WEB PAGE the model fetched on a previous
/// turn, not something the assistant said. Folding it into the
/// assistant's own `output_text` without a delimiter would attribute a
/// web page's words to the assistant — and a page can contain anything,
/// including text that looks like instructions. The delimiters make the
/// provenance explicit.
const WEB_FETCH_CONTENT_PREFIX: &str = "[Web page content fetched earlier: ";
const WEB_FETCH_CONTENT_SUFFIX: &str = "]\n";

/// Pull the URL and the extracted text out of an echoed
/// `web_fetch_tool_result` block's opaque `content` union.
///
/// The shape Anthropic sends is:
///
/// ```json
/// {
///   "type": "web_fetch_result",
///   "url": "https://example.com/article",
///   "content": {
///     "type": "document",
///     "source": { "type": "text", "media_type": "text/plain", "data": "…" }
///   }
/// }
/// ```
///
/// An ERROR result carries no text (`content.type` is
/// `web_fetch_tool_result_error`), so it yields an empty string and the
/// caller replays nothing — an error is not page content.
#[must_use]
pub fn web_fetch_result_fields(content: &Value) -> (String, String) {
    // Only a successful result carries content.
    if content.get("type").and_then(Value::as_str) != Some("web_fetch_result") {
        return (String::new(), String::new());
    }
    let url = content
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let text = content
        .get("content")
        .and_then(|c| c.get("source"))
        .and_then(|s| s.get("data"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    (url, text)
}

/// Neutralise the reasoning fence inside replayed reasoning text.
///
/// The reasoning is model output from a *previous* turn, and the model may
/// have quoted text containing `</thinking>` — from a file it read, a web
/// page, a tool result. Left alone, that closes the fence early and the
/// remainder of the reasoning lands in the assistant turn's *visible*
/// region, where the upstream reads it as the assistant's own words: quoted
/// content becomes a self-issued instruction, persisted on every later turn.
///
/// Escaping the fence tokens keeps the boundary unambiguous. Only the `<`
/// is replaced, so the rest of the quoted text is left intact.
fn escape_thinking_fence(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains('<') {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 16);
    let mut last = 0usize;
    let mut idx = 0usize;
    while idx < text.len() {
        if !text.is_char_boundary(idx) {
            idx += 1;
            continue;
        }
        let Some(rest) = text[idx..].strip_prefix('<') else {
            idx += 1;
            continue;
        };
        // Case-insensitive, so `</THINKING>` and `</thinking >` are caught
        // too — an upstream that matches the fence case-insensitively would
        // otherwise still be confused.
        let is_fence = starts_with_ignore_ascii_case(rest, "thinking")
            || starts_with_ignore_ascii_case(rest, "/thinking");
        if is_fence {
            out.push_str(&text[last..idx]);
            out.push_str("&lt;");
            last = idx + 1;
        }
        idx += 1;
    }
    out.push_str(&text[last..]);
    std::borrow::Cow::Owned(out)
}

/// Does `haystack` start with `needle`, ignoring ASCII case?
fn starts_with_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    let mut h = haystack.chars();
    for n in needle.chars() {
        match h.next() {
            Some(c) if c.eq_ignore_ascii_case(&n) => {}
            _ => return false,
        }
    }
    true
}

/// A deterministic synthetic id for a replayed reasoning item.
///
/// The upstream's own `rs_…` id is lost at the Anthropic wire boundary (the
/// `thinking` block carries no id), but OpenAI's schema marks `id` as a
/// required field of a reasoning input item, and some endpoints reject the
/// item outright when it is missing.
///
/// The id is derived from the reasoning text AND its signature, so that the
/// same block replays to the same id on every turn (keeping the upstream's
/// prompt cache stable) while two blocks that share text but differ in
/// signature still get distinct ids. FNV-1a is used rather than
/// `DefaultHasher` because the latter's output is not guaranteed stable
/// across Rust releases.
fn synthetic_reasoning_id(reasoning: &str, signature: &str) -> String {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = FNV_OFFSET;
    let bytes = reasoning
        .as_bytes()
        .iter()
        .copied()
        .chain(std::iter::once(0u8))
        .chain(signature.as_bytes().iter().copied());
    for byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("rs_proxy_{hash:016x}")
}

fn append_assistant_message(
    out: &mut Vec<InputItem>,
    content: &MessageContent,
    replay: &ReasoningReplay,
) -> Result<()> {
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
            let mut reasoning = String::new();
            let mut signature = String::new();
            let mut thinking_blocks = 0usize;
            let mut calls: Vec<(String, String, String)> = Vec::new(); // (id, name, arguments)
            // Ids of echoed `server_tool_use{name:"advisor"}` blocks, in wire
            // order. Kept so the paired result below can be attributed; they
            // must NOT become Responses `function_call` items (see below).
            let mut advisor_consultation_ids: Vec<String> = Vec::new();
            // Ids of echoed `server_tool_use{name:"web_fetch"}` blocks, in
            // wire order. Same rule as the advisor's: they must NOT become
            // Responses `function_call` items, because the paired RESULT
            // block carries the page text into context instead.
            let mut web_fetch_use_ids: Vec<String> = Vec::new();
            // `(id, query)` of echoed `web_search` uses, so each result can
            // say what was searched for.
            let mut web_search_queries: Vec<(String, String)> = Vec::new();
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
                    ContentBlockParam::Thinking(th) => {
                        // The client's echoed thinking block. Dropping it
                        // (the pre-fix behaviour) destroys the model's memory
                        // of its own prior reasoning, so replay it.
                        if replay.mode == ReasoningReplayMode::Off {
                            tracing::debug!(
                                chars = th.thinking.len(),
                                "dropping thinking block from assistant message \
                                 (reasoning replay is disabled for this model)"
                            );
                            continue;
                        }
                        thinking_blocks += 1;
                        let trimmed = th.thinking.trim();
                        if !trimmed.is_empty() {
                            if !reasoning.is_empty() {
                                reasoning.push('\n');
                            }
                            reasoning.push_str(&escape_thinking_fence(trimmed));
                        }
                        // The signature is the upstream's encrypted
                        // chain-of-thought. It is only meaningful to the
                        // upstream that issued it, so only `Native` mode
                        // forwards it.
                        if !th.signature.is_empty() {
                            signature = th.signature.clone();
                        }
                    }
                    ContentBlockParam::RedactedThinking(rt) => {
                        // Anthropic-encrypted blob with no readable text. It
                        // is NOT the upstream's `encrypted_content`, so
                        // replaying it as one would fail upstream
                        // verification.
                        tracing::warn!(
                            bytes = rt.data.len(),
                            "dropping redacted_thinking block from assistant message \
                             (opaque Anthropic blob; not replayable upstream)"
                        );
                    }
                    ContentBlockParam::ServerToolUse(stu) => {
                        // Claude Code echoes the advisor `server_tool_use`
                        // verbatim on the next turn. Do NOT synthesize a
                        // Responses `function_call`: it would need a matching
                        // `function_call_output` (an Anthropic advisor result
                        // is not one) and would orphan the call upstream.
                        // Record the consultation instead; the paired RESULT
                        // block is what carries the advice into context.
                        if stu.name == "advisor" {
                            advisor_consultation_ids.push(stu.id.clone());
                            tracing::debug!(
                                id = %stu.id,
                                "echoed advisor server_tool_use; no function_call emitted"
                            );
                        } else if stu.name == crate::tools::web_fetch::WEB_FETCH_TOOL_NAME {
                            // Same rule for the server-side `web_fetch`
                            // tool: the paired RESULT block carries the
                            // page text into context, so no `function_call`
                            // is emitted for the echoed use.
                            web_fetch_use_ids.push(stu.id.clone());
                            tracing::debug!(
                                id = %stu.id,
                                "echoed web_fetch server_tool_use; no function_call emitted"
                            );
                        } else if stu.name == "web_search" {
                            // The paired RESULT block carries what the model
                            // saw into context (see `web_search::history`);
                            // no `function_call` is emitted for the use.
                            let query = stu
                                .input
                                .get("query")
                                .and_then(Value::as_str)
                                .unwrap_or_default();
                            web_search_queries.push((stu.id.clone(), query.to_owned()));
                        } else {
                            tracing::warn!(
                                name = %stu.name,
                                "dropping unsupported server_tool_use block from assistant message"
                            );
                        }
                    }
                    ContentBlockParam::AdvisorToolResult(res) => {
                        // `content` is an opaque 3-way union. Only
                        // `advisor_result{text}` carries plaintext advice;
                        // `advisor_redacted_result` and
                        // `advisor_tool_result_error` must never be presented
                        // as advice that was actually given.
                        let kind = res.content.get("type").and_then(Value::as_str);
                        match kind {
                            Some("advisor_result") => {
                                let advice = res
                                    .content
                                    .get("text")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default();
                                if advice.trim().is_empty() {
                                    tracing::debug!(
                                        "empty advisor_result text; no advice replayed"
                                    );
                                } else {
                                    // The API requires the pair contiguous in
                                    // one assistant message; tolerate a
                                    // partial echo but still replay the text
                                    // so prior advice is never lost.
                                    let paired = advisor_consultation_ids
                                        .iter()
                                        .any(|id| id == &res.tool_use_id);
                                    if !paired {
                                        tracing::debug!(
                                            tool_use_id = %res.tool_use_id,
                                            "echoed advisor result has no preceding \
                                             server_tool_use in this message"
                                        );
                                    }
                                    // Fold the advice into the SAME assistant
                                    // `output_text` part, delimited so the
                                    // upstream can distinguish it from the
                                    // assistant's own words.
                                    text.push('\n');
                                    text.push_str(ADVISOR_ADVICE_PREFIX);
                                    text.push_str(advice);
                                    text.push_str(ADVISOR_ADVICE_SUFFIX);
                                }
                            }
                            _ => tracing::debug!(
                                kind = ?kind,
                                "echoed advisor result carries no plaintext advice"
                            ),
                        }
                    }
                    ContentBlockParam::WebFetchToolResult(res) => {
                        // The echoed `web_fetch_tool_result`. The page text
                        // it carries is what the model needs in context on
                        // the NEXT turn, so it is folded into the same
                        // assistant `output_text` part — delimited, so the
                        // upstream can tell it from the assistant's own
                        // words.
                        let (url, page_text) = web_fetch_result_fields(&res.content);
                        if page_text.trim().is_empty() {
                            tracing::debug!(
                                kind = ?res.content.get("type").and_then(|v| v.as_str()),
                                "echoed web_fetch result carries no page text"
                            );
                        } else {
                            let paired = web_fetch_use_ids.iter().any(|id| id == &res.tool_use_id);
                            if !paired {
                                tracing::debug!(
                                    tool_use_id = %res.tool_use_id,
                                    "echoed web_fetch result has no preceding \
                                     server_tool_use in this message"
                                );
                            }
                            text.push('\n');
                            text.push_str(WEB_FETCH_CONTENT_PREFIX);
                            if !url.is_empty() {
                                text.push_str(&url);
                                text.push_str(": ");
                            }
                            text.push_str(&page_text);
                            text.push_str(WEB_FETCH_CONTENT_SUFFIX);
                        }
                    }
                    ContentBlockParam::WebSearchToolResult(res) => {
                        // What the search returned is what the model needs in
                        // context on the NEXT turn: fold it into the same
                        // assistant `output_text`, delimited so the upstream
                        // can tell it from the assistant's own words.
                        let query = web_search_queries
                            .iter()
                            .find(|(id, _)| id == &res.tool_use_id)
                            .map(|(_, q)| q.as_str());
                        text.push_str(&crate::tools::web_search::history::replay_text(
                            query,
                            &res.content,
                        ));
                    }
                    ContentBlockParam::Image(_)
                    | ContentBlockParam::Document(_)
                    | ContentBlockParam::ToolResult(_)
                    | ContentBlockParam::Unknown => {
                        tracing::warn!("dropping unsupported block from assistant message");
                    }
                }
            }

            // Bound the replayed reasoning so a pathological summary cannot
            // blow up the upstream context window.
            let reasoning = if replay.max_chars > 0 && reasoning.chars().count() > replay.max_chars
            {
                tracing::warn!(
                    chars = reasoning.chars().count(),
                    limit = replay.max_chars,
                    "truncating replayed reasoning text"
                );
                reasoning.chars().take(replay.max_chars).collect()
            } else {
                reasoning
            };

            // Assemble the assistant's visible text. In `Text` mode the
            // replayed reasoning is folded into the SAME `output_text` part
            // (reasoning first, then the visible answer) — one part, not
            // two, so a provider that only reads `content[0]` still sees the
            // answer.
            if !reasoning.is_empty() {
                replay
                    .replayed_chars
                    .set(replay.replayed_chars.get() + reasoning.chars().count() as u64);
            }

            let body = match replay.mode {
                ReasoningReplayMode::Text if !reasoning.is_empty() => {
                    let mut b = String::with_capacity(reasoning.len() + text.len() + 24);
                    b.push_str(THINKING_PREFIX);
                    b.push_str(&reasoning);
                    b.push_str(THINKING_SUFFIX);
                    b.push_str(&text);
                    b
                }
                _ => text.clone(),
            };

            if replay.mode == ReasoningReplayMode::Native && thinking_blocks > 1 {
                // Anthropic's interleaved thinking emits one block per tool
                // call. The Responses API pairs a reasoning item with the
                // item that immediately follows it, so a faithful replay
                // would need one reasoning item per block. The proxy merges
                // them instead, which mis-attributes the reasoning to the
                // first call and keeps only the last signature.
                tracing::warn!(
                    blocks = thinking_blocks,
                    "merging multiple thinking blocks into one reasoning item; \
                     the upstream sees them as a single block"
                );
            }

            if replay.mode == ReasoningReplayMode::Native
                && let Some(item) = build_reasoning_item(&reasoning, &signature)
            {
                // OpenAI pairs a reasoning item with the item that follows
                // it, so the reasoning must come first. The upstream's own
                // `rs_…` id is lost at the Anthropic wire boundary, so a
                // stable synthetic one is used instead.
                out.push(item);
            }

            if !body.is_empty() || calls.is_empty() {
                // Always emit a Message item for the text (even if empty)
                // so that the assistant turn is well-formed when there
                // are no tool calls.
                out.push(InputItem::Message {
                    role: "assistant".into(),
                    content: if body.is_empty() {
                        Vec::new()
                    } else {
                        vec![InputContentPart::OutputText { text: body }]
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

/// Build a Responses `reasoning` input item for a replayed thinking block.
///
/// The Anthropic `thinking` block carries only the reasoning *text* and the
/// upstream's `encrypted_content`; the upstream's own `rs_…` item id is lost
/// at the Anthropic wire boundary, so a synthetic id is used instead.
///
/// Both `content` and `summary` carry the text: OpenAI's Responses API reads
/// `summary`, DeepSeek's reads `content`. `encrypted_content` is forwarded
/// only when the upstream actually issued it.
fn build_reasoning_item(reasoning: &str, signature: &str) -> Option<InputItem> {
    if reasoning.is_empty() && signature.is_empty() {
        return None;
    }
    Some(InputItem::Reasoning {
        // The upstream's own id is unrecoverable, but OpenAI's schema marks
        // `id` as required on a reasoning item — a synthetic one is better
        // than a missing one, and is stable across turns. It is derived
        // from the reasoning text AND the signature, so two reasoning
        // items that share text but differ in signature get different ids.
        id: Some(synthetic_reasoning_id(reasoning, signature)),
        content: if reasoning.is_empty() {
            Vec::new()
        } else {
            vec![ReasoningTextPart {
                kind: "reasoning_text".into(),
                text: reasoning.to_owned(),
            }]
        },
        // `summary` is always present (possibly empty): it is a required
        // field of a reasoning item, and `skip_serializing_if` would drop
        // it when the reasoning has no text.
        summary: if reasoning.is_empty() {
            Vec::new()
        } else {
            vec![ReasoningSummaryPart {
                kind: "summary_text".into(),
                text: reasoning.to_owned(),
            }]
        },
        encrypted_content: if signature.is_empty() {
            None
        } else {
            Some(signature.to_owned())
        },
    })
}

fn append_system_message(instructions: &mut String, content: &MessageContent) -> Result<()> {
    let text = system_message_text(content);
    if !text.is_empty() {
        if !instructions.is_empty() {
            instructions.push('\n');
        }
        instructions.push_str(&text);
    }
    Ok(())
}

/// The text of a `system` message. Non-text blocks (`thinking`, and the
/// `tool_addition` marker Claude Code uses to announce a tool) carry no text.
fn system_message_text(content: &MessageContent) -> String {
    match content {
        MessageContent::Text(s) => s.clone(),
        MessageContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlockParam::Text(t) => Some(t.text.as_str()),
                ContentBlockParam::Thinking(th) => {
                    tracing::warn!(
                        chars = th.thinking.len(),
                        "dropping thinking block from system message"
                    );
                    None
                }
                ContentBlockParam::RedactedThinking(rt) => {
                    tracing::warn!(
                        bytes = rt.data.len(),
                        "dropping redacted_thinking block from system message"
                    );
                    None
                }
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

/// Strip the `# Advisor Tool` section from every inline `system` message in
/// `items`. The same single-section strip the instructions get, for the
/// system messages kept in the conversation.
pub fn strip_advisor_from_system_items(items: &mut [InputItem]) {
    for item in items {
        if let InputItem::Message { role, content } = item
            && role == "system"
        {
            for part in content {
                if let InputContentPart::InputText { text, .. } = part {
                    strip_advisor_tool_instructions(text);
                }
            }
        }
    }
}

/// Map Anthropic's `ToolChoice` to a Responses-API `ToolChoice` and
/// capture the `disable_parallel_tool_use` flag separately so the
/// caller can set `parallel_tool_calls` on the outbound request.
/// Returns `(tool_choice, disable_parallel_tool_use)`.
///
/// `native_web_search` says whether the outbound tools include the built-in
/// `web_search` (the client declared the server tool, not a function tool).
fn map_tool_choice(
    choice: &ToolChoice,
    native_web_search: bool,
) -> (ResponsesToolChoice, Option<bool>) {
    match choice {
        ToolChoice::Auto {
            disable_parallel_tool_use,
        } => (
            ResponsesToolChoice::Simple("auto".to_string()),
            *disable_parallel_tool_use,
        ),
        ToolChoice::Any {
            disable_parallel_tool_use,
        } => (
            ResponsesToolChoice::Simple("required".to_string()),
            *disable_parallel_tool_use,
        ),
        ToolChoice::None {} => (ResponsesToolChoice::Simple("none".to_string()), None),
        ToolChoice::Tool {
            name,
            disable_parallel_tool_use,
        } => {
            // The Responses API distinguishes function tools (with
            // `{type: "function", name: "..."}`) from built-in tools like
            // `web_search` (which use `{type: "web_search"}` with no name).
            // If we forward `{type: "function", name: "web_search"}` while
            // the tools array contains a `web_search` built-in, the
            // upstream rejects with: Tool choice 'function' not found in
            // 'tools' parameter. For built-in server tools, fall back to
            // `"auto"` so the model can decide when to invoke them.
            let tc = if native_web_search && name == "web_search" {
                ResponsesToolChoice::Simple("auto".to_string())
            } else {
                ResponsesToolChoice::Function {
                    kind: "function".to_string(),
                    name: name.clone(),
                }
            };
            (tc, *disable_parallel_tool_use)
        }
    }
}

#[cfg(test)]
mod tests;
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
///     surfaced as a `[error] ` text block so the client sees it. This is a
///     fallback only: the proxy turns a failed reply into an error before it
///     gets here (`AppError::from_failed_response`, checked in
///     `Attempt::execute`), so a client sees a real error status it can
///     retry on, not a message.
///   - `status == "cancelled"` → `EndTurn`.
/// - `usage`: `input_tokens` minus the cache counts → `input_tokens`,
///   `cached_tokens` → `cache_read_input_tokens`, `cache_write_tokens` →
///   `cache_creation_input_tokens`, `output_tokens` → `output_tokens`,
///   `output_tokens_details.reasoning_tokens` → `thinking_tokens`.
#[must_use]
pub fn responses_to_anthropic(
    resp: &ResponsesResponse,
    registry: &crate::repair::ToolSchemaRegistry,
) -> Message {
    let (mut content, web_search_count, truncated_tool_call) =
        build_content_blocks(&resp.output, registry);

    // Defensive fallback: the proxy normally never gets here with a failed
    // reply (see the doc comment above). If it does, and we have no output
    // blocks of our own, surface the upstream error message as a
    // `[error] ...` text block instead of an empty message body. If the reply
    // produced output before failing, keep the output and skip the block.
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
    // A tool call the upstream cut short is an *incomplete* call: the
    // client must not run it. Anthropic reports `max_tokens` for one,
    // and that is the signal the client keys on to retry the turn with
    // a bigger budget.
    //
    // Only a reply that claims to be complete is overridden. A reply
    // the upstream marked `incomplete` carries its own reason, and
    // that reason wins — `max_output_tokens` already maps to
    // `max_tokens`, and `content_filter` maps to `refusal`, which
    // this must not overwrite.
    let stop_reason = if truncated_tool_call
        && resp.status == "completed"
        && matches!(
            stop_reason,
            Some(StopReason::EndTurn) | Some(StopReason::ToolUse)
        ) {
        Some(StopReason::MaxTokens)
    } else {
        stop_reason
    };

    let usage = resp
        .usage
        .as_ref()
        .map(|u| AnthropicUsage {
            input_tokens: u.net_input_tokens(),
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
            server_tool_use: if web_search_count > 0 {
                Some(ServerToolUseUsage {
                    web_search_requests: web_search_count,
                    ..ServerToolUseUsage::default()
                })
            } else {
                None
            },
            iterations: None,
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
            iterations: None,
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
/// `web_search_count` field in [`build_content_blocks`] and the
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
/// Whether a function call from the model is a web search made as a function
/// (`WebSearch` or `web_search`) rather than through the built-in tool.
///
/// A `web_search` the client declared as its own function tool is in the
/// registry (only function tools with a schema are), and a call to it is an
/// ordinary tool call, not a search the proxy should dress up as a server tool.
pub fn is_web_search_function_call(
    name: &str,
    registry: &crate::repair::ToolSchemaRegistry,
) -> bool {
    name == "WebSearch" || (name == "web_search" && registry.get(name).is_none())
}

fn extract_web_search_citations(
    items: &[OutputItem],
    registry: &crate::repair::ToolSchemaRegistry,
) -> (Vec<(String, String)>, u32) {
    let mut citations = Vec::new();
    let mut search_count = 0u32;

    for item in items {
        match item {
            OutputItem::WebSearchCall { .. } => {
                tracing::debug!("detected web_search_call output item");
                search_count = search_count.saturating_add(1);
            }
            OutputItem::FunctionCall { name, .. }
                if is_web_search_function_call(name, registry) =>
            {
                tracing::debug!(name = %name, "detected web search FunctionCall");
                search_count = search_count.saturating_add(1);
            }
            OutputItem::FunctionCall { .. } => {}
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
                            if ann.get("type").and_then(|v| v.as_str()) == Some("url_citation")
                                && let (Some(url), Some(title)) = (
                                    ann.get("url").and_then(|v| v.as_str()),
                                    ann.get("title").and_then(|v| v.as_str()),
                                )
                            {
                                citations.push((url.to_string(), title.to_string()));
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

    // Citation-only responses still represent one search. When the upstream
    // exposes a search output item without citations, synthesize one result so
    // the downstream search counter remains visible.
    if search_count == 0 && !citations.is_empty() {
        search_count = 1;
    }

    tracing::info!(
        citations_found = citations.len(),
        search_count,
        "web search citation extraction"
    );
    (citations, search_count)
}

fn build_content_blocks(
    items: &[OutputItem],
    registry: &crate::repair::ToolSchemaRegistry,
) -> (Vec<ResponseContentBlock>, u32, bool) {
    let (citations, web_search_count) = extract_web_search_citations(items, registry);

    let mut blocks = Vec::new();
    // Whether a tool call's arguments were cut short by the upstream.
    let mut truncated_tool_call = false;

    // Inject one server-tool/result pair per search with a distinct ID.
    for search_index in 0..web_search_count {
        let tool_use_id = format!("stoolu_web_search_{:02}", search_index + 1);
        let result_content = if search_index == 0 {
            citations
                .iter()
                .map(|(url, title)| WebSearchResult {
                    url: url.clone(),
                    title: title.clone(),
                    encrypted_content: String::new(),
                })
                .collect()
        } else {
            Vec::new()
        };
        blocks.push(ResponseContentBlock::ServerToolUse {
            id: tool_use_id.clone(),
            name: "web_search".to_string(),
            input: serde_json::json!({"query": "Web search"}),
        });
        blocks.push(ResponseContentBlock::WebSearchToolResult {
            block: WebSearchToolResultBlock {
                tool_use_id,
                content: result_content,
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
                // Parse the raw arguments. Unparseable blobs used to be
                // shipped as `{"_raw_arguments": ...}` — a synthetic key
                // every strict client schema rejects. Ship `{}` instead:
                // the client's validator then reports a specific
                // "missing required parameter" error the model can act
                // on, rather than an opaque unexpected-key rejection.
                //
                // A blob the upstream cut short (it ran out of output
                // tokens) is neither: the parameters that were finished
                // are kept and the one in flight is dropped, which is
                // what Anthropic's own API does.
                let parsed = match crate::repair::read_arguments(arguments) {
                    crate::repair::Arguments::Whole(value) => value,
                    crate::repair::Arguments::Truncated(value) => {
                        tracing::warn!(
                            tool = %name,
                            "upstream tool arguments were cut short; keeping the parameters that were finished"
                        );
                        truncated_tool_call = true;
                        value
                    }
                    crate::repair::Arguments::Malformed => {
                        tracing::warn!(
                            tool = %name,
                            "upstream tool arguments are not valid JSON; shipping empty input"
                        );
                        Value::Object(Default::default())
                    }
                };
                // Invert the strict sanitization so the client sees
                // input that conforms to the schema *it* sent us.
                let input = match registry.get(name) {
                    Some(original) => {
                        let repaired = crate::repair::repair(parsed, original);
                        if let Some(violation) = crate::repair::validate(&repaired, original) {
                            // Unrepairable (e.g. a genuinely required
                            // parameter the model botched). Ship the
                            // best-effort input anyway; the proxy's
                            // hidden retry loop may re-query the
                            // upstream with this violation attached.
                            tracing::warn!(
                                tool = %name,
                                violation = %violation,
                                "tool input still violates the original schema after repair"
                            );
                        }
                        repaired
                    }
                    None => parsed,
                };
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
    (blocks, web_search_count, truncated_tool_call)
}

#[cfg(test)]
mod response_tests;
