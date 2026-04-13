use serde_json::{json, Map, Value};
use crate::schema::{InferredSchema, ScalarType};

/// Convert an `InferredSchema` into a JSON Schema (draft-07) `Value`.
///
/// Design decisions:
/// - `Never`   → `{}` (accepts anything — no observations is not a constraint)
/// - `Any`     → `{}` (same semantics)
/// - `NullOnly` → `{ "type": "null" }`
/// - Nullable types → `{ "type": ["x", "null"] }` (preferred over anyOf for scalars)
/// - `Union`   → `{ "anyOf": [...] }` with null folded into each variant's type array
///                if all variants are scalar; otherwise `anyOf`
/// - `Object`  → `{ "type": "object", "properties": {...} }`
///               `required` array is intentionally omitted per spec
/// - `Map`     → `{ "type": "object", "additionalProperties": {...} }`
/// - `Array`   → `{ "type": "array", "items": {...} }`
///               items=Never (empty array observed) → omit `items` key
pub fn to_json_schema(schema: &InferredSchema) -> Value {
    emit(schema)
}

fn emit(schema: &InferredSchema) -> Value {
    match schema {
        InferredSchema::Never => json!({}),
        InferredSchema::Any   => json!({}),

        InferredSchema::NullOnly => json!({ "type": "null" }),

        InferredSchema::Scalar { ty, nullable } => emit_scalar(ty, *nullable),

        InferredSchema::Array { items, nullable } => emit_array(items, *nullable),

        InferredSchema::Object { fields, nullable } => emit_object(fields, *nullable),

        InferredSchema::Map { value_schema, nullable } => emit_map(value_schema, *nullable),

        InferredSchema::Union { variants, nullable } => emit_union(variants, *nullable),
    }
}

// ---------------------------------------------------------------------------
// Scalar
// ---------------------------------------------------------------------------

fn emit_scalar(ty: &ScalarType, nullable: bool) -> Value {
    let type_str = ty.json_schema_type();
    if nullable {
        json!({ "type": [type_str, "null"] })
    } else {
        json!({ "type": type_str })
    }
}

// ---------------------------------------------------------------------------
// Array
// ---------------------------------------------------------------------------

fn emit_array(items: &InferredSchema, nullable: bool) -> Value {
    let mut obj = Map::new();

    if nullable {
        obj.insert("type".into(), json!(["array", "null"]));
    } else {
        obj.insert("type".into(), json!("array"));
    }

    // Only emit `items` if we actually observed elements.
    // Never means only empty arrays were seen — omitting `items` is correct.
    match items {
        InferredSchema::Never => {}
        _ => { obj.insert("items".into(), emit(items)); }
    }

    Value::Object(obj)
}

// ---------------------------------------------------------------------------
// Object
// ---------------------------------------------------------------------------

fn emit_object(
    fields: &indexmap::IndexMap<String, crate::schema::FieldInfo>,
    nullable: bool,
) -> Value {
    let mut obj = Map::new();

    if nullable {
        obj.insert("type".into(), json!(["object", "null"]));
    } else {
        obj.insert("type".into(), json!("object"));
    }

    if !fields.is_empty() {
        let mut properties = Map::new();
        for (key, info) in fields {
            properties.insert(key.clone(), emit(&info.schema));
        }
        obj.insert("properties".into(), Value::Object(properties));
    }

    // additionalProperties intentionally omitted — we never want to reject
    // fields we haven't seen. The inferred schema is descriptive, not prescriptive.

    Value::Object(obj)
}

// ---------------------------------------------------------------------------
// Map
// ---------------------------------------------------------------------------

fn emit_map(value_schema: &InferredSchema, nullable: bool) -> Value {
    let mut obj = Map::new();

    if nullable {
        obj.insert("type".into(), json!(["object", "null"]));
    } else {
        obj.insert("type".into(), json!("object"));
    }

    obj.insert("additionalProperties".into(), emit(value_schema));
    Value::Object(obj)
}

// ---------------------------------------------------------------------------
// Union
// ---------------------------------------------------------------------------

fn emit_union(
    variants: &std::collections::BTreeSet<ScalarType>,
    nullable: bool,
) -> Value {
    // Special case: single variant — not really a union
    if variants.len() == 1 {
        return emit_scalar(variants.iter().next().unwrap(), nullable);
    }

    // All scalars — emit as { "type": ["a", "b", ...] } which is valid JSON Schema
    let mut types: Vec<Value> = variants
        .iter()
        .map(|ty| Value::String(ty.json_schema_type().to_string()))
        .collect();

    if nullable {
        types.push(Value::String("null".to_string()));
    }

    json!({ "type": types })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{FieldInfo, InferredSchema, ScalarType};
    use indexmap::indexmap;
    use std::collections::BTreeSet;

    #[test]
    fn never_emits_empty_schema() {
        let v = to_json_schema(&InferredSchema::Never);
        assert_eq!(v, json!({}));
    }

    #[test]
    fn scalar_string() {
        let v = to_json_schema(&InferredSchema::Scalar { ty: ScalarType::Str, nullable: false });
        assert_eq!(v, json!({ "type": "string" }));
    }

    #[test]
    fn nullable_integer() {
        let v = to_json_schema(&InferredSchema::Scalar { ty: ScalarType::Integer, nullable: true });
        assert_eq!(v, json!({ "type": ["integer", "null"] }));
    }

    #[test]
    fn empty_array_no_items_key() {
        let v = to_json_schema(&InferredSchema::Array {
            items: Box::new(InferredSchema::Never),
            nullable: false,
        });
        assert!(!v.as_object().unwrap().contains_key("items"));
    }

    #[test]
    fn array_with_items() {
        let v = to_json_schema(&InferredSchema::Array {
            items: Box::new(InferredSchema::Scalar { ty: ScalarType::Str, nullable: false }),
            nullable: false,
        });
        assert_eq!(v["items"], json!({ "type": "string" }));
    }

    #[test]
    fn object_properties() {
        let v = to_json_schema(&InferredSchema::Object {
            fields: indexmap! {
                "name".to_string() => FieldInfo {
                    schema: InferredSchema::Scalar { ty: ScalarType::Str, nullable: false },
                    occurrences: 1,
                }
            },
            nullable: false,
        });
        assert_eq!(v["type"], json!("object"));
        assert_eq!(v["properties"]["name"]["type"], json!("string"));
    }

    #[test]
    fn union_two_scalars() {
        let mut variants = BTreeSet::new();
        variants.insert(ScalarType::Str);
        variants.insert(ScalarType::Integer);
        let v = to_json_schema(&InferredSchema::Union { variants, nullable: false });
        // BTreeSet ordering: Boolean < Float < Integer < Str
        let types = v["type"].as_array().unwrap();
        assert!(types.contains(&json!("string")));
        assert!(types.contains(&json!("integer")));
    }

    #[test]
    fn union_with_null() {
        let mut variants = BTreeSet::new();
        variants.insert(ScalarType::Str);
        let v = to_json_schema(&InferredSchema::Union { variants, nullable: true });
        // Single variant + nullable → scalar form
        assert_eq!(v, json!({ "type": ["string", "null"] }));
    }
}