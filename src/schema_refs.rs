//! Resolve local JSON Schema `$ref`s in a tool's `input_schema`.
//!
//! MCP servers (and Pydantic / Zod generators in general) describe shared
//! types once under `$defs` and point at them with `$ref`, usually with an
//! annotation beside the reference:
//!
//! ```json
//! "address": { "$ref": "#/$defs/Address", "description": "Where to look" }
//! ```
//!
//! The Responses strict validator refuses a `$ref` that carries any sibling
//! keyword (`$ref cannot have keywords {'description'}`), and rejects the
//! whole request when one tool does. Rather than patching every sanitizer
//! rule to tolerate `$ref`, this module removes the references up front so
//! the sanitizer and [`crate::repair`] both see the real, fully-typed
//! schema. That also avoids a worse bug: the sanitizer used to see a `$ref`
//! node as typeless and add a `type` beside it that never came from the
//! definition.
//!
//! # Behaviour
//!
//! - Every `$ref` into the same document (`#`, `#/$defs/X`,
//!   `#/definitions/X`, or any other JSON Pointer) is replaced by a copy of
//!   its target, resolved recursively. `~0`, `~1` and percent-escapes in the
//!   pointer are decoded.
//! - Sibling keywords are merged onto the copy. Annotations (`description`,
//!   `title`, `default`, ...) from the reference site win, because they are
//!   the more specific statement. A validation keyword that exists only
//!   beside the `$ref` is added. When the sibling and the target both define
//!   a validation keyword differently, the target's value is kept: JSON
//!   Schema would apply both, strict mode cannot express the conjunction
//!   (`allOf` is refused), and keeping one side only relaxes the schema, so
//!   it can never reject a value the client's schema accepted.
//! - A reference that would loop (a recursive definition) is kept as a bare
//!   `{"$ref": ...}` pointing at a retained `$defs` entry. OpenAI documents
//!   that form as supported for recursive schemas. Siblings on such a
//!   reference are dropped, since the validator refuses them.
//! - Root `$defs` / `definitions` are removed unless a recursive definition
//!   still needs them.
//! - Data keywords (`enum`, `const`, `default`, `examples`, `example`) are
//!   never walked: their contents are values, not schemas.
//!
//! Anything this module cannot resolve faithfully - a remote reference, a
//! missing target, a cycle through a pointer that cannot be retained, or a
//! schema whose expansion would explode - leaves the schema **unchanged**
//! with a warning, so the upstream's own error names the real problem. It
//! never guesses and never fetches anything.

use serde_json::{Map, Value, json};
use std::collections::BTreeSet;

/// Upper bound on `$ref` expansions for one tool. Inlining a diamond-shaped
/// set of definitions is exponential in the depth of the sharing, so a
/// pathological schema is refused rather than allowed to exhaust memory.
const MAX_EXPANSIONS: usize = 20_000;

/// Keywords whose value is documentation. On a `$ref` site they override the
/// same keyword on the target.
const ANNOTATION_KEYWORDS: &[&str] = &[
    "description",
    "title",
    "default",
    "examples",
    "example",
    "$comment",
    "deprecated",
    "readOnly",
    "writeOnly",
];

/// Keywords whose value is data rather than a schema.
const DATA_KEYWORDS: &[&str] = &["enum", "const", "default", "examples", "example"];

/// Keywords whose value is a map of arbitrary names to schemas. Their direct
/// children are schemas even when the name collides with a keyword.
const SCHEMA_MAP_KEYWORDS: &[&str] = &[
    "properties",
    "patternProperties",
    "$defs",
    "definitions",
    "dependentSchemas",
];

/// Why a schema could not be resolved. Only used for the warning; the caller
/// falls back to the untouched schema.
#[derive(Debug)]
enum RefError {
    /// Not a reference into this document (e.g. `https://...`).
    NotLocal(String),
    /// The pointer does not lead anywhere in this document.
    Missing(String),
    /// A recursive reference whose target cannot be kept as a `$defs` entry.
    UnretainableCycle(String),
    /// Expansion exceeded [`MAX_EXPANSIONS`].
    TooLarge,
}

impl std::fmt::Display for RefError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotLocal(r) => write!(f, "`{r}` is not a reference into this schema"),
            Self::Missing(r) => write!(f, "`{r}` does not resolve to anything in this schema"),
            Self::UnretainableCycle(r) => {
                write!(
                    f,
                    "`{r}` is recursive but is not a `$defs`/`definitions` entry"
                )
            }
            Self::TooLarge => write!(
                f,
                "expanding the references would exceed {MAX_EXPANSIONS} copies"
            ),
        }
    }
}

/// Resolve the local `$ref`s in a tool's `input_schema`. See the module docs.
///
/// Returns the schema unchanged when it has no references and no root
/// definitions, and also when it cannot be resolved faithfully.
pub fn resolve_local_refs(schema: Value) -> Value {
    let has_root_defs = schema
        .as_object()
        .is_some_and(|m| m.contains_key("$defs") || m.contains_key("definitions"));
    if !has_root_defs && !contains_ref(&schema) {
        return schema;
    }

    let mut resolver = Resolver {
        root: &schema,
        expansions: 0,
        retained: BTreeSet::new(),
    };
    match resolver.resolve() {
        Ok(resolved) => {
            tracing::debug!(
                inlined = resolver.expansions,
                retained = resolver.retained.len(),
                "resolved `$ref`s in tool schema"
            );
            resolved
        }
        Err(err) => {
            tracing::warn!(
                error = %err,
                "leaving `$ref`s in a tool schema unresolved; the Responses strict validator \
                 refuses `$ref` with sibling keywords, so the upstream will likely reject it. \
                 See src/schema_refs.rs."
            );
            schema
        }
    }
}

/// True if any schema node carries a string `$ref`.
fn contains_ref(v: &Value) -> bool {
    match v {
        Value::Object(map) => {
            matches!(map.get("$ref"), Some(Value::String(_)))
                || map
                    .iter()
                    .any(|(k, child)| !DATA_KEYWORDS.contains(&k.as_str()) && contains_ref(child))
        }
        Value::Array(items) => items.iter().any(contains_ref),
        _ => false,
    }
}

struct Resolver<'a> {
    /// The untouched document every pointer is resolved against.
    root: &'a Value,
    expansions: usize,
    /// Recursive references that must stay behind as `$defs` entries.
    retained: BTreeSet<String>,
}

impl<'a> Resolver<'a> {
    fn resolve(&mut self) -> Result<Value, RefError> {
        let Value::Object(root) = self.root else {
            return Ok(self.root.clone());
        };
        let mut out = self.walk_schema(root, &mut Vec::new(), true)?;

        // Emit each retained definition. Resolving one can retain another,
        // so drain a worklist rather than iterating a snapshot.
        let mut emitted: BTreeSet<String> = BTreeSet::new();
        while let Some(reference) = self
            .retained
            .iter()
            .find(|r| !emitted.contains(*r))
            .cloned()
        {
            emitted.insert(reference.clone());
            let Some((container, name)) = def_entry(&reference) else {
                // "#" - the root itself stays where it is.
                continue;
            };
            let target = self.lookup(&reference)?;
            let body = self.walk_value(target, &mut vec![reference.clone()])?;
            if let Value::Object(map) = &mut out {
                let defs = map
                    .entry(container)
                    .or_insert_with(|| Value::Object(Map::new()));
                if let Value::Object(defs) = defs {
                    defs.insert(name, body);
                }
            }
        }
        Ok(out)
    }

    /// Walk one schema object. `is_root` drops the root's own definitions.
    fn walk_schema(
        &mut self,
        map: &Map<String, Value>,
        stack: &mut Vec<String>,
        is_root: bool,
    ) -> Result<Value, RefError> {
        if let Some(Value::String(reference)) = map.get("$ref") {
            return self.expand(map, reference, stack, is_root);
        }
        let mut out = Map::new();
        for (key, value) in map {
            if is_root && (key == "$defs" || key == "definitions") {
                continue;
            }
            let walked = if DATA_KEYWORDS.contains(&key.as_str()) {
                value.clone()
            } else if SCHEMA_MAP_KEYWORDS.contains(&key.as_str()) {
                self.walk_schema_map(value, stack)?
            } else {
                self.walk_value(value, stack)?
            };
            out.insert(key.clone(), walked);
        }
        Ok(Value::Object(out))
    }

    /// Walk a map whose children are schemas keyed by arbitrary names.
    fn walk_schema_map(
        &mut self,
        value: &Value,
        stack: &mut Vec<String>,
    ) -> Result<Value, RefError> {
        let Value::Object(entries) = value else {
            return self.walk_value(value, stack);
        };
        let mut out = Map::new();
        for (name, child) in entries {
            out.insert(name.clone(), self.walk_value(child, stack)?);
        }
        Ok(Value::Object(out))
    }

    fn walk_value(&mut self, value: &Value, stack: &mut Vec<String>) -> Result<Value, RefError> {
        match value {
            Value::Object(map) => self.walk_schema(map, stack, false),
            Value::Array(items) => items
                .iter()
                .map(|item| self.walk_value(item, stack))
                .collect::<Result<Vec<_>, _>>()
                .map(Value::Array),
            other => Ok(other.clone()),
        }
    }

    /// Replace one `$ref` node with its resolved target plus siblings.
    fn expand(
        &mut self,
        node: &Map<String, Value>,
        reference: &str,
        stack: &mut Vec<String>,
        is_root: bool,
    ) -> Result<Value, RefError> {
        if stack.iter().any(|seen| seen == reference) {
            // Recursive: keep a bare reference to a retained definition.
            self.retain(reference)?;
            return Ok(json!({ "$ref": reference }));
        }

        self.expansions += 1;
        if self.expansions > MAX_EXPANSIONS {
            return Err(RefError::TooLarge);
        }

        let target = self.lookup(reference)?;
        stack.push(reference.to_owned());
        let resolved = match target {
            Value::Object(target_map) => {
                self.walk_schema(target_map, stack, is_pointer_to_root(reference))
            }
            other => self.walk_value(other, stack),
        };
        // Siblings are resolved in the same recursion context as the target.
        let mut sibling_map = Map::new();
        for (key, value) in node {
            if key == "$ref" || (is_root && (key == "$defs" || key == "definitions")) {
                continue;
            }
            sibling_map.insert(key.clone(), value.clone());
        }
        let siblings = if sibling_map.is_empty() {
            Ok(Value::Object(sibling_map))
        } else {
            self.walk_schema(&sibling_map, stack, false)
        };
        stack.pop();

        Ok(merge_siblings(resolved?, siblings?))
    }

    /// Record that `reference` must survive as a `$defs` entry.
    fn retain(&mut self, reference: &str) -> Result<(), RefError> {
        if is_pointer_to_root(reference) || def_entry(reference).is_some() {
            self.retained.insert(reference.to_owned());
            Ok(())
        } else {
            Err(RefError::UnretainableCycle(reference.to_owned()))
        }
    }

    /// Resolve a local JSON Pointer reference against the root document.
    ///
    /// The result borrows from the document, not from the resolver, so the
    /// caller can keep it while mutating the resolver's own state.
    fn lookup(&self, reference: &str) -> Result<&'a Value, RefError> {
        let Some(fragment) = reference.strip_prefix('#') else {
            return Err(RefError::NotLocal(reference.to_owned()));
        };
        let fragment = percent_decode(fragment);
        if fragment.is_empty() {
            return Ok(self.root);
        }
        let Some(pointer) = fragment.strip_prefix('/') else {
            // `#anchor` style references are not supported.
            return Err(RefError::NotLocal(reference.to_owned()));
        };
        let mut node = self.root;
        for segment in pointer.split('/') {
            let key = segment.replace("~1", "/").replace("~0", "~");
            node = match node {
                Value::Object(map) => map.get(&key),
                Value::Array(items) => key.parse::<usize>().ok().and_then(|i| items.get(i)),
                _ => None,
            }
            .ok_or_else(|| RefError::Missing(reference.to_owned()))?;
        }
        Ok(node)
    }
}

/// Merge the siblings of a `$ref` onto its resolved target.
fn merge_siblings(resolved: Value, siblings: Value) -> Value {
    let (mut target, siblings) = match (resolved, siblings) {
        (Value::Object(target), Value::Object(siblings)) => (target, siblings),
        // A boolean schema target has nowhere to put siblings.
        (resolved, _) => return resolved,
    };
    if target.contains_key("$ref") {
        // The target is itself a retained recursive reference, which may not
        // carry siblings.
        if !siblings.is_empty() {
            tracing::debug!(
                "dropping keywords beside a recursive `$ref`; the validator refuses them"
            );
        }
        return Value::Object(target);
    }
    for (key, value) in siblings {
        if ANNOTATION_KEYWORDS.contains(&key.as_str()) {
            target.insert(key, value);
            continue;
        }
        match target.get(&key) {
            None => {
                target.insert(key, value);
            }
            Some(existing) if *existing == value => {}
            Some(_) => tracing::debug!(
                keyword = %key,
                "keyword beside `$ref` conflicts with its target; keeping the target's value"
            ),
        }
    }
    Value::Object(target)
}

/// `#` (or an empty fragment) addresses the whole document.
fn is_pointer_to_root(reference: &str) -> bool {
    reference == "#"
}

/// Split `#/$defs/Name` or `#/definitions/Name` into the container key and
/// the decoded entry name. Anything deeper or elsewhere is not retainable.
fn def_entry(reference: &str) -> Option<(String, String)> {
    let fragment = percent_decode(reference.strip_prefix('#')?);
    let mut segments = fragment.strip_prefix('/')?.split('/');
    let container = segments.next()?;
    let name = segments.next()?;
    if segments.next().is_some() || !(container == "$defs" || container == "definitions") {
        return None;
    }
    Some((
        container.to_owned(),
        name.replace("~1", "/").replace("~0", "~"),
    ))
}

/// Decode `%XX` escapes. Invalid escapes are left as written.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3])
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
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
}
