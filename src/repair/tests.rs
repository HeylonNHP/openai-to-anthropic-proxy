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

// ─── reading upstream arguments ───────────────────────────────────

/// The parameters a cut-short argument blob yielded, if it was one.
fn salvaged(raw: &str) -> Option<Value> {
    match read_arguments(raw) {
        Arguments::Truncated(value) => Some(value),
        _ => None,
    }
}

#[test]
fn whole_arguments_are_read_as_they_are() {
    match read_arguments(r#"{"a":1,"b":"x"}"#) {
        Arguments::Whole(value) => assert_eq!(value, json!({"a": 1, "b": "x"})),
        _ => panic!("expected whole arguments"),
    }
}

#[test]
fn empty_arguments_are_an_empty_object() {
    match read_arguments("   ") {
        Arguments::Whole(value) => assert_eq!(value, json!({})),
        _ => panic!("expected whole arguments"),
    }
}

#[test]
fn arguments_that_are_not_json_are_not_recovered() {
    assert!(matches!(
        read_arguments("not valid json"),
        Arguments::Malformed
    ));
}

#[test]
fn a_truncated_call_keeps_the_parameters_that_were_finished() {
    // The upstream ran out of output tokens mid-call. Anthropic's own
    // API keeps the parameters that were finished.
    let salvaged = salvaged(r#"{"path":"/tmp","content":"hello"#).expect("truncated");
    assert_eq!(salvaged, json!({"path": "/tmp"}));
}

#[test]
fn a_truncated_call_keeps_every_finished_parameter() {
    let salvaged = salvaged(r#"{"path":"/tmp","flag":true,"count":3"#).expect("truncated");
    assert_eq!(salvaged, json!({"path": "/tmp", "flag": true}));
}

#[test]
fn a_truncated_string_drops_the_member_being_written() {
    // The member being written when the cut happened is not a finished
    // parameter, so it goes — but the one before it stays.
    let salvaged = salvaged(r#"{"path":"/tmp","content":"hel"#).expect("truncated");
    assert_eq!(salvaged, json!({"path": "/tmp"}));
}

#[test]
fn a_truncated_number_is_not_a_finished_parameter() {
    // `12` could have been `123`: the digits the upstream did not get
    // to write are missing, so the number is not a finished parameter.
    let salvaged = salvaged(r#"{"path":"/tmp","count":12"#).expect("truncated");
    assert_eq!(salvaged, json!({"path": "/tmp"}));
}

#[test]
fn a_truncated_call_with_nothing_finished_is_an_empty_object() {
    let salvaged = salvaged(r#"{"path"#).expect("truncated");
    assert_eq!(salvaged, json!({}));
}

#[test]
fn a_truncated_call_keeps_a_finished_string_containing_a_comma() {
    let salvaged = salvaged(r#"{"path":"/tmp","note":"a,b"#).expect("truncated");
    assert_eq!(salvaged, json!({"path": "/tmp"}));
}

#[test]
fn a_truncated_call_keeps_a_finished_nested_object() {
    let salvaged = salvaged(r#"{"opts":{"model":"m"},"x":1"#).expect("truncated");
    assert_eq!(salvaged, json!({"opts": {"model": "m"}}));
}

#[test]
fn a_truncated_call_that_is_not_an_object_recovers_nothing() {
    let salvaged = salvaged("[1,2").expect("truncated");
    assert_eq!(salvaged, json!({}));
}

#[test]
fn a_number_the_blob_ends_after_is_complete() {
    // A comma terminated the number, so it is not ambiguous and stays.
    let salvaged = salvaged(r#"{"a":1,"b":2,"#).expect("truncated");
    assert_eq!(salvaged, json!({"a": 1, "b": 2}));
}

#[test]
fn a_literal_at_the_end_of_the_blob_is_complete() {
    // `true` is a fixed token: it cannot have been cut mid-way.
    let salvaged = salvaged(r#"{"a":1,"b":true"#).expect("truncated");
    assert_eq!(salvaged, json!({"a": 1, "b": true}));
}

#[test]
fn a_number_at_the_end_of_the_blob_is_dropped() {
    // `2` could have been `23`, so it is not a finished parameter.
    let salvaged = salvaged(r#"{"a":1,"b":2"#).expect("truncated");
    assert_eq!(salvaged, json!({"a": 1}));
}

#[test]
fn a_number_terminated_by_a_comma_is_kept() {
    let salvaged = salvaged(r#"{"a":1,"b":2,"#).expect("truncated");
    assert_eq!(salvaged, json!({"a": 1, "b": 2}));
}

#[test]
fn a_truncated_call_is_reported_as_a_truncation_not_a_schema_violation() {
    // The client will reject the call — it is incomplete — but the
    // cause is the output cap, not the model's arguments. The two are
    // reported differently so the caller can tell them apart, and so
    // it can decide not to spend a corrective retry on it.
    let resp = crate::responses::ResponsesResponse {
        id: "resp_1".into(),
        object: "response".into(),
        created_at: 1,
        status: "incomplete".into(),
        model: "m".into(),
        output: vec![crate::responses::OutputItem::FunctionCall {
            id: Some("fc_1".into()),
            status: Some("incomplete".into()),
            call_id: "call_1".into(),
            name: "Workflow".into(),
            arguments: r#"{"script":"s","resumeFromRunId":"half a "#.into(),
        }],
        usage: None,
        error: None,
        incomplete_details: None,
    };
    let violations = tool_input_violations(&resp, &ToolSchemaRegistry::new());
    assert_eq!(violations.len(), 1);
    assert_eq!(violations[0].0, "Workflow");
    assert!(
        violations[0].1.contains("cut short"),
        "the violation names the truncation: {:?}",
        violations[0]
    );
    // It is recognised as a truncation all the same.
    assert!(has_truncated_call(&resp));
}

#[test]
fn a_truncated_reply_is_recognised_as_one() {
    let resp = crate::responses::ResponsesResponse {
        id: "resp_1".into(),
        object: "response".into(),
        created_at: 1,
        status: "completed".into(),
        model: "m".into(),
        output: vec![crate::responses::OutputItem::FunctionCall {
            id: Some("fc_1".into()),
            status: Some("completed".into()),
            call_id: "call_1".into(),
            name: "Workflow".into(),
            arguments: r#"{"script":"s","resumeFromRunId":"half a "#.into(),
        }],
        usage: None,
        error: None,
        incomplete_details: None,
    };
    assert!(has_truncated_call(&resp));
}
