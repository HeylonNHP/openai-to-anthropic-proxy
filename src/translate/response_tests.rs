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
    let out = responses_to_anthropic(
        &fixture_response(),
        &crate::repair::ToolSchemaRegistry::new(),
    );
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
    let out = responses_to_anthropic(&resp, &crate::repair::ToolSchemaRegistry::new());
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
    let out = responses_to_anthropic(&resp, &crate::repair::ToolSchemaRegistry::new());
    // Just the tool_use; no empty text block.
    assert_eq!(out.content.len(), 1);
    assert!(matches!(
        out.content[0],
        ResponseContentBlock::ToolUse { .. }
    ));
}

#[test]
fn malformed_tool_arguments_become_empty_object() {
    // Unparseable upstream arguments used to be shipped as
    // `{"_raw_arguments": ...}` — a synthetic key every strict
    // client schema rejects. Now they become `{}`, so the client
    // reports a specific "missing required parameter" error.
    let mut resp = fixture_response();
    resp.output = vec![OutputItem::FunctionCall {
        id: Some("fc_x".into()),
        status: Some("completed".into()),
        call_id: "call_bad".into(),
        name: "bad".into(),
        arguments: "not valid json".into(),
    }];
    let out = responses_to_anthropic(&resp, &crate::repair::ToolSchemaRegistry::new());
    match &out.content[0] {
        ResponseContentBlock::ToolUse { input, .. } => {
            assert_eq!(input, &serde_json::json!({}));
        }
        _ => panic!("expected tool_use block"),
    }
}

#[test]
fn truncated_tool_arguments_keep_the_parameters_that_were_finished() {
    // The upstream ran out of output tokens mid-call. Anthropic's own
    // API buffers each parameter and drops the one that was still being
    // written, so the client sees the finished ones — not `{}`, which
    // would look like a valid empty call.
    let mut resp = fixture_response();
    resp.status = "incomplete".into();
    resp.incomplete_details = Some(IncompleteDetails {
        reason: "max_output_tokens".into(),
    });
    resp.output = vec![OutputItem::FunctionCall {
        id: Some("fc_x".into()),
        status: Some("incomplete".into()),
        call_id: "call_cut".into(),
        name: "write_file".into(),
        arguments: r#"{"path":"/tmp/x","content":"half a sen"#.into(),
    }];
    let out = responses_to_anthropic(&resp, &crate::repair::ToolSchemaRegistry::new());
    match &out.content[0] {
        ResponseContentBlock::ToolUse { input, .. } => {
            assert_eq!(input, &serde_json::json!({"path": "/tmp/x"}));
        }
        _ => panic!("expected tool_use block"),
    }
    // The client keys its retry on the stop reason, not on the shape
    // of the input.
    assert_eq!(out.stop_reason, Some(StopReason::MaxTokens));
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
    let out = responses_to_anthropic(&resp, &crate::repair::ToolSchemaRegistry::new());
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
    let out = responses_to_anthropic(&resp, &crate::repair::ToolSchemaRegistry::new());
    assert_eq!(out.stop_reason, Some(StopReason::MaxTokens));
}

#[test]
fn incomplete_response_content_filter_maps_to_end_turn() {
    let mut resp = fixture_response();
    resp.status = "incomplete".into();
    resp.incomplete_details = Some(IncompleteDetails {
        reason: "content_filter".into(),
    });
    let out = responses_to_anthropic(&resp, &crate::repair::ToolSchemaRegistry::new());
    assert_eq!(out.stop_reason, Some(StopReason::EndTurn));
}

#[test]
fn failed_response_surfaces_error_message_in_text_block() {
    // The translator's defensive fallback (see the note above
    // `failed_status_with_existing_output_keeps_output`).
    let mut resp = fixture_response();
    resp.status = "failed".into();
    resp.error = Some(ResponsesError {
        message: "upstream rejected".into(),
        kind: "server_error".into(),
        code: None,
        param: None,
    });
    resp.output = vec![];
    let out = responses_to_anthropic(&resp, &crate::repair::ToolSchemaRegistry::new());
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
    let out = responses_to_anthropic(&resp, &crate::repair::ToolSchemaRegistry::new());
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
    let out = responses_to_anthropic(&resp, &crate::repair::ToolSchemaRegistry::new());
    // 100 gross - 20 cached - 7 written: Anthropic's input_tokens excludes both.
    assert_eq!(out.usage.input_tokens, 73);
    assert_eq!(out.usage.output_tokens, 50);
    assert_eq!(out.usage.cache_read_input_tokens, 20);
    assert_eq!(out.usage.cache_creation_input_tokens, 7);
}

#[test]
fn missing_usage_zeros_tokens() {
    let mut resp = fixture_response();
    resp.usage = None;
    let out = responses_to_anthropic(&resp, &crate::repair::ToolSchemaRegistry::new());
    assert_eq!(out.usage.input_tokens, 0);
    assert_eq!(out.usage.output_tokens, 0);
}

#[test]
fn id_without_msg_prefix_gets_one() {
    let resp = fixture_response();
    let out = responses_to_anthropic(&resp, &crate::repair::ToolSchemaRegistry::new());
    assert!(out.id.starts_with("msg_"));
}

#[test]
fn id_already_prefixed_is_not_double_prefixed() {
    let mut resp = fixture_response();
    resp.id = "msg_already".into();
    let out = responses_to_anthropic(&resp, &crate::repair::ToolSchemaRegistry::new());
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
    let out = responses_to_anthropic(&resp, &crate::repair::ToolSchemaRegistry::new());
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
    // This pins only the translator's defensive fallback: the proxy turns a
    // failed reply into an error before it is translated (see the e2e tests
    // `a_failed_200_*`). If a failed reply with output ever does reach here,
    // the output blocks are kept and stop_reason is EndTurn.
    let mut resp = fixture_response();
    resp.status = "failed".into();
    resp.error = Some(ResponsesError {
        message: "post-output error".into(),
        kind: "server_error".into(),
        code: None,
        param: None,
    });
    let out = responses_to_anthropic(&resp, &crate::repair::ToolSchemaRegistry::new());
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
    let (mapped, _) = map_tool_choice(&choice, true);
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
    let (mapped, _) = map_tool_choice(&choice, false);
    match mapped {
        ResponsesToolChoice::Function { kind, name } => {
            assert_eq!(kind, "function");
            assert_eq!(name, "get_weather");
        }
        other => panic!("expected Function, got {other:?}"),
    }
}

#[test]
fn a_client_web_search_function_tool_can_be_forced() {
    // The client declared its OWN function tool called `web_search` (it has a
    // schema), so the upstream gets a function and the choice must name it.
    let choice = ToolChoice::Tool {
        name: "web_search".into(),
        disable_parallel_tool_use: None,
    };
    let (mapped, _) = map_tool_choice(&choice, false);
    match mapped {
        ResponsesToolChoice::Function { kind, name } => {
            assert_eq!(kind, "function");
            assert_eq!(name, "web_search");
        }
        other => panic!("expected Function, got {other:?}"),
    }
}

#[test]
fn a_call_to_a_client_web_search_function_is_a_plain_tool_use() {
    let mut registry = crate::repair::ToolSchemaRegistry::new();
    registry.insert("web_search".into(), json!({"type": "object"}));
    let mut resp = fixture_response();
    resp.output = vec![OutputItem::FunctionCall {
        id: Some("fc_x".into()),
        status: Some("completed".into()),
        call_id: "call_1".into(),
        name: "web_search".into(),
        arguments: "{}".into(),
    }];
    let out = responses_to_anthropic(&resp, &registry);
    assert_eq!(out.content.len(), 1, "no synthetic search blocks");
    assert!(matches!(
        out.content[0],
        ResponseContentBlock::ToolUse { .. }
    ));

    // With no such tool declared, the same call is still read as a search.
    let out = responses_to_anthropic(&resp, &crate::repair::ToolSchemaRegistry::new());
    assert!(matches!(
        out.content[0],
        ResponseContentBlock::ServerToolUse { .. }
    ));
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
    let out = responses_to_anthropic(&resp, &crate::repair::ToolSchemaRegistry::new());

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
            // No url_citation annotations were present, so the result
            // list is empty. Claude Code renders this as "No links
            // found." and the search counter still registers via the
            // server_tool_use block count.
            assert!(block.content.is_empty());
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

#[test]
fn multiple_web_searches_get_unique_ids_and_accurate_usage() {
    let mut resp = fixture_response();
    resp.output = vec![
        OutputItem::WebSearchCall {
            id: Some("ws_01".into()),
            status: Some("completed".into()),
        },
        OutputItem::WebSearchCall {
            id: Some("ws_02".into()),
            status: Some("completed".into()),
        },
    ];

    let out = responses_to_anthropic(&resp, &crate::repair::ToolSchemaRegistry::new());
    assert_eq!(out.content.len(), 4);
    let ids: Vec<&str> = out
        .content
        .iter()
        .filter_map(|block| match block {
            ResponseContentBlock::ServerToolUse { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(ids, vec!["stoolu_web_search_01", "stoolu_web_search_02"]);
    let result_ids: Vec<&str> = out
        .content
        .iter()
        .filter_map(|block| match block {
            ResponseContentBlock::WebSearchToolResult { block } => Some(block.tool_use_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(result_ids, ids);
    assert_eq!(
        out.usage
            .server_tool_use
            .as_ref()
            .expect("server search usage")
            .web_search_requests,
        2
    );
}

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
    let out = responses_to_anthropic(&resp, &crate::repair::ToolSchemaRegistry::new());

    // server_tool_use + web_search_tool_result (injected) +
    // tool_use (the WebSearch function call) + text = 4 blocks.
    assert_eq!(out.content.len(), 4, "expected 4 content blocks");
    assert!(matches!(
        &out.content[0],
        ResponseContentBlock::ServerToolUse { .. }
    ));
    assert!(matches!(
        &out.content[1],
        ResponseContentBlock::WebSearchToolResult { .. }
    ));
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
    let out = responses_to_anthropic(&resp, &crate::repair::ToolSchemaRegistry::new());
    // server_tool_use + web_search_tool_result + tool_use = 3 blocks.
    assert_eq!(out.content.len(), 3);
    assert!(matches!(
        &out.content[0],
        ResponseContentBlock::ServerToolUse { .. }
    ));
    assert!(matches!(
        &out.content[1],
        ResponseContentBlock::WebSearchToolResult { .. }
    ));
    assert!(out.usage.server_tool_use.is_some());
}

// ─── untyped object schemas inside `anyOf` / `oneOf` ──────────
//
// Live regression from a claude-code → atlassian MCP tool:
//   "Invalid schema for function 'mcp__atlassian__createJiraIssue':
//    In context=('properties', 'description', 'anyOf', '1'),
//    'additionalProperties' is required to be supplied and to be false."
//
// Root cause: that branch of `anyOf` is an object schema with
// `properties` but no explicit `"type": "object"` and no
// `additionalProperties`. The Responses strict validator infers
// "object" from the `properties` key, then requires
// `additionalProperties: false`. `is_object_schema` previously
// only recognised schemas with `"type": "object"`, so the
// injection step silently skipped the branch and the upstream
// returned 400. The fix is to also treat any map carrying
// `properties` as an object schema.

#[test]
fn sanitize_tool_schema_reconciles_untyped_object_in_any_of_branch() {
    let schema = json!({
        "type": "object",
        "properties": {
            "description": {
                "anyOf": [
                    {"type": "string"},
                    {
                        // No "type": "object"; only `properties`.
                        "properties": {
                            "type": {"type": "string"},
                            "text": {"type": "string"},
                        },
                    },
                ],
            },
        },
    });
    let out = sanitize_tool_schema(schema, None);
    let branch = &out["properties"]["description"]["anyOf"][1];
    assert_eq!(branch["type"], "object");
    assert_eq!(
        branch.get("additionalProperties"),
        Some(&Value::Bool(false)),
        "expected additionalProperties: false injected into untyped anyOf branch, got {:?}",
        branch
    );
    // `required` should also have been populated from
    // `properties` keys, so this is also a regression guard for
    // the prior required-reconciler running here too.
    let required = branch.get("required").and_then(|v| v.as_array());
    let names: Vec<&str> = required
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    for expected in ["type", "text"] {
        assert!(
            names.contains(&expected),
            "required {:?} should contain {:?}",
            names,
            expected
        );
    }
}

/// Live regression from `mcp__atlassian__createJiraIssue`:
/// `properties.description.anyOf[1]` is a description-only object
/// schema with no `properties`, no `type`, and no
/// `additionalProperties`. The OpenAI Responses strict validator
/// still treats the branch as an object schema and demands
/// `additionalProperties: false`. Combinator branches are
/// force-coerced to objects.
#[test]
fn sanitize_tool_schema_reconciles_description_only_any_of_branch() {
    let schema = json!({
        "type": "object",
        "properties": {
            "description": {
                "anyOf": [
                    {"type": "string"},
                    {
                        // No `type`, no `properties`, no
                        // `additionalProperties`. OpenAI's strict
                        // validator treats this as an object.
                        "description": "An ADF document."
                    },
                ],
            },
        },
    });
    let out = sanitize_tool_schema(schema, None);
    let branch = &out["properties"]["description"]["anyOf"][1];
    assert_eq!(branch["type"], "object");
    assert_eq!(
        branch.get("additionalProperties"),
        Some(&Value::Bool(false)),
        "expected additionalProperties: false on description-only anyOf branch, got {:?}",
        branch
    );
    assert!(
        branch.get("properties").is_some(),
        "expected empty `properties` map injected on description-only anyOf branch, got {:?}",
        branch
    );
}

/// Branches the strict validator treats as objects get
/// `additionalProperties: false` (and an empty `properties` map).
/// `oneOf` reaches this walk by way of rule 9 (`oneOf` -> `anyOf`),
/// so both fixtures below exercise the same code path.
#[test]
fn sanitize_tool_schema_force_objects_combinator_branches() {
    let schema = json!({
        "type": "object",
        "properties": {
            "x": {
                "oneOf": [
                    {"description": "a oneOf branch"},
                ],
            },
            "y": {
                "anyOf": [
                    {"description": "an anyOf branch"},
                ],
            },
        },
    });
    let out = sanitize_tool_schema(schema, None);
    for path in ["properties/x/anyOf/0", "properties/y/anyOf/0"] {
        let node = out
            .pointer(&format!("/{}", path))
            .unwrap_or_else(|| panic!("missing path /{}", path));
        assert_eq!(node["type"], "object", "path {}", path);
        assert_eq!(
            node.get("additionalProperties"),
            Some(&Value::Bool(false)),
            "path {}: expected additionalProperties: false",
            path
        );
        assert!(
            node.get("properties").is_some(),
            "path {}: expected properties map",
            path
        );
    }
}

/// A schema that *only* declares `properties` (no `type` and no
/// `additionalProperties`) is treated as an object and gets
/// `additionalProperties: false` injected. This is what
/// `is_object_schema`'s new branch enables.
#[test]
fn sanitize_tool_schema_untyped_object_receives_additional_properties_false() {
    let schema = json!({
        "properties": {
            "x": {"type": "string"},
        },
    });
    let out = sanitize_tool_schema(schema, None);
    assert_eq!(out["type"], "object");
    assert_eq!(out["additionalProperties"], Value::Bool(false));
    let required = out["required"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
        .unwrap_or_default();
    assert!(
        required.contains(&"x"),
        "required {:?} should contain x",
        required
    );
}

/// Live regression: the Atlassian MCP `createJiraIssue` tool's
/// `properties.description.anyOf[1]` branch arrived with
/// `additionalProperties: true` on an object-shaped schema. The
/// OpenAI strict validator rejected that and demanded the literal
/// `additionalProperties: false`. `sanitize_tool_schema` must
/// coerce `true` to `false` on object-like schemas.
///
/// The branch shape is replayed from the captured dump file at
/// `tests/fixtures/atlassian_create_jira_issue_description_any_of_1.json`
/// rather than re-synthesized here, so the test tracks the actual
/// payload that triggered the live 400.
#[test]
fn sanitize_tool_schema_coerces_additional_properties_true_to_false_on_object() {
    // 1. Load the captured branch from the test fixture.
    let fixture_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("atlassian_create_jira_issue_description_any_of_1.json");
    let captured: Value =
        serde_json::from_str(&std::fs::read_to_string(&fixture_path).expect("read fixture"))
            .expect("parse fixture");

    // Sanity-check the fixture matches the originally captured
    // shape (additionalProperties: true + type: object). If a
    // future Atlassian MCP release changes the schema, this guard
    // makes it obvious the fixture needs updating, not the
    // sanitizer.
    assert_eq!(captured["type"], "object");
    assert_eq!(captured["additionalProperties"], Value::Bool(true));

    // 2. Build the surrounding `properties.description` wrapper
    //    that contained the branch in the live request, so the
    //    sanitizer walks the same recursion path it walks for
    //    real traffic.
    let schema = json!({
        "type": "object",
        "properties": {
            "description": {
                "anyOf": [
                    {"type": "string", "maxLength": 32000},
                    captured,
                ],
            },
        },
    });

    // 3. Sanitize and assert the offender branch now has
    //    `additionalProperties: false`.
    let out = sanitize_tool_schema(schema, None);
    let branch = &out["properties"]["description"]["anyOf"][1];
    assert_eq!(
        branch["additionalProperties"],
        Value::Bool(false),
        "expected additionalProperties: false on object branch, got {}",
        branch["additionalProperties"]
    );
}

#[test]
fn a_truncated_call_from_a_completed_reply_still_reports_max_tokens() {
    // An upstream that claims the turn is complete but ships truncated
    // arguments is lying: the call is incomplete whatever the status
    // says. `max_tokens` is the honest stop reason, and it is the
    // signal the client keys on to retry with a bigger budget — a
    // `tool_use` stop reason would tell it to run the call.
    let mut resp = fixture_response();
    resp.status = "completed".into();
    resp.output = vec![OutputItem::FunctionCall {
        id: Some("fc_x".into()),
        status: Some("completed".into()),
        call_id: "call_cut".into(),
        name: "write_file".into(),
        arguments: r#"{"path":"/tmp/x","content":"half a sen"#.into(),
    }];
    let out = responses_to_anthropic(&resp, &crate::repair::ToolSchemaRegistry::new());
    match &out.content[0] {
        ResponseContentBlock::ToolUse { input, .. } => {
            assert_eq!(input, &serde_json::json!({"path": "/tmp/x"}));
        }
        _ => panic!("expected tool_use block"),
    }
    assert_eq!(out.stop_reason, Some(StopReason::MaxTokens));
}
