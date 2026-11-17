use super::*;

use crate::anthropic::{CreateMessageRequest, Tool};

/// Build a request that declares the given tools.
///
/// The tools are serialized to JSON first: `Tool` is `Deserialize`-only,
/// so it cannot go straight into a `json!` macro.
fn request_with_tools(tools: Vec<Tool>) -> CreateMessageRequest {
    let tools: Vec<serde_json::Value> = tools
        .iter()
        .map(|t| {
            serde_json::json!({
                "name": t.name,
                "description": t.description,
                "input_schema": t.input_schema,
            })
        })
        .collect();
    serde_json::from_value(serde_json::json!({
        "model": "claude-sonnet-5",
        "max_tokens": 1024,
        "messages": [{"role": "user", "content": "hi"}],
        "tools": tools,
    }))
    .expect("parse CreateMessageRequest")
}

/// The Anthropic server-side `web_fetch` declaration: a name and no
/// `input_schema`.
fn server_side_declaration(name: &str) -> Tool {
    serde_json::from_value(serde_json::json!({
        "name": name,
        "type": "web_fetch_20250910",
    }))
    .expect("parse Tool")
}

/// A client function tool: a name and an `input_schema`.
fn client_function_tool(name: &str) -> Tool {
    serde_json::from_value(serde_json::json!({
        "name": name,
        "description": "a client tool",
        "input_schema": {
            "type": "object",
            "properties": {"q": {"type": "string"}},
        },
    }))
    .expect("parse Tool")
}

fn config() -> WebFetchConfig {
    WebFetchConfig {
        enabled: true,
        ..WebFetchConfig::default()
    }
}

#[test]
fn detects_the_server_side_declaration() {
    let req = request_with_tools(vec![server_side_declaration("web_fetch")]);
    assert!(has_server_side_web_fetch(&req));
    assert!(active(&config(), &req));
}

#[test]
fn a_client_function_tool_named_web_fetch_does_not_activate() {
    // A client tool with the same name is an ORDINARY tool: the proxy
    // must forward it upstream, not hijack it.
    let req = request_with_tools(vec![client_function_tool("web_fetch")]);
    assert!(!has_server_side_web_fetch(&req));
    assert!(!active(&config(), &req));
}

#[test]
fn disabled_never_activates() {
    let req = request_with_tools(vec![server_side_declaration("web_fetch")]);
    let mut cfg = config();
    cfg.enabled = false;
    assert!(!active(&cfg, &req));
    // The declaration is still DETECTED — only the activation is gated.
    assert!(has_server_side_web_fetch(&req));
}

#[test]
fn a_request_with_no_tools_never_activates() {
    let req: CreateMessageRequest = serde_json::from_value(serde_json::json!({
        "model": "claude-sonnet-5",
        "max_tokens": 1024,
        "messages": [{"role": "user", "content": "hi"}],
    }))
    .expect("parse CreateMessageRequest");
    assert!(!has_server_side_web_fetch(&req));
    assert!(!active(&config(), &req));
}

#[test]
fn another_server_tool_does_not_activate() {
    let req = request_with_tools(vec![server_side_declaration("web_search")]);
    assert!(!has_server_side_web_fetch(&req));
}

#[test]
fn prepare_synthesizes_the_function_tool() {
    let req = request_with_tools(vec![server_side_declaration("web_fetch")]);
    let mut outbound = ResponsesRequest {
        model: "gpt-5.4-mini".to_owned(),
        input: crate::responses::Input::Text("hi".to_owned()),
        instructions: None,
        tools: None,
        tool_choice: None,
        reasoning: None,
        text: None,
        temperature: None,
        top_p: None,
        max_output_tokens: None,
        parallel_tool_calls: None,
        store: None,
        previous_response_id: None,
        user: None,
        stream: true,
        metadata: None,
        prompt_cache_key: None,
        prompt_cache_options: None,
    };

    assert!(prepare(&req, &mut outbound, &config()));

    let tools = outbound.tools.as_ref().expect("tools");
    let ToolDefinition::Function(f) = &tools[0] else {
        panic!("expected a function tool, got {:?}", tools[0]);
    };
    assert_eq!(f.name, "web_fetch");
    assert_eq!(f.parameters["properties"]["url"]["type"], "string");
    assert_eq!(f.parameters["required"][0], "url");
}

#[test]
fn prepare_forces_buffering() {
    let req = request_with_tools(vec![server_side_declaration("web_fetch")]);
    let mut outbound = ResponsesRequest {
        model: "gpt-5.4-mini".to_owned(),
        input: crate::responses::Input::Text("hi".to_owned()),
        instructions: None,
        tools: None,
        tool_choice: None,
        reasoning: None,
        text: None,
        temperature: None,
        top_p: None,
        max_output_tokens: None,
        parallel_tool_calls: None,
        store: None,
        previous_response_id: None,
        user: None,
        stream: true,
        metadata: None,
        prompt_cache_key: None,
        prompt_cache_options: None,
    };

    prepare(&req, &mut outbound, &config());
    assert!(!outbound.stream, "the fetch chain must be buffered");
}

#[test]
fn prepare_is_a_no_op_when_disabled() {
    let req = request_with_tools(vec![server_side_declaration("web_fetch")]);
    let mut outbound = ResponsesRequest {
        model: "gpt-5.4-mini".to_owned(),
        input: crate::responses::Input::Text("hi".to_owned()),
        instructions: None,
        tools: None,
        tool_choice: None,
        reasoning: None,
        text: None,
        temperature: None,
        top_p: None,
        max_output_tokens: None,
        parallel_tool_calls: None,
        store: None,
        previous_response_id: None,
        user: None,
        stream: true,
        metadata: None,
        prompt_cache_key: None,
        prompt_cache_options: None,
    };

    let mut cfg = config();
    cfg.enabled = false;
    assert!(!prepare(&req, &mut outbound, &cfg));
    assert!(outbound.tools.is_none(), "no tool may be synthesized");
    assert!(outbound.stream, "streaming must not be disabled");
}

#[test]
fn prepare_never_duplicates_the_tool() {
    let req = request_with_tools(vec![server_side_declaration("web_fetch")]);
    let mut outbound = ResponsesRequest {
        model: "gpt-5.4-mini".to_owned(),
        input: crate::responses::Input::Text("hi".to_owned()),
        instructions: None,
        tools: Some(vec![ToolDefinition::Function(ResponsesTool {
            kind: "function".to_owned(),
            name: "web_fetch".to_owned(),
            description: None,
            strict: true,
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        })]),
        tool_choice: None,
        reasoning: None,
        text: None,
        temperature: None,
        top_p: None,
        max_output_tokens: None,
        parallel_tool_calls: None,
        store: None,
        previous_response_id: None,
        user: None,
        stream: true,
        metadata: None,
        prompt_cache_key: None,
        prompt_cache_options: None,
    };

    prepare(&req, &mut outbound, &config());
    assert_eq!(outbound.tools.as_ref().expect("tools").len(), 1);
}
