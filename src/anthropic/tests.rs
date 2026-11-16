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

// ── Advisor echo blocks (§11.5) ────────────────────────────────────────
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
