use super::*;
use crate::anthropic::Tool;

fn request(tools: serde_json::Value) -> CreateMessageRequest {
    serde_json::from_value(json!({
        "model": "m", "max_tokens": 10,
        "messages": [{"role": "user", "content": "hi"}],
        "tools": tools,
    }))
    .unwrap()
}

fn server_decl() -> serde_json::Value {
    json!({"type": "web_search_20250305", "name": "web_search", "max_uses": 8})
}

fn client_tool() -> serde_json::Value {
    json!({"name": "web_search", "description": "d",
           "input_schema": {"type": "object", "properties": {}}})
}

fn cfg() -> WebSearchConfig {
    WebSearchConfig {
        enabled: true,
        ..WebSearchConfig::default()
    }
}

fn outbound_with_native_search() -> ResponsesRequest {
    let (outbound, _) = crate::translate::anthropic_to_responses_with(
        &request(json!([server_decl()])),
        None,
        &Default::default(),
        &Default::default(),
        Default::default(),
        Default::default(),
    )
    .expect("translate");
    outbound
}

#[test]
fn detects_only_the_server_side_declaration() {
    assert!(has_server_side_web_search(&request(json!([server_decl()]))));
    assert!(!has_server_side_web_search(&request(json!(
        [client_tool()]
    ))));
    let other = json!([{"name": "Bash", "input_schema": {"type": "object"}}]);
    assert!(!has_server_side_web_search(&request(other)));
}

#[test]
fn disabled_never_activates() {
    let mut c = cfg();
    c.enabled = false;
    assert!(!active(&c, &request(json!([server_decl()]))));
    let mut outbound = outbound_with_native_search();
    let before = outbound.stream;
    assert!(!prepare(
        &request(json!([server_decl()])),
        &mut outbound,
        &c
    ));
    assert_eq!(outbound.stream, before);
    assert!(
        outbound
            .tools
            .unwrap()
            .iter()
            .any(|t| matches!(t, ToolDefinition::WebSearch(_))),
        "the upstream's built-in search must be untouched when disabled"
    );
}

#[test]
fn prepare_swaps_the_builtin_search_for_the_function_tool_and_buffers() {
    let req = request(json!([server_decl()]));
    let mut outbound = outbound_with_native_search();
    assert!(
        outbound
            .tools
            .as_ref()
            .unwrap()
            .iter()
            .any(|t| matches!(t, ToolDefinition::WebSearch(_)))
    );

    assert!(prepare(&req, &mut outbound, &cfg()));

    let tools = outbound.tools.unwrap();
    assert!(
        !tools
            .iter()
            .any(|t| matches!(t, ToolDefinition::WebSearch(_)))
    );
    let functions: Vec<_> = tools
        .iter()
        .filter_map(|t| match t {
            ToolDefinition::Function(f) => Some(f),
            ToolDefinition::WebSearch(_) => None,
        })
        .collect();
    assert_eq!(functions.len(), 1);
    assert_eq!(functions[0].name, EXECUTOR_TOOL_NAME);
    assert_eq!(functions[0].parameters["required"][0], "queries");
    assert!(!outbound.stream);
}

#[test]
fn prepare_is_idempotent() {
    let req = request(json!([server_decl()]));
    let mut outbound = outbound_with_native_search();
    prepare(&req, &mut outbound, &cfg());
    prepare(&req, &mut outbound, &cfg());
    assert_eq!(outbound.tools.unwrap().len(), 1);
}

#[test]
fn the_tool_advertises_the_configured_query_cap() {
    let mut c = cfg();
    c.max_queries = 4;
    let tool = executor_tool(&c);
    assert!(tool.description.unwrap().contains("at most 4"));
    assert!(
        tool.parameters["properties"]["queries"]["description"]
            .as_str()
            .unwrap()
            .contains("1 to 4")
    );
}

#[test]
fn tool_type_is_parseable() {
    // Guards the fixture helpers themselves.
    let t: Tool = serde_json::from_value(server_decl()).unwrap();
    assert!(t.input_schema.is_none());
}
