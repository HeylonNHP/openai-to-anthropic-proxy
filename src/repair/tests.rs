use super::*;
use serde_json::json;

#[test]
fn validate_flags_missing_required() {
    let schema = json!({
        "type": "object",
        "properties": {"location": {"type": "string"}},
        "required": ["location"],
    });
    assert!(validate(&json!({}), &schema).is_some());
    assert!(validate(&json!({"location": "SF"}), &schema).is_none());
}

#[test]
fn validate_flags_pattern_violation() {
    let schema = json!({
        "type": "string",
        "pattern": "^wf_[a-z0-9-]{6,}$"
    });
    assert!(validate(&json!("bogus"), &schema).is_some());
    assert!(validate(&json!("wf_abc123"), &schema).is_none());
}

#[test]
fn validate_flags_unexpected_property() {
    let schema = json!({
        "type": "object",
        "properties": {"a": {"type": "string"}},
        "additionalProperties": false,
    });
    assert!(validate(&json!({"a": "x", "b": 1}), &schema).is_some());
}

#[test]
fn repair_strips_null_sentinel_on_optional() {
    let original = json!({
        "type": "object",
        "properties": {
            "location": {"type": "string"},
            "resumeFromRunId": {"type": "string"}
        },
        "required": ["location"],
    });
    let input = json!({"location": "SF", "resumeFromRunId": null});
    let out = repair(input, &original);
    assert_eq!(out, json!({"location": "SF"}));
    assert!(validate(&out, &original).is_none());
}

/// A retained recursive `$ref` has no `type`, so without the opaque
/// handling the repair would treat it as a typeless node and could
/// JSON-decode a string the model legitimately sent.
#[test]
fn retained_ref_nodes_are_opaque_to_validate_and_repair() {
    let original = json!({
        "type": "object",
        "properties": {"tree": {"$ref": "#/$defs/Node"}},
        "required": ["tree"],
        "$defs": {"Node": {"type": "object", "properties": {}}},
    });
    let input = json!({"tree": "{\"a\": 1}"});
    assert!(validate(&input, &original).is_none());
    assert_eq!(repair(input.clone(), &original), input);
}

#[test]
fn repair_keeps_null_on_required() {
    let original = json!({
        "type": "object",
        "properties": {"location": {"type": ["string", "null"]}},
        "required": ["location"],
    });
    let input = json!({"location": null});
    let out = repair(input.clone(), &original);
    assert_eq!(out, input);
}

#[test]
fn repair_drops_invalid_optional_value() {
    let original = json!({
        "type": "object",
        "properties": {
            "location": {"type": "string"},
            "resumeFromRunId": {"type": "string", "pattern": "^wf_[a-z0-9-]{6,}$"}
        },
        "required": ["location"],
    });
    let input = json!({"location": "SF", "resumeFromRunId": "not-a-run-id"});
    let out = repair(input, &original);
    assert_eq!(out, json!({"location": "SF"}));
}

#[test]
fn repair_keeps_valid_optional_value() {
    let original = json!({
        "type": "object",
        "properties": {
            "location": {"type": "string"},
            "resumeFromRunId": {"type": "string", "pattern": "^wf_[a-z0-9-]{6,}$"}
        },
        "required": ["location"],
    });
    let input = json!({"location": "SF", "resumeFromRunId": "wf_abc123"});
    let out = repair(input.clone(), &original);
    assert_eq!(out, input);
}

#[test]
fn repair_decodes_json_in_string_for_typeless_schema() {
    // The sanitizer turns typeless schemas into `type: "string"`;
    // the model then JSON-encodes structured values.
    let original = json!({"description": "any value"});
    let input = json!("[1, 2, 3]");
    let out = repair(input, &original);
    assert_eq!(out, json!([1, 2, 3]));
}

#[test]
fn repair_leaves_plain_strings_alone() {
    let original = json!({"type": "string"});
    let input = json!("hello");
    let out = repair(input.clone(), &original);
    assert_eq!(out, input);
}

#[test]
fn repair_normalises_null_to_empty_object() {
    let original = json!({"type": "object", "properties": {}});
    let out = repair(Value::Null, &original);
    assert_eq!(out, json!({}));
}

#[test]
fn repair_decodes_stringified_object() {
    let original = json!({
        "type": "object",
        "properties": {"location": {"type": "string"}},
        "required": ["location"],
    });
    let out = repair(json!("{\"location\":\"SF\"}"), &original);
    assert_eq!(out, json!({"location": "SF"}));
}

#[test]
fn repair_removes_forbidden_undeclared_keys() {
    let original = json!({
        "type": "object",
        "properties": {"a": {"type": "string"}},
        "additionalProperties": false,
    });
    let out = repair(json!({"a": "x", "_raw_arguments": "junk"}), &original);
    assert_eq!(out, json!({"a": "x"}));
}

#[test]
fn repair_recurses_into_nested_properties() {
    let original = json!({
        "type": "object",
        "properties": {
            "opts": {
                "type": "object",
                "properties": {
                    "model": {"type": "string"},
                    "effort": {"type": "string", "enum": ["low", "high"]}
                },
                "required": ["model"]
            }
        },
    });
    let input = json!({"opts": {"model": "m", "effort": "bogus"}});
    let out = repair(input, &original);
    assert_eq!(out, json!({"opts": {"model": "m"}}));
}

#[test]
fn registry_round_trips_schemas() {
    let mut reg = ToolSchemaRegistry::new();
    assert!(reg.is_empty());
    reg.insert("Workflow".into(), json!({"type": "object"}));
    assert!(!reg.is_empty());
    assert!(reg.get("Workflow").is_some());
    assert!(reg.get("Other").is_none());
}
