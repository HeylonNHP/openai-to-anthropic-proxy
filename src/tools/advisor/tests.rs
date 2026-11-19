use super::*;
use crate::anthropic::Tool;
use crate::config::AdvisorConfig;
use crate::responses::Input;
use serde_json::{Value, json};

fn req_with_advisor_decl() -> CreateMessageRequest {
    serde_json::from_value(json!({
        "model": "gpt-4o",
        "max_tokens": 32,
        "stream": true,
        "messages": [{"role": "user", "content": "hi"}],
        "tools": [{"type": "advisor_20260301", "name": "advisor", "model": "beam"}]
    }))
    .unwrap()
}

fn empty_outbound() -> ResponsesRequest {
    ResponsesRequest {
        model: "gpt-4o".into(),
        input: Input::Items(Vec::new()),
        instructions: None,
        tools: None,
        tool_choice: None,
        reasoning: None,
        text: None,
        temperature: None,
        top_p: None,
        max_output_tokens: Some(32),
        parallel_tool_calls: None,
        store: None,
        previous_response_id: None,
        user: None,
        stream: true,
        metadata: None,
        prompt_cache_key: None,
        prompt_cache_options: None,
    }
}

#[test]
fn disabled_config_prepares_nothing() {
    let req = req_with_advisor_decl();
    let mut outbound = empty_outbound();
    let before = serde_json::to_value(&outbound).unwrap();
    assert!(!prepare(&req, &mut outbound, &AdvisorConfig::default()));
    assert_eq!(serde_json::to_value(&outbound).unwrap(), before);
}

#[test]
fn no_declaration_prepares_nothing_even_when_enabled() {
    let mut req = req_with_advisor_decl();
    req.tools = None;
    let mut outbound = empty_outbound();
    let before = serde_json::to_value(&outbound).unwrap();
    let cfg = AdvisorConfig {
        enabled: true,
        ..AdvisorConfig::default()
    };
    assert!(!prepare(&req, &mut outbound, &cfg));
    assert_eq!(serde_json::to_value(&outbound).unwrap(), before);
}

#[test]
fn prepare_adds_one_zero_argument_tool_and_forces_buffering() {
    let req = req_with_advisor_decl();
    let mut outbound = empty_outbound();
    let cfg = AdvisorConfig {
        enabled: true,
        ..AdvisorConfig::default()
    };
    assert!(prepare(&req, &mut outbound, &cfg));
    assert!(!outbound.stream);
    let tools = outbound.tools.unwrap();
    assert_eq!(tools.len(), 1);
    let v: Value = serde_json::to_value(&tools[0]).unwrap();
    assert_eq!(
        v,
        json!({
            "type": "function",
            "name": "advisor",
            "description": "Consult a reviewer model before substantive work. Takes no arguments.",
            "strict": true,
            "parameters": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }
        })
    );
}

#[test]
fn prepare_appends_to_existing_tools() {
    let mut req = req_with_advisor_decl();
    req.tools.as_mut().unwrap().push(Tool {
        name: "Bash".into(),
        description: Some("run a command".into()),
        input_schema: Some(json!({"type": "object", "properties": {}})),
        ..Tool::default()
    });
    let mut outbound = empty_outbound();
    outbound.tools = Some(vec![ToolDefinition::Function(ResponsesTool {
        kind: "function".into(),
        name: "Bash".into(),
        description: None,
        strict: true,
        parameters: json!({"type": "object", "properties": {}}),
    })]);
    let cfg = AdvisorConfig {
        enabled: true,
        ..AdvisorConfig::default()
    };
    assert!(prepare(&req, &mut outbound, &cfg));
    let tools = outbound.tools.unwrap();
    assert_eq!(tools.len(), 2);
    assert_eq!(serde_json::to_value(&tools[0]).unwrap()["name"], "Bash");
    assert_eq!(serde_json::to_value(&tools[1]).unwrap()["name"], "advisor");
}

#[test]
fn activation_requires_the_schema_less_declaration_shape() {
    let cfg = AdvisorConfig {
        enabled: true,
        ..AdvisorConfig::default()
    };
    // Real Claude Code declaration: name only, no input_schema.
    assert!(active_cfg(&cfg, &req_with_advisor_decl()));
    // An ordinary function tool also named `advisor` (with a schema)
    // must NOT activate the advisor pipeline.
    let mut req = req_with_advisor_decl();
    req.tools = Some(vec![Tool {
        name: "advisor".into(),
        description: Some("an ordinary client tool".into()),
        input_schema: Some(json!({"type": "object", "properties": {}})),
        ..Tool::default()
    }]);
    assert!(!active_cfg(&cfg, &req));
    let mut outbound = empty_outbound();
    assert!(!prepare(&req, &mut outbound, &cfg));
    assert!(outbound.stream, "normal streaming is preserved");
}

#[test]
fn prepare_does_not_duplicate_an_existing_advisor_function_tool() {
    let req = req_with_advisor_decl();
    let mut outbound = empty_outbound();
    outbound.tools = Some(vec![ToolDefinition::Function(ResponsesTool {
        kind: "function".into(),
        name: "advisor".into(),
        description: None,
        strict: true,
        parameters: json!({"type": "object", "properties": {}}),
    })]);
    let cfg = AdvisorConfig {
        enabled: true,
        ..AdvisorConfig::default()
    };
    assert!(prepare(&req, &mut outbound, &cfg));
    assert!(!outbound.stream, "still forced to buffer");
    let tools = outbound.tools.unwrap();
    assert_eq!(tools.len(), 1, "no duplicate advisor function tool");
}

#[test]
fn scrub_error_body_removes_all_three_latching_tokens() {
    let body = "400: advisor_20260301 / advisor-tool-2026-03-01 / Input tag 'advisor_20260301'";
    let scrubbed = scrub_error_body(body);
    for token in FORBIDDEN_ERROR_TOKENS {
        assert!(!scrubbed.contains(token), "{token} must be gone");
    }
    assert!(scrubbed.contains("[redacted]"));
    assert_eq!(
        scrub_error_body("plain upstream error"),
        "plain upstream error"
    );
}

#[test]
fn active_requires_both_the_flag_and_the_declaration() {
    let req = req_with_advisor_decl();
    assert!(
        !active_cfg(&AdvisorConfig::default(), &req),
        "default config is off"
    );
    let cfg = AdvisorConfig {
        enabled: true,
        ..AdvisorConfig::default()
    };
    assert!(active_cfg(&cfg, &req));
    let mut plain = req_with_advisor_decl();
    plain.tools = None;
    assert!(!active_cfg(&cfg, &plain));
}
