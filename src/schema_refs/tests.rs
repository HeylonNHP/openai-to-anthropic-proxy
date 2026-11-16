use super::*;

/// The real schema Claude Code sent for `mcp__rea__address_name`, which
/// produced the live `$ref cannot have keywords {'description'}` 400.
fn address_name_schema() -> Value {
    json!({
        "$defs": {
            "__schema0": {
                "$ref": "#/$defs/__schema1",
                "description": "The active target's provider document name; use open_binary to switch targets"
            },
            "__schema1": { "type": "string" },
            "__schema2": {
                "description": "A provider-normalized address; default memory uses 0x-prefixed hexadecimal",
                "type": "string"
            }
        },
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "additionalProperties": false,
        "examples": [{}],
        "properties": {
            "address": {
                "$ref": "#/$defs/__schema2",
                "description": "A provider-normalized address; default memory uses 0x-prefixed hexadecimal"
            },
            "document": {
                "$ref": "#/$defs/__schema0",
                "description": "The active target's provider document name; use open_binary to switch targets"
            }
        },
        "type": "object"
    })
}

/// True if a bare-or-not `$ref` appears anywhere in the schema.
fn has_ref(v: &Value) -> bool {
    match v {
        Value::Object(m) => m.contains_key("$ref") || m.values().any(has_ref),
        Value::Array(a) => a.iter().any(has_ref),
        _ => false,
    }
}

#[test]
fn resolves_the_rea_address_name_schema() {
    let out = resolve_local_refs(address_name_schema());
    assert!(!has_ref(&out), "no $ref may remain: {out}");
    assert!(out.get("$defs").is_none(), "unreferenced $defs are dropped");
    assert_eq!(out["properties"]["address"]["type"], "string");
    assert_eq!(
        out["properties"]["address"]["description"],
        "A provider-normalized address; default memory uses 0x-prefixed hexadecimal"
    );
    // `__schema0 -> __schema1` is a chain: the type comes from the far end.
    assert_eq!(out["properties"]["document"]["type"], "string");
    assert_eq!(
        out["properties"]["document"]["description"],
        "The active target's provider document name; use open_binary to switch targets"
    );
    // Everything unrelated to references is left alone.
    assert_eq!(
        out["$schema"],
        "https://json-schema.org/draft/2020-12/schema"
    );
    assert_eq!(out["additionalProperties"], false);
    assert_eq!(out["type"], "object");
}

#[test]
fn schema_without_refs_or_defs_is_returned_as_is() {
    let schema = json!({"type": "object", "properties": {"x": {"type": "string"}}});
    assert_eq!(resolve_local_refs(schema.clone()), schema);
}

#[test]
fn unused_root_defs_are_dropped() {
    let schema = json!({
        "type": "object",
        "properties": {"x": {"type": "string"}},
        "$defs": {"Unused": {"type": "integer"}}
    });
    let out = resolve_local_refs(schema);
    assert!(out.get("$defs").is_none());
    assert_eq!(out["properties"]["x"]["type"], "string");
}

#[test]
fn sibling_annotation_overrides_the_target_annotation() {
    let schema = json!({
        "type": "object",
        "properties": {"x": {"$ref": "#/$defs/S", "description": "at the use site", "default": "a"}},
        "$defs": {"S": {"type": "string", "description": "on the definition", "default": "b"}}
    });
    let out = resolve_local_refs(schema);
    assert_eq!(out["properties"]["x"]["description"], "at the use site");
    assert_eq!(out["properties"]["x"]["default"], "a");
    assert_eq!(out["properties"]["x"]["type"], "string");
}

#[test]
fn sibling_validation_keywords_are_merged_not_dropped() {
    // The target has a `pattern`, the use site adds `minLength`. JSON
    // Schema applies both, so both must survive.
    let schema = json!({
        "type": "object",
        "properties": {"x": {"$ref": "#/$defs/S", "minLength": 3}},
        "$defs": {"S": {"type": "string", "pattern": "^a"}}
    });
    let out = resolve_local_refs(schema);
    assert_eq!(out["properties"]["x"]["pattern"], "^a");
    assert_eq!(out["properties"]["x"]["minLength"], 3);
}

#[test]
fn conflicting_sibling_validation_keyword_keeps_the_target_value() {
    let schema = json!({
        "type": "object",
        "properties": {"x": {"$ref": "#/$defs/S", "type": "integer"}},
        "$defs": {"S": {"type": "string"}}
    });
    let out = resolve_local_refs(schema);
    assert_eq!(out["properties"]["x"]["type"], "string");
}

#[test]
fn resolves_the_legacy_definitions_container() {
    let schema = json!({
        "type": "object",
        "properties": {"x": {"$ref": "#/definitions/S"}},
        "definitions": {"S": {"type": "string"}}
    });
    let out = resolve_local_refs(schema);
    assert_eq!(out["properties"]["x"]["type"], "string");
    assert!(out.get("definitions").is_none());
}

#[test]
fn decodes_pointer_escapes_and_percent_encoding() {
    let schema = json!({
        "type": "object",
        "properties": {
            "slash": {"$ref": "#/$defs/a~1b"},
            "tilde": {"$ref": "#/$defs/a~0b"},
            "space": {"$ref": "#/$defs/a%20b"}
        },
        "$defs": {
            "a/b": {"type": "string"},
            "a~b": {"type": "integer"},
            "a b": {"type": "boolean"}
        }
    });
    let out = resolve_local_refs(schema);
    assert_eq!(out["properties"]["slash"]["type"], "string");
    assert_eq!(out["properties"]["tilde"]["type"], "integer");
    assert_eq!(out["properties"]["space"]["type"], "boolean");
}

#[test]
fn resolves_a_pointer_into_the_document_body() {
    let schema = json!({
        "type": "object",
        "properties": {
            "from": {"type": "string", "description": "origin"},
            "to": {"$ref": "#/properties/from"}
        }
    });
    let out = resolve_local_refs(schema);
    assert_eq!(out["properties"]["to"]["type"], "string");
    assert_eq!(out["properties"]["to"]["description"], "origin");
}

#[test]
fn root_level_ref_resolves_and_ignores_the_defs_sibling() {
    let schema = json!({
        "$ref": "#/$defs/Root",
        "$defs": {"Root": {"type": "object", "properties": {"x": {"type": "string"}}}}
    });
    let out = resolve_local_refs(schema);
    assert_eq!(out["type"], "object");
    assert_eq!(out["properties"]["x"]["type"], "string");
    assert!(out.get("$defs").is_none());
    assert!(!has_ref(&out));
}

#[test]
fn a_property_named_like_a_data_keyword_is_still_resolved() {
    let schema = json!({
        "type": "object",
        "properties": {"default": {"$ref": "#/$defs/S"}, "enum": {"$ref": "#/$defs/S"}},
        "$defs": {"S": {"type": "string"}}
    });
    let out = resolve_local_refs(schema);
    assert_eq!(out["properties"]["default"]["type"], "string");
    assert_eq!(out["properties"]["enum"]["type"], "string");
}

#[test]
fn data_keywords_are_not_walked() {
    let schema = json!({
        "type": "object",
        "properties": {"x": {"type": "object", "default": {"$ref": "not-a-schema"}}},
        "$defs": {"S": {"type": "string"}}
    });
    let out = resolve_local_refs(schema);
    assert_eq!(
        out["properties"]["x"]["default"],
        json!({"$ref": "not-a-schema"})
    );
}

#[test]
fn root_recursion_keeps_a_bare_hash_ref() {
    let schema = json!({
        "type": "object",
        "properties": {"child": {"$ref": "#", "description": "a nested node"}}
    });
    let out = resolve_local_refs(schema);
    // One level is inlined; the next hits the cycle and stays bare.
    assert_eq!(out["properties"]["child"]["type"], "object");
    assert_eq!(
        out["properties"]["child"]["properties"]["child"],
        json!({"$ref": "#"})
    );
}

#[test]
fn named_recursion_retains_only_the_recursive_definition() {
    let schema = json!({
        "type": "object",
        "properties": {
            "tree": {"$ref": "#/$defs/Node", "description": "the tree"},
            "label": {"$ref": "#/$defs/Label"}
        },
        "$defs": {
            "Node": {
                "type": "object",
                "properties": {
                    "value": {"$ref": "#/$defs/Label"},
                    "next": {"$ref": "#/$defs/Node", "description": "the next node"}
                }
            },
            "Label": {"type": "string"}
        }
    });
    let out = resolve_local_refs(schema);

    // The non-recursive definition is gone; only the recursive one stays.
    let defs = out["$defs"].as_object().expect("recursive def retained");
    assert_eq!(defs.keys().collect::<Vec<_>>(), vec!["Node"]);
    assert_eq!(out["properties"]["label"]["type"], "string");

    // Every surviving reference is bare (no siblings) and points at it.
    fn check(v: &Value) {
        match v {
            Value::Object(m) => {
                if let Some(r) = m.get("$ref") {
                    assert_eq!(r, "#/$defs/Node");
                    assert_eq!(m.len(), 1, "a retained $ref must be bare: {m:?}");
                }
                m.values().for_each(check);
            }
            Value::Array(a) => a.iter().for_each(check),
            _ => {}
        }
    }
    check(&out);
    assert_eq!(
        out["$defs"]["Node"]["properties"]["value"]["type"],
        "string"
    );
    assert_eq!(
        out["$defs"]["Node"]["properties"]["next"],
        json!({"$ref": "#/$defs/Node"})
    );
}

#[test]
fn mutual_recursion_retains_both_definitions() {
    let schema = json!({
        "type": "object",
        "properties": {"a": {"$ref": "#/$defs/A"}},
        "$defs": {
            "A": {"type": "object", "properties": {"b": {"$ref": "#/$defs/B"}}},
            "B": {"type": "object", "properties": {"a": {"$ref": "#/$defs/A"}}}
        }
    });
    let out = resolve_local_refs(schema);
    let defs = out["$defs"].as_object().expect("defs retained");
    assert!(defs.contains_key("A"));
    // Every `$ref` left anywhere resolves to something that still exists.
    fn check(v: &Value, root: &Value) {
        match v {
            Value::Object(m) => {
                if let Some(Value::String(r)) = m.get("$ref") {
                    let name = r.rsplit('/').next().unwrap();
                    assert!(root["$defs"].get(name).is_some(), "dangling {r}");
                }
                m.values().for_each(|c| check(c, root));
            }
            Value::Array(a) => a.iter().for_each(|c| check(c, root)),
            _ => {}
        }
    }
    check(&out, &out);
}

#[test]
fn a_missing_target_leaves_the_schema_unchanged() {
    let schema = json!({
        "type": "object",
        "properties": {"x": {"$ref": "#/$defs/Nope", "description": "d"}},
        "$defs": {"Other": {"type": "string"}}
    });
    assert_eq!(resolve_local_refs(schema.clone()), schema);
}

#[test]
fn a_remote_reference_is_never_fetched_and_leaves_the_schema_unchanged() {
    let schema = json!({
        "type": "object",
        "properties": {"x": {"$ref": "https://example.com/schema.json"}}
    });
    assert_eq!(resolve_local_refs(schema.clone()), schema);
}

#[test]
fn a_cycle_through_an_unretainable_pointer_leaves_the_schema_unchanged() {
    // `#/properties/a/properties/b` loops through a location that is not
    // a `$defs` entry, so there is nothing to retain.
    let schema = json!({
        "type": "object",
        "properties": {
            "a": {
                "type": "object",
                "properties": {"b": {"$ref": "#/properties/a/properties/b"}}
            }
        }
    });
    assert_eq!(resolve_local_refs(schema.clone()), schema);
}

#[test]
fn exponential_sharing_is_refused_instead_of_expanded() {
    // Each definition references the next one twice: 2^30 copies if
    // inlined naively.
    let mut defs = Map::new();
    for i in 0..30 {
        defs.insert(
            format!("D{i}"),
            json!({
                "type": "object",
                "properties": {
                    "l": {"$ref": format!("#/$defs/D{}", i + 1)},
                    "r": {"$ref": format!("#/$defs/D{}", i + 1)}
                }
            }),
        );
    }
    defs.insert("D30".to_owned(), json!({"type": "string"}));
    let schema = json!({
        "type": "object",
        "properties": {"x": {"$ref": "#/$defs/D0"}},
        "$defs": Value::Object(defs)
    });
    assert_eq!(resolve_local_refs(schema.clone()), schema);
}
