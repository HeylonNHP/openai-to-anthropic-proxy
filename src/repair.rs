//! Response-side tool-call repair.
//!
//! The request-side sanitizer (`translate::sanitize_tool_schema`)
//! rewrites every tool schema so it compiles under the OpenAI Responses
//! strict validator: every property becomes `required`, optional
//! properties are encoded as nullable unions (`type: ["X","null"]` or an
//! added `{"type":"null"}` `anyOf` branch), typeless nodes get a `type`,
//! and so on. The backend model therefore produces arguments that
//! conform to the *mutated* schema — which is not what the Anthropic
//! client validates against. The client (e.g. Claude Code) validates the
//! `tool_use.input` we ship against the *original* schema, and rejects
//! the whole call with `InputValidationError` ("Invalid tool
//! parameters") on any mismatch.
//!
//! This module closes that gap on the response side:
//!
//! - [`ToolSchemaRegistry`] remembers the original `input_schema` per
//!   tool name for one in-flight request, so the response path can
//!   invert the sanitizer's mutations.
//! - [`repair`] inverts the mutations: strips null sentinels from
//!   nullable-encoded optional properties, drops optional keys whose
//!   values violate the original subschema (e.g. a fabricated
//!   `resumeFromRunId` that fails the original `pattern`), decodes
//!   JSON-in-string values produced by the string encoding of typeless
//!   schemas, removes keys the original schema forbids, and normalises
//!   `null` / empty input to `{}`.
//! - [`validate`] is a small JSON-Schema-subset validator (the subset
//!   Claude Code emits) used both to drive repair decisions and to gate
//!   the proxy's hidden retry loop for unrepairable inputs.
//!
//! Repair is best-effort by design: it only *removes* or *decodes*
//! information the sanitizer added; it never invents values. When a
//! genuinely required parameter is missing or wrong, `validate` still
//! reports a violation and the caller (the proxy handler) can re-query
//! the backend with a corrective message instead of shipping a call the
//! client will reject.

use std::collections::HashMap;

use serde_json::{Map, Value};

/// Original (client-supplied) `input_schema` per tool name, captured at
/// request-translation time and consulted when translating the
/// response. Cheap to clone (one `HashMap` per request).
#[derive(Debug, Clone, Default)]
pub struct ToolSchemaRegistry {
    schemas: HashMap<String, Value>,
}

impl ToolSchemaRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Remember the original schema for a tool.
    pub fn insert(&mut self, name: String, schema: Value) {
        self.schemas.insert(name, schema);
    }

    /// The original schema for a tool, if the tool was declared on the
    /// inbound request.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.schemas.get(name)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.schemas.is_empty()
    }
}

// ─── validation ────────────────────────────────────────────────────

/// Validate `value` against the JSON-Schema subset Claude Code emits.
///
/// Covered keywords: `type`, `enum`, `const`, `properties`, `required`,
/// `additionalProperties`, `items`, `anyOf`/`oneOf`/`allOf`, `not`,
/// `pattern`, numeric/string/array bounds. Returns the first
/// human-readable violation, or `None` when the value conforms.
///
/// Unknown / unrepresentable keywords are treated as passing — the
/// validator's job is to catch the shapes the client's own validator
/// will reject, not to be a complete JSON Schema implementation.
#[must_use]
pub fn validate(value: &Value, schema: &Value) -> Option<String> {
    validate_inner(value, schema, "input")
}

fn validate_inner(value: &Value, schema: &Value, path: &str) -> Option<String> {
    let Some(obj) = schema.as_object() else {
        // `true`-ish / non-object schemas accept anything.
        return None;
    };

    // Combinators.
    if let Some(arr) = obj.get("anyOf").and_then(Value::as_array) {
        if arr.iter().any(|b| validate_inner(value, b, path).is_none()) {
            return None;
        }
        return Some(format!("{path}: value matches no branch of `anyOf`"));
    }
    if let Some(arr) = obj.get("oneOf").and_then(Value::as_array) {
        let hits = arr
            .iter()
            .filter(|b| validate_inner(value, b, path).is_none())
            .count();
        if hits != 1 {
            return Some(format!(
                "{path}: value matches {hits} branches of `oneOf` (expected exactly 1)"
            ));
        }
        return None;
    }
    if let Some(arr) = obj.get("allOf").and_then(Value::as_array) {
        for branch in arr {
            if let Some(e) = validate_inner(value, branch, path) {
                return Some(e);
            }
        }
        // `allOf` may coexist with sibling keywords; fall through.
    }
    if let Some(not) = obj.get("not")
        && validate_inner(value, not, path).is_none()
    {
        return Some(format!("{path}: value violates `not`"));
    }

    // enum / const.
    if let Some(en) = obj.get("enum").and_then(Value::as_array)
        && !en.iter().any(|e| e == value)
    {
        return Some(format!(
            "{path}: value is not one of the declared enum values"
        ));
    }
    if let Some(c) = obj.get("const")
        && value != c
    {
        return Some(format!("{path}: value does not equal `const`"));
    }

    // type.
    if let Some(t) = obj.get("type") {
        let types: Vec<&str> = match t {
            Value::String(s) => vec![s.as_str()],
            Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        if !types.is_empty() && !types.iter().any(|ty| json_type_matches(value, ty)) {
            return Some(format!(
                "{path}: expected type {}, got {}",
                types.join("|"),
                value_type_name(value)
            ));
        }
    }

    // pattern.
    if let (Some(p), Value::String(s)) = (obj.get("pattern").and_then(Value::as_str), value) {
        match regex::Regex::new(p) {
            Ok(re) => {
                if !re.is_match(s) {
                    return Some(format!("{path}: string does not match pattern `{p}`"));
                }
            }
            Err(e) => {
                tracing::warn!(pattern = p, error = %e, "invalid pattern in tool schema; skipping pattern check")
            }
        }
    }

    // Numeric bounds.
    if let Some(f) = value.as_f64() {
        if let Some(max) = obj.get("exclusiveMaximum").and_then(Value::as_f64)
            && f >= max
        {
            return Some(format!("{path}: value >= exclusiveMaximum"));
        }
        if let Some(max) = obj.get("maximum").and_then(Value::as_f64)
            && f > max
        {
            return Some(format!("{path}: value > maximum"));
        }
        if let Some(min) = obj.get("exclusiveMinimum").and_then(Value::as_f64)
            && f <= min
        {
            return Some(format!("{path}: value <= exclusiveMinimum"));
        }
        if let Some(min) = obj.get("minimum").and_then(Value::as_f64)
            && f < min
        {
            return Some(format!("{path}: value < minimum"));
        }
    }

    // String bounds.
    if let Value::String(s) = value {
        let len = s.chars().count();
        if let Some(max) = obj.get("maxLength").and_then(Value::as_u64)
            && len > max as usize
        {
            return Some(format!("{path}: string longer than maxLength"));
        }
        if let Some(min) = obj.get("minLength").and_then(Value::as_u64)
            && len < min as usize
        {
            return Some(format!("{path}: string shorter than minLength"));
        }
    }

    // Object checks.
    if let Value::Object(map) = value {
        if let Some(req) = obj.get("required").and_then(Value::as_array) {
            for r in req.iter().filter_map(Value::as_str) {
                if !map.contains_key(r) {
                    return Some(format!("{path}: missing required property `{r}`"));
                }
            }
        }
        let declared: Vec<&str> = obj
            .get("properties")
            .and_then(Value::as_object)
            .map(|o| o.keys().map(String::as_str).collect())
            .unwrap_or_default();
        if let Some(props) = obj.get("properties").and_then(Value::as_object) {
            for (k, sub) in props {
                if let Some(v) = map.get(k)
                    && let Some(e) = validate_inner(v, sub, &format!("{path}.{k}"))
                {
                    return Some(e);
                }
            }
        }
        match obj.get("additionalProperties") {
            Some(Value::Bool(false)) => {
                for k in map.keys() {
                    if !declared.contains(&k.as_str()) {
                        return Some(format!(
                            "{path}: unexpected property `{k}` (additionalProperties: false)"
                        ));
                    }
                }
            }
            Some(addl @ Value::Object(_)) => {
                for (k, v) in map {
                    if !declared.contains(&k.as_str())
                        && let Some(e) = validate_inner(v, addl, &format!("{path}.{k}"))
                    {
                        return Some(e);
                    }
                }
            }
            _ => {}
        }
    }

    // Array checks.
    if let Value::Array(items) = value {
        if let Some(max) = obj.get("maxItems").and_then(Value::as_u64)
            && items.len() > max as usize
        {
            return Some(format!("{path}: array longer than maxItems"));
        }
        if let Some(min) = obj.get("minItems").and_then(Value::as_u64)
            && items.len() < min as usize
        {
            return Some(format!("{path}: array shorter than minItems"));
        }
        if let Some(item_schema) = obj.get("items") {
            for (i, it) in items.iter().enumerate() {
                if let Some(e) = validate_inner(it, item_schema, &format!("{path}[{i}]")) {
                    return Some(e);
                }
            }
        }
    }

    None
}

fn json_type_matches(value: &Value, ty: &str) -> bool {
    match ty {
        "null" => value.is_null(),
        "boolean" => value.is_boolean(),
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => value.is_i64() || value.is_u64(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        _ => true,
    }
}

fn value_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

// ─── repair ────────────────────────────────────────────────────────

/// Invert the request-side strict sanitization so `input` conforms to
/// the *original* schema the client will validate against.
///
/// Rules applied (recursively):
/// 1. `null` input normalises to `{}` (Anthropic `tool_use.input` must
///    be an object).
/// 2. Null sentinels on *optional* properties (the nullable encoding of
///    "omitted") are removed.
/// 3. Optional properties whose repaired value still violates the
///    original subschema (wrong type, enum, pattern, …) are removed —
///    the client rejects the whole call otherwise, while omitting an
///    optional key is always legal.
/// 4. Properties the original schema forbids (`additionalProperties:
///    false`) are removed.
/// 5. String values at nodes whose original schema accepts non-string
///    JSON (the sanitizer's typeless→`string` encoding) are decoded
///    back into real JSON when they parse.
/// 6. A non-object value where the original schema is an object is
///    JSON-decoded if possible (models sometimes stringify the whole
///    argument object).
///
/// Required properties are never removed; violations on them are left
/// for [`validate`] to report so the caller can decide on a retry.
#[must_use]
pub fn repair(input: Value, original: &Value) -> Value {
    let mut v = if input.is_null() {
        Value::Object(Map::new())
    } else {
        input
    };
    // Whole-value stringification: the original expects an object but
    // the model shipped a JSON string of it.
    if let (Value::String(s), true) = (&v, original_expects_object(original))
        && let Ok(parsed) = serde_json::from_str::<Value>(s)
        && parsed.is_object()
    {
        v = parsed;
    }
    repair_inner(&mut v, original);
    v
}

fn original_expects_object(schema: &Value) -> bool {
    let Some(obj) = schema.as_object() else {
        return false;
    };
    matches!(obj.get("type"), Some(Value::String(t)) if t == "object")
        || (obj.get("type").is_none() && obj.contains_key("properties"))
}

fn repair_inner(value: &mut Value, schema: &Value) {
    let Some(obj) = schema.as_object() else {
        return;
    };

    // Combinator nodes: try to make the value conform to one branch.
    if let Some(arr) = obj
        .get("anyOf")
        .or_else(|| obj.get("oneOf"))
        .and_then(Value::as_array)
    {
        if arr
            .iter()
            .any(|b| validate_inner(value, b, "input").is_none())
        {
            return;
        }
        for branch in arr {
            let mut candidate = value.clone();
            repair_inner(&mut candidate, branch);
            if validate_inner(&candidate, branch, "input").is_none() {
                *value = candidate;
                return;
            }
        }
        return;
    }

    match value {
        Value::Object(map) => {
            let declared: Vec<String> = obj
                .get("properties")
                .and_then(Value::as_object)
                .map(|o| o.keys().cloned().collect())
                .unwrap_or_default();
            let required: Vec<String> = obj
                .get("required")
                .and_then(Value::as_array)
                .map(|r| {
                    r.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();

            // Remove forbidden undeclared keys first (original
            // additionalProperties: false).
            if matches!(obj.get("additionalProperties"), Some(Value::Bool(false))) {
                let forbidden: Vec<String> = map
                    .keys()
                    .filter(|k| !declared.contains(k))
                    .cloned()
                    .collect();
                for k in forbidden {
                    tracing::debug!(key = %k, "repair: dropping property forbidden by original schema");
                    map.remove(&k);
                }
            }

            if let Some(props) = obj.get("properties").and_then(Value::as_object) {
                let mut remove: Vec<String> = Vec::new();
                for (k, sub) in props {
                    let Some(v) = map.get_mut(k) else {
                        continue;
                    };
                    let is_required = required.contains(k);
                    // Rule 2: null sentinel on an optional property.
                    if v.is_null() {
                        if !is_required {
                            remove.push(k.clone());
                        }
                        continue;
                    }
                    repair_inner(v, sub);
                    // Rule 3: optional property still violating the
                    // original subschema after repair.
                    if !is_required && validate_inner(v, sub, &format!("input.{k}")).is_some() {
                        remove.push(k.clone());
                    }
                }
                for k in remove {
                    tracing::debug!(key = %k, "repair: dropping optional property that violates the original schema");
                    map.remove(&k);
                }
            }

            // Recurse into undeclared-but-allowed keys when the
            // original carries an additionalProperties sub-schema.
            if let Some(addl @ Value::Object(_)) = obj.get("additionalProperties") {
                for (k, v) in map.iter_mut() {
                    if !declared.contains(k) {
                        repair_inner(v, addl);
                    }
                }
            }
        }
        Value::Array(items) => {
            if let Some(item_schema) = obj.get("items") {
                for it in items.iter_mut() {
                    repair_inner(it, item_schema);
                }
            }
        }
        Value::String(s) => {
            // Rule 5: the sanitizer encodes typeless / non-string
            // schemas as `type: "string"`; decode JSON-in-string back
            // when the original accepts non-string values.
            if accepts_non_string(obj)
                && let Ok(parsed) = serde_json::from_str::<Value>(s)
                && !parsed.is_string()
            {
                *value = parsed;
                repair_inner(value, schema);
            }
        }
        _ => {}
    }
}

/// True when the original (pre-sanitization) schema at this node
/// accepts at least one non-string JSON type — i.e. the string encoding
/// was lossy and a JSON-in-string value should be decoded.
fn accepts_non_string(obj: &Map<String, Value>) -> bool {
    let type_accepts_non_string = |t: &Value| match t {
        Value::String(s) => s != "string",
        Value::Array(ts) => ts.iter().any(|x| x.as_str().is_some_and(|s| s != "string")),
        _ => false,
    };
    if let Some(t) = obj.get("type") {
        if type_accepts_non_string(t) {
            return true;
        }
    } else {
        // Typeless nodes accept anything (this is the `unknown` /
        // description-only shape the sanitizer stringifies).
        return true;
    }
    for kw in ["anyOf", "oneOf", "allOf"] {
        if let Some(arr) = obj.get(kw).and_then(Value::as_array)
            && arr.iter().any(|b| {
                b.as_object()
                    .and_then(|o| o.get("type"))
                    .is_some_and(type_accepts_non_string)
            })
        {
            return true;
        }
    }
    false
}

// ─── response-level helpers ────────────────────────────────────────

/// Collect tool-input violations across a response.
///
/// For every function-call output item, run the same parse+repair+
/// validate the response path runs, and return one `(tool, violation)`
/// pair per call whose repaired input still violates the original
/// schema (or whose arguments are not valid JSON at all). Empty when
/// everything conforms — i.e. when the client will accept the call.
#[must_use]
pub fn tool_input_violations(
    resp: &crate::responses::ResponsesResponse,
    registry: &ToolSchemaRegistry,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for item in &resp.output {
        if let crate::responses::OutputItem::FunctionCall {
            name, arguments, ..
        } = item
        {
            let parsed = match serde_json::from_str::<Value>(arguments) {
                Ok(v) => v,
                Err(_) => {
                    out.push((name.clone(), "arguments are not valid JSON".to_owned()));
                    continue;
                }
            };
            if let Some(original) = registry.get(name) {
                let repaired = repair(parsed, original);
                if let Some(v) = validate(&repaired, original) {
                    out.push((name.clone(), v));
                }
            }
        }
    }
    out
}

/// Append a corrective round-trip to the request input.
///
/// Re-emits the violating function calls (as assistant items) plus a
/// corrective `function_call_output` per call, so the model gets one
/// chance to re-issue schema-conforming arguments. The corrective text
/// mirrors what the Anthropic client would have said (`[TOOL_ERROR]
/// ...`) but carries the *specific* violation, so the model can
/// actually act on it.
pub fn append_corrective_input(
    input: &mut crate::responses::Input,
    resp: &crate::responses::ResponsesResponse,
    violations: &[(String, String)],
) {
    let crate::responses::Input::Items(items) = input else {
        return;
    };
    for item in &resp.output {
        if let crate::responses::OutputItem::FunctionCall {
            call_id,
            name,
            arguments,
            ..
        } = item
        {
            items.push(crate::responses::InputItem::FunctionCall {
                call_id: call_id.clone(),
                name: name.clone(),
                arguments: arguments.clone(),
            });
            let violation = violations
                .iter()
                .find(|(n, _)| n == name)
                .map_or("invalid arguments", |(_, v)| v.as_str());
            items.push(crate::responses::InputItem::FunctionCallOutput {
                call_id: call_id.clone(),
                output: format!(
                    "[TOOL_ERROR] Your arguments for `{name}` failed the tool's input \
                     schema: {violation}. Re-issue the call with arguments that conform \
                     to the schema. For optional parameters you have no real value for, \
                     send null (or omit them if the schema allows)."
                ),
            });
        }
    }
}

// ─── tests ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
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
}
