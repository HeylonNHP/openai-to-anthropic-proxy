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

// ── Generic wire extension points ─────────────────────────────

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
    let value = serde_json::to_value(plain_usage()).unwrap();
    assert!(!value.as_object().unwrap().contains_key("iterations"));
}

/// Claude Code's usage schema marks these nullable but not optional, so they
/// are sent even when there is nothing to report.
#[test]
fn usage_always_carries_the_keys_the_client_requires() {
    let value = serde_json::to_value(plain_usage()).unwrap();
    assert_eq!(
        value,
        serde_json::json!({
            "input_tokens": 7,
            "output_tokens": 3,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 0,
            "cache_creation": null,
            "service_tier": "standard",
        })
    );
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

// ── Advisor echo blocks ─────────────────────────────────────────
//
// Claude Code echoes `server_tool_use` + `advisor_tool_result` verbatim
// on the next turn. They used to land in `Unknown` and be dropped, so the
// upstream never saw the prior advice. These tests pin the wire types.

#[test]
fn parses_advisor_echo_pair_with_unknown_fields() {
    let blocks = parse_blocks(
        r#"{
                "model": "claude-sonnet-5",
                "max_tokens": 100,
                "messages": [{"role": "assistant", "content": [
                    {"type": "text", "text": "Let me consult the advisor."},
                    {"type": "server_tool_use", "id": "srvtoolu_abc",
                     "name": "advisor", "input": {},
                     "cache_control": {"type": "ephemeral"},
                     "caller": {"type": "direct"}},
                    {"type": "advisor_tool_result", "tool_use_id": "srvtoolu_abc",
                     "content": {"type": "advisor_result",
                                 "text": "Use a channel-based pattern.",
                                 "stop_reason": "end_turn"},
                     "cache_control": {"type": "ephemeral"}}
                ]}]
            }"#,
    );
    assert_eq!(blocks.len(), 3);
    match &blocks[1] {
        ContentBlockParam::ServerToolUse(u) => {
            assert_eq!(u.id, "srvtoolu_abc");
            assert_eq!(u.name, "advisor");
            assert_eq!(u.input, serde_json::json!({}));
            assert_eq!(u.extra["cache_control"]["type"], "ephemeral");
            assert_eq!(u.extra["caller"]["type"], "direct");
            // Round-trip: named fields + flattened extras reproduce the
            // original object (the tag is added by the enum).
            let mut round = serde_json::Map::from_iter([
                ("id".into(), u.id.clone().into()),
                ("name".into(), u.name.clone().into()),
                ("input".into(), u.input.clone()),
            ]);
            round.extend(u.extra.clone());
            round.insert("type".into(), "server_tool_use".into());
            assert_eq!(
                Value::Object(round),
                serde_json::json!({
                    "type": "server_tool_use", "id": "srvtoolu_abc",
                    "name": "advisor", "input": {},
                    "cache_control": {"type": "ephemeral"},
                    "caller": {"type": "direct"}
                })
            );
        }
        other => panic!("expected ServerToolUse, got {other:?}"),
    }
    match &blocks[2] {
        ContentBlockParam::AdvisorToolResult(r) => {
            assert_eq!(r.tool_use_id, "srvtoolu_abc");
            assert_eq!(r.content["type"], "advisor_result");
            assert_eq!(r.content["text"], "Use a channel-based pattern.");
            assert_eq!(r.content["stop_reason"], "end_turn");
            assert_eq!(r.extra["cache_control"]["type"], "ephemeral");
        }
        other => panic!("expected AdvisorToolResult, got {other:?}"),
    }
}

#[test]
fn advisor_echo_blocks_accept_all_three_result_variants() {
    // `content` is opaque: the redacted and error variants must parse
    // without failing the request, even though the translator will not
    // inject any advice text for them.
    let blocks = parse_blocks(
        r#"{
                "model": "claude-sonnet-5",
                "max_tokens": 100,
                "messages": [{"role": "assistant", "content": [
                    {"type": "advisor_tool_result", "tool_use_id": "srvtoolu_1",
                     "content": {"type": "advisor_redacted_result",
                                 "encrypted_content": "BLOB"}},
                    {"type": "advisor_tool_result", "tool_use_id": "srvtoolu_2",
                     "content": {"type": "advisor_tool_result_error",
                                 "error_code": "unavailable"}},
                    {"type": "advisor_tool_result", "tool_use_id": "srvtoolu_3",
                     "content": {"type": "future_result", "future": 1}}
                ]}]
            }"#,
    );
    assert_eq!(blocks.len(), 3);
    for (i, expected) in [
        "advisor_redacted_result",
        "advisor_tool_result_error",
        "future_result",
    ]
    .iter()
    .enumerate()
    {
        match &blocks[i] {
            ContentBlockParam::AdvisorToolResult(r) => {
                assert_eq!(r.content["type"], *expected);
            }
            other => panic!("expected AdvisorToolResult, got {other:?}"),
        }
    }
}

#[test]
fn partial_or_wrong_typed_advisor_echo_blocks_never_fail_the_request() {
    // A matched tag with missing/wrong-typed fields is a hard serde error
    // unless every field defaults; `#[serde(other)]` does not help here.
    let blocks = parse_blocks(
        r#"{
                "model": "claude-sonnet-5",
                "max_tokens": 100,
                "messages": [{"role": "assistant", "content": [
                    {"type": "server_tool_use"},
                    {"type": "server_tool_use", "id": 123, "name": {},
                     "input": null},
                    {"type": "advisor_tool_result"},
                    {"type": "advisor_tool_result", "tool_use_id": 5,
                     "content": "garbage", "future": [1, 2]}
                ]}]
            }"#,
    );
    assert_eq!(blocks.len(), 4);
    match &blocks[0] {
        ContentBlockParam::ServerToolUse(u) => {
            assert!(u.id.is_empty());
            assert!(u.name.is_empty());
            assert!(u.input.is_null());
            assert!(u.extra.is_empty());
        }
        other => panic!("expected ServerToolUse, got {other:?}"),
    }
    match &blocks[1] {
        ContentBlockParam::ServerToolUse(u) => {
            assert_eq!(u.id, "123");
            assert_eq!(u.name, "{}");
            assert!(u.input.is_null());
        }
        other => panic!("expected ServerToolUse, got {other:?}"),
    }
    match &blocks[2] {
        ContentBlockParam::AdvisorToolResult(r) => {
            assert!(r.tool_use_id.is_empty());
            assert!(r.content.is_null());
        }
        other => panic!("expected AdvisorToolResult, got {other:?}"),
    }
    match &blocks[3] {
        ContentBlockParam::AdvisorToolResult(r) => {
            assert_eq!(r.tool_use_id, "5");
            assert_eq!(r.content, "garbage");
            assert_eq!(r.extra["future"], serde_json::json!([1, 2]));
        }
        other => panic!("expected AdvisorToolResult, got {other:?}"),
    }
}

#[test]
fn unknown_block_type_still_maps_to_unknown() {
    // Guards `#[serde(other)] Unknown` remaining the LAST variant. Use a
    // genuinely unrecognised tag: `server_tool_use` is now explicit.
    let blocks = parse_blocks(
        r#"{
                "model": "claude-sonnet-5",
                "max_tokens": 100,
                "messages": [{"role": "assistant", "content": [
                    {"type": "future_server_block", "id": "srvtool_1"}
                ]}]
            }"#,
    );
    assert_eq!(blocks.len(), 1);
    assert!(matches!(blocks[0], ContentBlockParam::Unknown));
}

fn tool(json: &str) -> Tool {
    serde_json::from_str(json).expect("parse Tool")
}

#[test]
fn a_search_declaration_keeps_its_type_and_options() {
    let t = tool(
        r#"{"type":"web_search_20250305","name":"web_search","max_uses":5,
            "allowed_domains":["rust-lang.org"],
            "user_location":{"type":"approximate","country":"GB"}}"#,
    );
    assert_eq!(t.kind.as_deref(), Some("web_search_20250305"));
    assert_eq!(t.options.max_uses, Some(5));
    assert_eq!(
        t.options.allowed_domains.as_deref(),
        Some(&["rust-lang.org".to_owned()][..])
    );
    assert!(t.options.blocked_domains.is_none());
    assert_eq!(t.options.user_location.unwrap()["country"], "GB");
}

#[test]
fn a_fetch_declaration_keeps_its_options() {
    let t = tool(
        r#"{"type":"web_fetch_20250910","name":"web_fetch","max_content_tokens":1000,
            "blocked_domains":["x.example"],"citations":{"enabled":true}}"#,
    );
    assert_eq!(t.options.max_content_tokens, Some(1000));
    assert_eq!(t.options.blocked_domains.unwrap()[0], "x.example");
    assert_eq!(t.options.citations.unwrap()["enabled"], true);
}

#[test]
fn a_client_function_tool_has_no_options() {
    let t = tool(r#"{"name":"Bash","description":"d","input_schema":{"type":"object"}}"#);
    assert!(t.kind.is_none());
    assert!(t.options.max_uses.is_none() && t.options.allowed_domains.is_none());
}

#[test]
fn unknown_declaration_fields_are_ignored() {
    // Claude Code adds fields such as `cache_control`; they must not break parsing.
    let t = tool(
        r#"{"type":"custom","name":"Bash","input_schema":{},"cache_control":{"type":"ephemeral"}}"#,
    );
    assert_eq!(t.kind.as_deref(), Some("custom"));
}

#[test]
fn a_schemaless_tool_is_a_server_tool_by_name() {
    let t = tool(r#"{"name":"web_fetch"}"#);
    assert!(t.is_server_tool("web_fetch", "web_fetch_"));
    assert!(!t.is_server_tool("web_search", "web_search_"));
}

#[test]
fn a_versioned_type_marks_a_server_tool_even_with_a_schema() {
    let t = tool(
        r#"{"type":"web_fetch_20260309","name":"web_fetch","input_schema":{"type":"object"}}"#,
    );
    assert!(t.is_server_tool("web_fetch", "web_fetch_"));
}

#[test]
fn a_function_tool_with_a_server_tools_name_is_not_a_server_tool() {
    for json in [
        r#"{"name":"web_fetch","input_schema":{"type":"object"}}"#,
        r#"{"type":"custom","name":"web_fetch","input_schema":{"type":"object"}}"#,
    ] {
        assert!(
            !tool(json).is_server_tool("web_fetch", "web_fetch_"),
            "{json}"
        );
    }
}

#[test]
fn both_domain_lists_are_rejected() {
    for name_type in [
        ("web_search", "web_search_20250305"),
        ("web_fetch", "web_fetch_20250910"),
    ] {
        let t = tool(&format!(
            r#"{{"type":"{}","name":"{}","allowed_domains":["a.example"],"blocked_domains":["b.example"]}}"#,
            name_type.1, name_type.0
        ));
        let err = validate_server_tool_options(&[t]).unwrap_err();
        assert!(
            err.contains(name_type.0) && err.contains("allowed_domains"),
            "{err}"
        );
    }
}

#[test]
fn one_domain_list_or_none_is_accepted() {
    let tools = [
        tool(
            r#"{"type":"web_search_20250305","name":"web_search","allowed_domains":["a.example"]}"#,
        ),
        tool(r#"{"type":"web_fetch_20250910","name":"web_fetch","blocked_domains":["b.example"]}"#),
        tool(r#"{"type":"web_search_20250305","name":"web_search"}"#),
    ];
    assert!(validate_server_tool_options(&tools).is_ok());
}

#[test]
fn a_function_tool_with_both_fields_is_not_validated_as_a_server_tool() {
    let t = tool(
        r#"{"name":"web_fetch","input_schema":{"type":"object"},
            "allowed_domains":["a"],"blocked_domains":["b"]}"#,
    );
    assert!(validate_server_tool_options(&[t]).is_ok());
}

#[test]
fn server_tool_usage_carries_both_counts() {
    let only_search = ServerToolUseUsage {
        web_search_requests: 2,
        ..ServerToolUseUsage::default()
    };
    assert_eq!(
        serde_json::to_value(only_search).unwrap(),
        serde_json::json!({"web_search_requests": 2, "web_fetch_requests": 0})
    );
    let both = ServerToolUseUsage {
        web_search_requests: 1,
        web_fetch_requests: 3,
    };
    assert_eq!(
        serde_json::to_value(both).unwrap(),
        serde_json::json!({"web_search_requests": 1, "web_fetch_requests": 3})
    );
}

// ─── GET /v1/models serialization ────────────────────────────────────────

/// A row the operator configured nothing for emits only its four base
/// fields — Anthropic's own responder omits unconfigured metadata rather
/// than sending `null`.
#[test]
fn a_model_row_omits_metadata_it_did_not_configure() {
    let row = AnthropicModelInfo {
        r#type: "model",
        id: "claude-sonnet-5".to_owned(),
        display_name: "claude-sonnet-5".to_owned(),
        created_at: "1970-01-01T00:00:00Z".to_owned(),
        description: None,
        max_input_tokens: None,
        max_tokens: None,
        capabilities: None,
    };
    let value = serde_json::to_value(&row).unwrap();
    let obj = value.as_object().unwrap();
    // serde_json sorts keys alphabetically; the point is that these four
    // are the whole row and no metadata key is present.
    let keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    assert_eq!(keys, ["created_at", "display_name", "id", "type"]);
}

/// Configured metadata rides along with Anthropic's field names and
/// JSON types.
#[test]
fn a_model_row_carries_configured_metadata_with_its_types() {
    let row = AnthropicModelInfo {
        r#type: "model",
        id: "claude-sonnet-5".to_owned(),
        display_name: "claude-sonnet-5".to_owned(),
        created_at: "1970-01-01T00:00:00Z".to_owned(),
        description: Some("GPT-5.4-mini behind the alias".to_owned()),
        max_input_tokens: Some(1_000_000),
        max_tokens: Some(64_000),
        capabilities: Some(serde_json::json!({"thinking": {"supported": true}})),
    };
    let value = serde_json::to_value(&row).unwrap();
    assert_eq!(value["max_input_tokens"], 1_000_000);
    assert_eq!(value["max_tokens"], 64_000);
    assert_eq!(value["description"], "GPT-5.4-mini behind the alias");
    assert_eq!(value["capabilities"]["thinking"]["supported"], true);
}

/// An empty page carries explicit `null` boundaries, as Anthropic's
/// responder emits — present keys, null values, not omitted keys.
#[test]
fn an_empty_models_page_reports_null_boundaries() {
    let list = AnthropicModelsList {
        data: Vec::new(),
        has_more: false,
        first_id: None,
        last_id: None,
    };
    let value = serde_json::to_value(&list).unwrap();
    let obj = value.as_object().unwrap();
    assert!(obj.contains_key("first_id"));
    assert!(obj.contains_key("last_id"));
    assert_eq!(value["first_id"], serde_json::Value::Null);
    assert_eq!(value["last_id"], serde_json::Value::Null);
    assert_eq!(value["has_more"], false);
}

/// Anthropic nests the thinking-token count under `output_tokens_details`.
/// A static reading of Claude Code 2.1.332 shows it reads only that nested
/// form, so the old flat `thinking_tokens` was dropped by the client and its
/// thinking accounting always came to zero.
#[test]
fn thinking_tokens_are_sent_nested_not_flat() {
    let usage = Usage {
        thinking_tokens: 14,
        ..plain_usage()
    };
    let value = serde_json::to_value(usage).unwrap();
    assert_eq!(
        value["output_tokens_details"],
        serde_json::json!({"thinking_tokens": 14})
    );
    assert!(
        !value.as_object().unwrap().contains_key("thinking_tokens"),
        "the flat alias is gone: {value}"
    );
}

#[test]
fn zero_thinking_tokens_leave_the_usage_unchanged() {
    let value = serde_json::to_value(plain_usage()).unwrap();
    assert!(
        !value
            .as_object()
            .unwrap()
            .contains_key("output_tokens_details")
    );
}
