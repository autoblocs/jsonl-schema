use indexmap::IndexMap;
use serde_json::Value;
use crate::schema::{FieldInfo, InferredSchema, ScalarType};
use crate::merge::{lub, MergeConfig};

/// Configuration for the inference pass.
#[derive(Debug, Clone, Copy)]
pub struct InferConfig {
    pub max_depth: usize,
    pub merge: MergeConfig,
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Convert a single `serde_json::Value` into an `InferredSchema`.
///
/// `depth` is the *remaining* depth budget (starts at `cfg.max_depth`,
/// decrements on each recursive call). When it hits zero we emit `Any`.
pub fn infer_value(value: &Value, depth: usize, cfg: InferConfig) -> InferredSchema {
    if depth == 0 {
        return InferredSchema::Any;
    }

    match value {
        Value::Null    => InferredSchema::NullOnly,
        Value::Bool(_) => InferredSchema::Scalar { ty: ScalarType::Boolean, nullable: false },
        Value::String(_) => InferredSchema::Scalar { ty: ScalarType::Str,     nullable: false },

        Value::Number(n) => {
            // Prefer Integer when the value is representable as i64 without loss.
            let ty = if n.is_i64() || n.is_u64() {
                ScalarType::Integer
            } else {
                ScalarType::Float
            };
            InferredSchema::Scalar { ty, nullable: false }
        }

        Value::Array(arr) => infer_array(arr, depth - 1, cfg),
        Value::Object(map) => infer_object(map, depth - 1, cfg),
    }
}

// ---------------------------------------------------------------------------
// Array inference
// ---------------------------------------------------------------------------

fn infer_array(arr: &[Value], depth: usize, cfg: InferConfig) -> InferredSchema {
    // Empty array contributes nothing to item type — items stay Never.
    let mut items = InferredSchema::Never;

    for element in arr {
        let elem_schema = infer_value(element, depth, cfg);
        items = lub(items, elem_schema, cfg.merge);
    }

    InferredSchema::Array {
        items: Box::new(items),
        nullable: false,
    }
}

// ---------------------------------------------------------------------------
// Object inference
// ---------------------------------------------------------------------------

fn infer_object(
    map: &serde_json::Map<String, Value>,
    depth: usize,
    cfg: InferConfig,
) -> InferredSchema {
    let mut fields: IndexMap<String, FieldInfo> = IndexMap::new();

    for (key, val) in map {
        let schema = infer_value(val, depth, cfg);
        fields.insert(
            key.clone(),
            FieldInfo { schema, occurrences: 1 },
        );
    }

    InferredSchema::Object { fields, nullable: false }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg() -> InferConfig {
        InferConfig {
            max_depth: 20,
            merge: MergeConfig { cap_union: 5 },
        }
    }

    fn infer(v: Value) -> InferredSchema {
        infer_value(&v, cfg().max_depth, cfg())
    }

    #[test]
    fn null_infers_null_only() {
        assert!(matches!(infer(json!(null)), InferredSchema::NullOnly));
    }

    #[test]
    fn bool_infers_boolean() {
        assert!(matches!(
            infer(json!(true)),
            InferredSchema::Scalar { ty: ScalarType::Boolean, .. }
        ));
    }

    #[test]
    fn integer_infers_integer() {
        assert!(matches!(
            infer(json!(42)),
            InferredSchema::Scalar { ty: ScalarType::Integer, .. }
        ));
    }

    #[test]
    fn float_infers_float() {
        assert!(matches!(
            infer(json!(3.14)),
            InferredSchema::Scalar { ty: ScalarType::Float, .. }
        ));
    }

    #[test]
    fn empty_array_has_never_items() {
        let s = infer(json!([]));
        if let InferredSchema::Array { items, .. } = s {
            assert!(matches!(*items, InferredSchema::Never));
        } else {
            panic!("expected Array");
        }
    }

    #[test]
    fn homogeneous_array_infers_item_type() {
        let s = infer(json!([1, 2, 3]));
        if let InferredSchema::Array { items, .. } = s {
            assert!(matches!(*items, InferredSchema::Scalar { ty: ScalarType::Integer, .. }));
        } else {
            panic!("expected Array");
        }
    }

    #[test]
    fn object_fields_inferred() {
        let s = infer(json!({ "name": "alice", "age": 30 }));
        if let InferredSchema::Object { fields, .. } = s {
            assert!(matches!(
                fields["name"].schema,
                InferredSchema::Scalar { ty: ScalarType::Str, .. }
            ));
            assert!(matches!(
                fields["age"].schema,
                InferredSchema::Scalar { ty: ScalarType::Integer, .. }
            ));
        } else {
            panic!("expected Object");
        }
    }

    #[test]
    fn depth_zero_emits_any() {
        let s = infer_value(&json!({ "x": 1 }), 0, cfg());
        assert!(matches!(s, InferredSchema::Any));
    }

    #[test]
    fn nested_object_respects_depth() {
        // depth=1 means the object itself is fine, but its children get depth=0
        let s = infer_value(&json!({ "inner": { "x": 1 } }), 1, cfg());
        if let InferredSchema::Object { fields, .. } = s {
            // inner was inferred at depth=0, so it should be Any
            assert!(matches!(fields["inner"].schema, InferredSchema::Any));
        } else {
            panic!("expected Object");
        }
    }
}
