//! JSON-Schema → Gemini schema projection.
//!
//! Port of opencode `packages/llm/src/protocols/utils/gemini-tool-schema.ts`.
//! Tool-schema conversion has two distinct concerns:
//!
//! 1. **Sanitize** ([`sanitize_node`]) — fix common authoring mistakes Gemini
//!    rejects: integer/number enums (must be strings), `required` entries
//!    that don't match a property, untyped arrays (`items` must be present),
//!    and `properties`/`required` keys on non-object scalars.
//! 2. **Project** ([`project_node`]) — lossy mapping from JSON Schema to
//!    Gemini's schema dialect: drop empty objects, derive `nullable: true`
//!    from `type: [..., "null"]`, coerce `const` to `[const]` enum, recurse
//!    properties/items, and propagate only an allowlisted set of keys
//!    (`description`, `required`, `format`, `type`, `properties`, `items`,
//!    `allOf`, `anyOf`, `oneOf`, `minLength`). Anything outside the allowlist
//!    (e.g. `additionalProperties`, `$ref`) is silently dropped.
//!
//! Sanitize runs first, then project ([`convert`]).

use std::collections::HashSet;

use serde_json::{Map, Value};

/// Keys whose presence signals a schema node carries real validation intent
/// (used to decide whether a bare `items: {}` should be defaulted to
/// `{ type: "string" }`).
const SCHEMA_INTENT_KEYS: [&str; 14] = [
    "type",
    "properties",
    "items",
    "prefixItems",
    "enum",
    "const",
    "$ref",
    "additionalProperties",
    "patternProperties",
    "required",
    "not",
    "if",
    "then",
    "else",
];

/// Whether `map` carries a `anyOf`/`oneOf`/`allOf` combinator.
fn has_combiner(map: &Map<String, Value>) -> bool {
    ["anyOf", "oneOf", "allOf"]
        .iter()
        .any(|key| matches!(map.get(*key), Some(Value::Array(_))))
}

/// Whether `map` carries any key that signals real schema intent.
fn has_schema_intent(map: &Map<String, Value>) -> bool {
    has_combiner(map) || SCHEMA_INTENT_KEYS.iter().any(|key| map.contains_key(*key))
}

/// `String(value)`-style rendering for a JSON-Schema `enum` member.
fn sanitize_enum_value(value: &Value) -> Value {
    let rendered = match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => "null".to_string(),
        other => serde_json::to_string(other).unwrap_or_default(),
    };
    Value::String(rendered)
}

/// Sanitize one schema node. Port of `sanitizeNode`
/// (gemini-tool-schema.ts:29-57).
fn sanitize_node(schema: &Value) -> Value {
    let obj = match schema {
        Value::Array(items) => return Value::Array(items.iter().map(sanitize_node).collect()),
        Value::Object(obj) => obj,
        other => return other.clone(),
    };

    let mut result: Map<String, Value> = obj
        .iter()
        .map(|(key, value)| {
            let sanitized = if key == "enum" {
                match value {
                    Value::Array(items) => {
                        Value::Array(items.iter().map(sanitize_enum_value).collect())
                    }
                    other => sanitize_node(other),
                }
            } else {
                sanitize_node(value)
            };
            (key.clone(), sanitized)
        })
        .collect();

    // Integer/number enums must be re-typed as string (Gemini only accepts
    // string enums).
    if matches!(result.get("enum"), Some(Value::Array(_)))
        && matches!(
            result.get("type").and_then(Value::as_str),
            Some("integer") | Some("number")
        )
    {
        result.insert("type".to_string(), Value::String("string".to_string()));
    }

    // Drop `required` entries that don't name an actual property.
    let type_is_object = result.get("type").and_then(Value::as_str) == Some("object");
    if type_is_object {
        let allowed: Option<HashSet<String>> = result
            .get("properties")
            .and_then(Value::as_object)
            .map(|props| props.keys().cloned().collect());
        if let (Some(allowed), Some(Value::Array(required))) =
            (allowed, result.get("required").cloned())
        {
            let filtered: Vec<Value> = required
                .into_iter()
                .filter(|field| field.as_str().is_some_and(|s| allowed.contains(s)))
                .collect();
            result.insert("required".to_string(), Value::Array(filtered));
        }
    }

    // Arrays must carry `items`; default an untyped `items` to `{ type: "string" }`.
    let type_is_array = result.get("type").and_then(Value::as_str) == Some("array");
    if type_is_array && !has_combiner(&result) {
        let items = result
            .remove("items")
            .unwrap_or_else(|| Value::Object(Map::new()));
        let items = match items {
            Value::Object(items_obj) if !has_schema_intent(&items_obj) => {
                let mut items_obj = items_obj;
                items_obj.insert("type".to_string(), Value::String("string".to_string()));
                Value::Object(items_obj)
            }
            other => other,
        };
        result.insert("items".to_string(), items);
    }

    // Non-object scalar types cannot carry `properties`/`required`.
    let type_str = result
        .get("type")
        .and_then(Value::as_str)
        .map(str::to_string);
    if let Some(type_str) = type_str
        && type_str != "object"
        && !has_combiner(&result)
    {
        result.remove("properties");
        result.remove("required");
    }

    Value::Object(result)
}

/// JS-style falsy check (`undefined`/`null`/`false`/`0`/`""` are falsy;
/// arrays and objects are always truthy).
fn is_falsy(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Bool(b) => !b,
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::String(s) => s.is_empty(),
        Value::Array(_) | Value::Object(_) => false,
    }
}

/// Whether `schema` is a bare, unconstrained object schema Gemini rejects.
/// Port of `emptyObjectSchema` (gemini-tool-schema.ts:59-62).
fn is_empty_object_schema(obj: &Map<String, Value>) -> bool {
    if obj.get("type").and_then(Value::as_str) != Some("object") {
        return false;
    }
    let properties_empty = match obj.get("properties") {
        Some(Value::Object(props)) => props.is_empty(),
        _ => true,
    };
    let additional_properties_falsy = obj.get("additionalProperties").is_none_or(is_falsy);
    properties_empty && additional_properties_falsy
}

/// Project one sanitized schema node down to the Gemini-accepted key
/// allowlist. Port of `projectNode` (gemini-tool-schema.ts:64-95).
fn project_node(schema: &Value) -> Option<Value> {
    let obj = schema.as_object()?;
    if is_empty_object_schema(obj) {
        return None;
    }

    let mut result = Map::new();

    if let Some(v) = obj.get("description") {
        result.insert("description".to_string(), v.clone());
    }
    if let Some(v) = obj.get("required") {
        result.insert("required".to_string(), v.clone());
    }
    if let Some(v) = obj.get("format") {
        result.insert("format".to_string(), v.clone());
    }

    match obj.get("type") {
        Some(Value::Array(types)) => {
            if let Some(first) = types.iter().find(|t| t.as_str() != Some("null")) {
                result.insert("type".to_string(), first.clone());
            }
            if types.iter().any(|t| t.as_str() == Some("null")) {
                result.insert("nullable".to_string(), Value::Bool(true));
            }
        }
        Some(other) => {
            result.insert("type".to_string(), other.clone());
        }
        None => {}
    }

    if let Some(constant) = obj.get("const") {
        result.insert("enum".to_string(), Value::Array(vec![constant.clone()]));
    } else if let Some(e) = obj.get("enum") {
        result.insert("enum".to_string(), e.clone());
    }

    if let Some(Value::Object(props)) = obj.get("properties") {
        let projected: Map<String, Value> = props
            .iter()
            .filter_map(|(k, v)| project_node(v).map(|pv| (k.clone(), pv)))
            .collect();
        result.insert("properties".to_string(), Value::Object(projected));
    }

    match obj.get("items") {
        Some(Value::Array(items)) => {
            let projected: Vec<Value> = items
                .iter()
                .map(|item| project_node(item).unwrap_or(Value::Null))
                .collect();
            result.insert("items".to_string(), Value::Array(projected));
        }
        Some(other) => {
            if let Some(p) = project_node(other) {
                result.insert("items".to_string(), p);
            }
        }
        None => {}
    }

    for key in ["allOf", "anyOf", "oneOf"] {
        if let Some(Value::Array(items)) = obj.get(key) {
            let projected: Vec<Value> = items
                .iter()
                .map(|item| project_node(item).unwrap_or(Value::Null))
                .collect();
            result.insert(key.to_string(), Value::Array(projected));
        }
    }

    if let Some(v) = obj.get("minLength") {
        result.insert("minLength".to_string(), v.clone());
    }

    Some(Value::Object(result))
}

/// Sanitize + project a JSON Schema into Gemini's accepted schema dialect.
/// Port of `convert` (gemini-tool-schema.ts:97). Returns `Value::Null` when
/// the schema is degenerate (e.g. a bare `{ type: "object" }` with no
/// properties) — callers treat that as "no parameters".
#[must_use]
pub fn convert(schema: &Value) -> Value {
    project_node(&sanitize_node(schema)).unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn numeric_enum_is_retyped_as_string() {
        let schema = json!({"type": "integer", "enum": [1, 2, 3]});
        let converted = convert(&schema);
        assert_eq!(converted["type"], "string");
        assert_eq!(converted["enum"], json!(["1", "2", "3"]));
    }

    #[test]
    fn array_without_items_defaults_to_string_items() {
        let schema = json!({"type": "array"});
        let converted = convert(&schema);
        assert_eq!(converted["items"], json!({"type": "string"}));
    }

    #[test]
    fn required_field_not_in_properties_is_dropped() {
        let schema = json!({
            "type": "object",
            "properties": {"a": {"type": "string"}},
            "required": ["a", "ghost"]
        });
        let converted = convert(&schema);
        assert_eq!(converted["required"], json!(["a"]));
    }

    #[test]
    fn empty_object_schema_projects_to_null() {
        let schema = json!({"type": "object"});
        assert!(convert(&schema).is_null());
    }

    #[test]
    fn nullable_union_type_becomes_nullable_flag() {
        let schema = json!({"type": ["string", "null"]});
        let converted = convert(&schema);
        assert_eq!(converted["type"], "string");
        assert_eq!(converted["nullable"], true);
    }

    #[test]
    fn additional_properties_and_ref_are_dropped() {
        let schema = json!({
            "type": "object",
            "properties": {"a": {"type": "string"}},
            "additionalProperties": false,
            "$ref": "#/definitions/x"
        });
        let converted = convert(&schema);
        assert!(converted.get("additionalProperties").is_none());
        assert!(converted.get("$ref").is_none());
    }
}
