//! Translation between Anthropic Messages API and OpenAI Chat Completions.
//!
//! Three entry points:
//! - [`anthropic_to_openai`] — inbound request, used by the proxy's
//!   `/v1/messages` handler. Converts Claude Code's request into the
//!   shape the upstream expects.
//! - [`openai_to_anthropic`] — non-streaming response. The upstream
//!   returned a full JSON message; we map it to Anthropic's
//!   `Message` and ship it back to the client.
//! - [`StreamTranslator`] (in the `stream` submodule, if/when present) —
//!   turns an OpenAI SSE byte stream into Anthropic SSE events. Not in
//!   this file; lives in its own file when we split the module up.
//!
//! Out-of-scope fields (vision, thinking, cache_control, server tools) are
//! silently dropped on the request side. On the response side, OpenAI's
//! `usage.prompt_tokens` becomes Anthropic's `input_tokens` and
//! `completion_tokens` becomes `output_tokens`; cache fields are 0.

use crate::anthropic::{
    ContentBlockParam, CreateMessageRequest, MessageContent, MessageParam, MessageRole,
    SystemPrompt, ToolChoice,
};
use crate::openai::{
    ChatCompletionRequest, ChatMessage, FunctionDef, StreamOptions, ToolChoiceFunction,
    ToolChoiceValue, ToolDef, UserContent,
};
use anyhow::{Context, Result, anyhow};
use serde_json::Value;

/// Translate a Claude Code request into the upstream OpenAI shape.
///
/// Translation rules:
/// - `system` (string or text blocks) → leading `system` message.
/// - `messages[]` is walked in order:
///   - `user` string content → one `user` message.
///   - `user` block content → emit one `user` message per run of text
///     blocks and one `tool` message per `tool_result` block, preserving
///     the original ordering. Image / unknown blocks are dropped.
///   - `assistant` text → `assistant` message with `content: text`.
///   - `assistant` text + `tool_use` → `assistant` message with
///     `content: <text>` and `tool_calls: [...]`.
///   - `assistant` only `tool_use` → `assistant` message with `content: null`
///     and `tool_calls: [...]`.
///   - `system` role → additional system message (legacy).
/// - `tools[]` → `tools[]` with `parameters` lifted from `input_schema`.
/// - `tool_choice`: `Auto`→`"auto"`, `Any`→`"required"`, `Tool{name}`→structured,
///   `None{}`→`"none"`.
/// - `temperature`, `top_p`, `max_tokens`, `stop_sequences` map 1:1.
/// - `stream` is preserved; when true, `stream_options.include_usage` is set
///   so the upstream sends a terminal usage chunk.
/// - `metadata.user_id` → `user` field.
pub fn anthropic_to_openai(req: &CreateMessageRequest) -> Result<ChatCompletionRequest> {
    let stream = req.stream.unwrap_or(false);
    let mut messages = Vec::new();

    if let Some(system) = &req.system {
        let text = system_text(system);
        if !text.is_empty() {
            messages.push(ChatMessage::System { content: text });
        }
    }

    for msg in &req.messages {
        append_message_translation(&mut messages, msg)?;
    }

    let tools = req.tools.as_ref().map(|tools| {
        tools
            .iter()
            .map(|t| ToolDef {
                kind: "function".to_string(),
                function: FunctionDef {
                    name: t.name.clone(),
                    description: t.description.clone(),
                    parameters: t.input_schema.clone(),
                },
            })
            .collect()
    });

    let tool_choice = req.tool_choice.as_ref().map(map_tool_choice);

    let user = req.metadata.as_ref().and_then(|m| m.user_id.clone());

    let stream_options = if stream {
        Some(StreamOptions {
            include_usage: true,
        })
    } else {
        None
    };

    Ok(ChatCompletionRequest {
        model: req.model.clone(),
        messages,
        temperature: req.temperature,
        top_p: req.top_p,
        max_tokens: Some(req.max_tokens),
        stop: req.stop_sequences.clone(),
        tools,
        tool_choice,
        stream,
        stream_options,
        user,
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

fn append_message_translation(out: &mut Vec<ChatMessage>, msg: &MessageParam) -> Result<()> {
    match msg.role {
        MessageRole::User => append_user_message(out, &msg.content),
        MessageRole::Assistant => append_assistant_message(out, &msg.content),
        MessageRole::System => append_system_message(out, &msg.content),
    }
}

fn append_user_message(out: &mut Vec<ChatMessage>, content: &MessageContent) -> Result<()> {
    match content {
        MessageContent::Text(s) => {
            out.push(ChatMessage::User {
                content: UserContent::Text(s.clone()),
            });
            Ok(())
        }
        MessageContent::Blocks(blocks) => {
            // Walk the block list in order, emitting one `user` message per
            // run of text blocks and one `tool` message per `tool_result`.
            // Anything else is dropped (with a tracing warning in the
            // production code path; tests just skip).
            let mut text_buf = String::new();
            for block in blocks {
                match block {
                    ContentBlockParam::Text(t) => {
                        if !text_buf.is_empty() {
                            text_buf.push('\n');
                        }
                        text_buf.push_str(&t.text);
                    }
                    ContentBlockParam::ToolResult(tr) => {
                        flush_user_text(out, &mut text_buf);
                        out.push(ChatMessage::Tool {
                            content: tool_result_text(&tr.content),
                            tool_call_id: tr.tool_use_id.clone(),
                        });
                    }
                    ContentBlockParam::Image(_) => {
                        tracing::warn!("dropping image block — not supported in MVP");
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
            flush_user_text(out, &mut text_buf);
            Ok(())
        }
    }
}

fn flush_user_text(out: &mut Vec<ChatMessage>, buf: &mut String) {
    if !buf.is_empty() {
        out.push(ChatMessage::User {
            content: UserContent::Text(std::mem::take(buf)),
        });
    }
}

fn tool_result_text(content: &Option<crate::anthropic::ToolResultContent>) -> String {
    match content {
        Some(crate::anthropic::ToolResultContent::Text(s)) => s.clone(),
        Some(crate::anthropic::ToolResultContent::Blocks(blocks)) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        None => String::new(),
    }
}

fn append_assistant_message(out: &mut Vec<ChatMessage>, content: &MessageContent) -> Result<()> {
    match content {
        MessageContent::Text(s) => {
            out.push(ChatMessage::Assistant {
                content: Some(s.clone()),
                tool_calls: None,
            });
            Ok(())
        }
        MessageContent::Blocks(blocks) => {
            let mut text = String::new();
            let mut calls = Vec::new();
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
                        calls.push(crate::openai::ToolCall {
                            id: tu.id.clone(),
                            kind: "function".to_string(),
                            function: crate::openai::FunctionCall {
                                name: tu.name.clone(),
                                arguments,
                            },
                        });
                    }
                    ContentBlockParam::Image(_)
                    | ContentBlockParam::ToolResult(_)
                    | ContentBlockParam::Unknown => {
                        tracing::warn!("dropping unsupported block from assistant message");
                    }
                }
            }
            out.push(ChatMessage::Assistant {
                content: if text.is_empty() { None } else { Some(text) },
                tool_calls: if calls.is_empty() { None } else { Some(calls) },
            });
            Ok(())
        }
    }
}

fn append_system_message(out: &mut Vec<ChatMessage>, content: &MessageContent) -> Result<()> {
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
        out.push(ChatMessage::System { content: text });
    }
    Ok(())
}

fn map_tool_choice(choice: &ToolChoice) -> ToolChoiceValue {
    match choice {
        ToolChoice::Auto { .. } => ToolChoiceValue::Simple("auto".to_string()),
        ToolChoice::Any { .. } => ToolChoiceValue::Simple("required".to_string()),
        ToolChoice::None {} => ToolChoiceValue::Simple("none".to_string()),
        ToolChoice::Tool { name, .. } => ToolChoiceValue::Function {
            kind: "function",
            function: ToolChoiceFunction { name: name.clone() },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anthropic::{
        ImageBlockParam, MessageParam, SystemPrompt, TextBlockParam, Tool, ToolChoice,
        ToolResultBlockParam, ToolUseBlockParam,
    };
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
                input_schema: json!({
                    "type": "object",
                    "properties": {"location": {"type": "string"}},
                    "required": ["location"],
                }),
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

    #[test]
    fn basic_request_translates_cleanly() {
        let req = fixture_request();
        let out = anthropic_to_openai(&req).unwrap();
        assert_eq!(out.model, "test-model");
        assert_eq!(out.max_tokens, Some(256));
        assert!(!out.stream);

        // System + user
        assert_eq!(out.messages.len(), 2);
        match &out.messages[0] {
            ChatMessage::System { content } => assert_eq!(content, "You are helpful."),
            _ => panic!("expected system message first"),
        }
        match &out.messages[1] {
            ChatMessage::User { content } => match content {
                UserContent::Text(t) => assert_eq!(t, "Hello"),
                _ => panic!("expected text user content"),
            },
            _ => panic!("expected user message second"),
        }

        // Tools lifted, input_schema → parameters
        let tools = out.tools.as_ref().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].kind, "function");
        assert_eq!(tools[0].function.name, "get_weather");
        assert_eq!(
            tools[0].function.description.as_deref(),
            Some("Get the weather")
        );
        assert_eq!(tools[0].function.parameters["type"], "object");

        // temperature passed through
        assert_eq!(out.temperature, Some(0.5));
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
        let out = anthropic_to_openai(&req).unwrap();
        match &out.messages[0] {
            ChatMessage::System { content } => {
                assert_eq!(content, "Be concise.\n\nUse examples.");
            }
            _ => panic!("expected system message first"),
        }
    }

    #[test]
    fn tool_use_in_assistant_becomes_tool_calls() {
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
                        content: Some(crate::anthropic::ToolResultContent::Text(
                            "72F sunny".into(),
                        )),
                        is_error: None,
                    },
                )]),
            },
        ];

        let out = anthropic_to_openai(&req).unwrap();
        // system + user + assistant + tool
        assert_eq!(out.messages.len(), 4);

        match &out.messages[2] {
            ChatMessage::Assistant {
                content,
                tool_calls,
            } => {
                assert_eq!(content.as_deref(), Some("Let me check."));
                let calls = tool_calls.as_ref().unwrap();
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].id, "toolu_01");
                assert_eq!(calls[0].function.name, "get_weather");
                // arguments is a JSON string of the input
                let parsed: Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
                assert_eq!(parsed["location"], "SF");
            }
            _ => panic!("expected assistant message with tool_calls"),
        }

        match &out.messages[3] {
            ChatMessage::Tool {
                content,
                tool_call_id,
            } => {
                assert_eq!(content, "72F sunny");
                assert_eq!(tool_call_id, "toolu_01");
            }
            _ => panic!("expected tool message"),
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
                    content: Some(crate::anthropic::ToolResultContent::Text(
                        "first result".into(),
                    )),
                    is_error: None,
                }),
                ContentBlockParam::Text(TextBlockParam {
                    text: "and:".into(),
                    cache_control: None,
                }),
                ContentBlockParam::ToolResult(ToolResultBlockParam {
                    tool_use_id: "toolu_02".into(),
                    content: Some(crate::anthropic::ToolResultContent::Text(
                        "second result".into(),
                    )),
                    is_error: None,
                }),
            ]),
        }];

        let out = anthropic_to_openai(&req).unwrap();
        // system + user("Here's what I got:") + tool("first result") + user("and:") + tool("second result")
        assert_eq!(out.messages.len(), 5);
        match &out.messages[1] {
            ChatMessage::User {
                content: UserContent::Text(t),
            } => assert_eq!(t, "Here's what I got:"),
            _ => panic!(),
        }
        match &out.messages[2] {
            ChatMessage::Tool {
                content,
                tool_call_id,
            } => {
                assert_eq!(content, "first result");
                assert_eq!(tool_call_id, "toolu_01");
            }
            _ => panic!(),
        }
        match &out.messages[3] {
            ChatMessage::User {
                content: UserContent::Text(t),
            } => assert_eq!(t, "and:"),
            _ => panic!(),
        }
        match &out.messages[4] {
            ChatMessage::Tool {
                content,
                tool_call_id,
            } => {
                assert_eq!(content, "second result");
                assert_eq!(tool_call_id, "toolu_02");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn user_with_image_block_drops_with_warning() {
        let mut req = fixture_request();
        req.messages = vec![MessageParam {
            role: MessageRole::User,
            content: MessageContent::Blocks(vec![
                ContentBlockParam::Text(TextBlockParam {
                    text: "What is this?".into(),
                    cache_control: None,
                }),
                ContentBlockParam::Image(ImageBlockParam {
                    source: json!({"type": "base64", "data": "..."}),
                }),
            ]),
        }];
        let out = anthropic_to_openai(&req).unwrap();
        // system + user(text only — image dropped)
        assert_eq!(out.messages.len(), 2);
        match &out.messages[1] {
            ChatMessage::User {
                content: UserContent::Text(t),
            } => assert_eq!(t, "What is this?"),
            _ => panic!(),
        }
    }

    #[test]
    fn tool_choice_maps() {
        let mut req = fixture_request();
        req.tool_choice = Some(ToolChoice::Auto {
            disable_parallel_tool_use: None,
        });
        let out = anthropic_to_openai(&req).unwrap();
        match out.tool_choice.unwrap() {
            ToolChoiceValue::Simple(s) => assert_eq!(s, "auto"),
            _ => panic!("expected simple tool_choice"),
        }

        req.tool_choice = Some(ToolChoice::Any {
            disable_parallel_tool_use: None,
        });
        let out = anthropic_to_openai(&req).unwrap();
        match out.tool_choice.unwrap() {
            ToolChoiceValue::Simple(s) => assert_eq!(s, "required"),
            _ => panic!("expected simple 'required'"),
        }

        req.tool_choice = Some(ToolChoice::None {});
        let out = anthropic_to_openai(&req).unwrap();
        match out.tool_choice.unwrap() {
            ToolChoiceValue::Simple(s) => assert_eq!(s, "none"),
            _ => panic!("expected simple 'none'"),
        }

        req.tool_choice = Some(ToolChoice::Tool {
            name: "get_weather".into(),
            disable_parallel_tool_use: None,
        });
        let out = anthropic_to_openai(&req).unwrap();
        match out.tool_choice.unwrap() {
            ToolChoiceValue::Function { function, .. } => {
                assert_eq!(function.name, "get_weather");
            }
            _ => panic!("expected structured tool_choice"),
        }
    }

    #[test]
    fn stream_true_sets_include_usage() {
        let mut req = fixture_request();
        req.stream = Some(true);
        let out = anthropic_to_openai(&req).unwrap();
        assert!(out.stream);
        let opts = out.stream_options.unwrap();
        assert!(opts.include_usage);
    }

    #[test]
    fn metadata_user_id_becomes_user() {
        let mut req = fixture_request();
        req.metadata = Some(crate::anthropic::Metadata {
            user_id: Some("user-123".into()),
        });
        let out = anthropic_to_openai(&req).unwrap();
        assert_eq!(out.user.as_deref(), Some("user-123"));
    }
}

// ─── Response translation (non-streaming) ─────────────────────────────────

use crate::anthropic::{Message, ResponseContentBlock, StopReason, Usage as AnthropicUsage};

/// Translate an OpenAI `ChatCompletionResponse` into an Anthropic `Message`.
///
/// The proxy returns this to Claude Code in the non-streaming path.
///
/// Translation rules:
/// - `id` is prefixed with `msg_` if it doesn't already start that way,
///   so the id looks like an Anthropic message id.
/// - `role` is always `"assistant"`.
/// - Content blocks: any `text` becomes a `text` block; each `tool_call`
///   becomes a `tool_use` block. Text comes first, then tool uses (the
///   order Claude Code expects).
/// - `stop_reason` mapping: `stop`→`EndTurn`, `length`→`MaxTokens`,
///   `tool_calls`→`ToolUse`, `content_filter`→`EndTurn`,
///   `function_call`→`ToolUse` (legacy), `null`/missing→`None`.
/// - `usage`: `input_tokens = prompt_tokens`, `output_tokens = completion_tokens`.
///   Cache fields are 0.
#[must_use]
pub fn openai_to_anthropic(resp: &crate::openai::ChatCompletionResponse) -> Message {
    let choice = resp.choices.first();

    let stop_reason = choice
        .and_then(|c| c.finish_reason.as_deref())
        .map(map_finish_reason);

    let content = choice
        .map(|c| build_content_blocks(&c.message))
        .unwrap_or_default();

    let usage = resp
        .usage
        .as_ref()
        .map(|u| AnthropicUsage {
            input_tokens: u.prompt_tokens,
            output_tokens: u.completion_tokens,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens_5m: 0,
            cache_creation_input_tokens_1h: 0,
            thinking_tokens: 0,
        })
        .unwrap_or(AnthropicUsage {
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens_5m: 0,
            cache_creation_input_tokens_1h: 0,
            thinking_tokens: 0,
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

fn map_finish_reason(reason: &str) -> StopReason {
    match reason {
        "stop" => StopReason::EndTurn,
        "length" => StopReason::MaxTokens,
        "tool_calls" | "function_call" => StopReason::ToolUse,
        // Content filters don't have a direct Anthropic equivalent; map
        // to EndTurn as a neutral fallback so the client doesn't error.
        "content_filter" => StopReason::EndTurn,
        other => {
            tracing::warn!(finish_reason = %other, "unknown finish_reason; defaulting to EndTurn");
            StopReason::EndTurn
        }
    }
}

fn build_content_blocks(msg: &crate::openai::ResponseMessage) -> Vec<ResponseContentBlock> {
    let mut blocks = Vec::new();
    if let Some(text) = &msg.content
        && !text.is_empty()
    {
        blocks.push(ResponseContentBlock::Text { text: text.clone() });
    }
    if let Some(calls) = &msg.tool_calls {
        for call in calls {
            // OpenAI gives `arguments` as a JSON string; Anthropic wants
            // a parsed JSON object. If parsing fails, fall back to the
            // raw string under a `_raw` key so the client still sees it.
            let input = serde_json::from_str(&call.function.arguments).unwrap_or_else(
                |_| serde_json::json!({ "_raw_arguments": call.function.arguments }),
            );
            blocks.push(ResponseContentBlock::ToolUse {
                id: call.id.clone(),
                name: call.function.name.clone(),
                input,
            });
        }
    }
    blocks
}

#[cfg(test)]
mod response_tests {
    use super::*;
    use crate::openai::{
        ChatCompletionResponse, Choice, FunctionCall as OaiFunctionCall, ResponseMessage, ToolCall,
        Usage,
    };

    fn fixture_response() -> ChatCompletionResponse {
        ChatCompletionResponse {
            id: "chatcmpl-abc123".into(),
            object: "chat.completion".into(),
            created: 1_700_000_000,
            model: "gpt-4o".into(),
            choices: vec![Choice {
                index: 0,
                message: ResponseMessage {
                    role: "assistant".into(),
                    content: Some("Hello there!".into()),
                    tool_calls: None,
                },
                finish_reason: Some("stop".into()),
            }],
            usage: Some(Usage {
                prompt_tokens: 12,
                completion_tokens: 3,
                total_tokens: 15,
                prompt_tokens_details: None,
                completion_tokens_details: None,
            }),
        }
    }

    #[test]
    fn text_response_maps_to_message() {
        let out = openai_to_anthropic(&fixture_response());
        assert_eq!(out.id, "msg_chatcmpl-abc123");
        assert_eq!(out.r#type, "message");
        assert_eq!(out.role, "assistant");
        assert_eq!(out.model, "gpt-4o");
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
    fn tool_calls_become_tool_use_blocks() {
        let mut resp = fixture_response();
        resp.choices[0].finish_reason = Some("tool_calls".into());
        resp.choices[0].message.content = Some("Calling tool".into());
        resp.choices[0].message.tool_calls = Some(vec![ToolCall {
            id: "call_xyz".into(),
            kind: "function".into(),
            function: OaiFunctionCall {
                name: "get_weather".into(),
                arguments: r#"{"location":"SF"}"#.into(),
            },
        }]);

        let out = openai_to_anthropic(&resp);
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
    fn finish_reason_length_maps_to_max_tokens() {
        let mut resp = fixture_response();
        resp.choices[0].finish_reason = Some("length".into());
        let out = openai_to_anthropic(&resp);
        assert_eq!(out.stop_reason, Some(StopReason::MaxTokens));
    }

    #[test]
    fn content_filter_maps_to_end_turn() {
        let mut resp = fixture_response();
        resp.choices[0].finish_reason = Some("content_filter".into());
        let out = openai_to_anthropic(&resp);
        assert_eq!(out.stop_reason, Some(StopReason::EndTurn));
    }

    #[test]
    fn missing_finish_reason_is_none() {
        let mut resp = fixture_response();
        resp.choices[0].finish_reason = None;
        let out = openai_to_anthropic(&resp);
        assert_eq!(out.stop_reason, None);
    }

    #[test]
    fn tool_only_response_has_null_text_block() {
        let mut resp = fixture_response();
        resp.choices[0].message.content = None;
        resp.choices[0].message.tool_calls = Some(vec![ToolCall {
            id: "call_1".into(),
            kind: "function".into(),
            function: OaiFunctionCall {
                name: "noop".into(),
                arguments: "{}".into(),
            },
        }]);
        let out = openai_to_anthropic(&resp);
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
        resp.choices[0].message.content = None;
        resp.choices[0].message.tool_calls = Some(vec![ToolCall {
            id: "call_bad".into(),
            kind: "function".into(),
            function: OaiFunctionCall {
                name: "bad".into(),
                arguments: "not valid json".into(),
            },
        }]);
        let out = openai_to_anthropic(&resp);
        match &out.content[0] {
            ResponseContentBlock::ToolUse { input, .. } => {
                assert!(input.get("_raw_arguments").is_some());
            }
            _ => panic!("expected tool_use block"),
        }
    }

    #[test]
    fn missing_usage_zeros_tokens() {
        let mut resp = fixture_response();
        resp.usage = None;
        let out = openai_to_anthropic(&resp);
        assert_eq!(out.usage.input_tokens, 0);
        assert_eq!(out.usage.output_tokens, 0);
    }

    #[test]
    fn id_without_msg_prefix_gets_one() {
        let resp = fixture_response();
        let out = openai_to_anthropic(&resp);
        assert!(out.id.starts_with("msg_"));
    }

    #[test]
    fn id_already_prefixed_is_not_double_prefixed() {
        let mut resp = fixture_response();
        resp.id = "msg_already".into();
        let out = openai_to_anthropic(&resp);
        assert_eq!(out.id, "msg_already");
    }
}
