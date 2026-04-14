use indexmap::IndexMap;
use crate::schema::{FieldInfo, InferredSchema, ScalarType};

/// Configuration threaded through every merge call.
#[derive(Debug, Clone, Copy)]
pub struct MergeConfig {
    pub cap_union: usize,
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Compute LUB(a, b) under the type lattice.
/// This is the single most important function in the codebase.
/// It must be:
///   - associative:  LUB(LUB(a,b),c) == LUB(a,LUB(b,c))
///   - commutative:  LUB(a,b) == LUB(b,a)
///   - idempotent:   LUB(a,a) == a
pub fn lub(a: InferredSchema, b: InferredSchema, cfg: MergeConfig) -> InferredSchema {
    use InferredSchema::*;

    match (a, b) {
        // Identity element
        (Never, x) | (x, Never) => x,

        // Top element — absorbs everything
        (Any, _) | (_, Any) => Any,

        // Null seen in isolation — make the other side nullable
        (NullOnly, x) | (x, NullOnly) => x.into_nullable(),

        // Two scalars — same type, numeric promotion, or scalar Union
        (Scalar { ty: ta, nullable: na }, Scalar { ty: tb, nullable: nb }) => {
            let nullable = na || nb;
            match scalar_lub(ta, tb) {
                // Same type or Integer+Float → single scalar
                ScalarLub::Scalar(ty) => Scalar { ty, nullable },
                // Incompatible scalars → Union
                ScalarLub::Union(ta, tb) => {
                    let mut variants = std::collections::BTreeSet::new();
                    variants.insert(ta);
                    variants.insert(tb);
                    union_or_any(variants, nullable, cfg)
                }
            }
        }

        // Two arrays — recursively LUB their item schemas
        (Array { items: ia, nullable: na }, Array { items: ib, nullable: nb }) => {
            Array {
                items: Box::new(lub(*ia, *ib, cfg)),
                nullable: na || nb,
            }
        }

        // Two objects — field-wise recursive LUB
        (Object { fields: fa, nullable: na }, Object { fields: fb, nullable: nb }) => {
            Object {
                fields: merge_fields(fa, fb, cfg),
                nullable: na || nb,
            }
        }

        // Map + Map — LUB the value schemas
        (Map { value_schema: va, nullable: na }, Map { value_schema: vb, nullable: nb }) => {
            Map {
                value_schema: Box::new(lub(*va, *vb, cfg)),
                nullable: na || nb,
            }
        }

        // Object + Map — promote the Object to a Map by merging all its field
        // schemas into the Map's value schema, then LUB with the other Map.
        (Object { fields, nullable: no }, Map { value_schema, nullable: nm })
        | (Map { value_schema, nullable: nm }, Object { fields, nullable: no }) => {
            let merged_value = fields
                .into_values()
                .fold(*value_schema, |acc, fi| lub(acc, fi.schema, cfg));
            Map {
                value_schema: Box::new(merged_value),
                nullable: no || nm,
            }
        }

        // Two scalar Unions — merge variant sets, check cap
        (Union { variants: va, nullable: na }, Union { variants: vb, nullable: nb }) => {
            let mut variants = va;
            variants.extend(vb);
            normalize_numeric(&mut variants);
            let nullable = na || nb;
            union_or_any(variants, nullable, cfg)
        }

        // Scalar + Union — add scalar to union, check cap
        (Scalar { ty, nullable: ns }, Union { variants, nullable: nu })
        | (Union { variants, nullable: nu }, Scalar { ty, nullable: ns }) => {
            let mut variants = variants;
            variants.insert(ty);
            normalize_numeric(&mut variants);
            let nullable = ns || nu;
            union_or_any(variants, nullable, cfg)
        }

        // AnyOf + AnyOf — merge variant vecs, deduplicate structurally, check cap
        (AnyOf { variants: va, nullable: na }, AnyOf { variants: vb, nullable: nb }) => {
            let nullable = na || nb;
            let mut variants = va;
            for v in vb {
                anyof_insert(&mut variants, v, cfg);
            }
            anyof_or_any(variants, nullable, cfg)
        }

        // AnyOf + anything else — absorb the new type into the AnyOf
        (AnyOf { variants, nullable: na }, other)
        | (other, AnyOf { variants: variants, nullable: na }) => {
            let nullable = na || other.is_nullable();
            let mut variants = variants;
            anyof_insert(&mut variants, other, cfg);
            anyof_or_any(variants, nullable, cfg)
        }

        // Structural conflict: two types that are not compatible and neither
        // is already an AnyOf — promote to AnyOf([a, b]).
        // Covers: Object+Scalar, Array+Scalar, Array+Object,
        //         Union+Object, Union+Array, Map+Scalar, etc.
        (a, b) => {
            let nullable = a.is_nullable() || b.is_nullable();
            let variants = vec![a, b];
            anyof_or_any(variants, nullable, cfg)
        }
    }
}

// ---------------------------------------------------------------------------
// Scalar LUB (the number sub-lattice)
// ---------------------------------------------------------------------------

enum ScalarLub {
    /// Same type or numeric promotion — collapse to a single scalar.
    Scalar(ScalarType),
    /// Incomparable types — caller must build a Union.
    Union(ScalarType, ScalarType),
}

/// Returns the LUB of two scalar types.
/// - Same type       → Scalar(same)
/// - Integer + Float → Scalar(Float)  (numeric sub-lattice)
/// - All other pairs → Union(a, b)
fn scalar_lub(a: ScalarType, b: ScalarType) -> ScalarLub {
    use ScalarType::*;
    match (&a, &b) {
        (x, y) if x == y                    => ScalarLub::Scalar(a),
        (Integer, Float) | (Float, Integer) => ScalarLub::Scalar(Float),
        _                                   => ScalarLub::Union(a, b),
    }
}

// ---------------------------------------------------------------------------
// Union (scalar) cap enforcement
// ---------------------------------------------------------------------------

/// If a union contains both Integer and Float, remove Integer (Float subsumes it).
fn normalize_numeric(variants: &mut std::collections::BTreeSet<ScalarType>) {
    if variants.contains(&ScalarType::Float) {
        variants.remove(&ScalarType::Integer);
    }
}

fn union_or_any(
    variants: std::collections::BTreeSet<ScalarType>,
    nullable: bool,
    cfg: MergeConfig,
) -> InferredSchema {
    if cfg.cap_union > 0 && variants.len() > cfg.cap_union {
        InferredSchema::Any
    } else {
        InferredSchema::Union { variants, nullable }
    }
}

// ---------------------------------------------------------------------------
// AnyOf cap enforcement and insertion
// ---------------------------------------------------------------------------

/// Insert `new` into an AnyOf variant list, merging into an existing
/// compatible variant where possible rather than always appending.
///
/// Compatibility rules:
/// - Scalar + existing Scalar  → LUB into a single Scalar or Union
/// - Object + existing Object  → LUB field-wise (already handled upstream,
///   but can arrive here via AnyOf+AnyOf merges)
/// - Array  + existing Array   → LUB items
/// - Everything else           → append as a new distinct variant
fn anyof_insert(variants: &mut Vec<InferredSchema>, new: InferredSchema, cfg: MergeConfig) {
    // Never contributes nothing
    if matches!(new, InferredSchema::Never) {
        return;
    }
    // Any poisons the whole AnyOf — caller will detect via anyof_or_any
    if matches!(new, InferredSchema::Any) {
        variants.push(new);
        return;
    }

    // Try to merge `new` into an existing compatible variant.
    for existing in variants.iter_mut() {
        if can_merge_inline(existing, &new) {
            let merged = lub(
                std::mem::replace(existing, InferredSchema::Never),
                new,
                cfg,
            );
            *existing = merged;
            return;
        }
    }

    // No compatible existing variant — append as new.
    variants.push(new);
}

/// Returns true if `a` and `b` are the same structural category
/// (both Scalar/Union, both Object/Map, both Array) and should be
/// merged inline rather than kept as separate AnyOf arms.
fn can_merge_inline(a: &InferredSchema, b: &InferredSchema) -> bool {
    use InferredSchema::*;
    matches!(
        (a, b),
        (Scalar { .. }, Scalar { .. })
        | (Scalar { .. }, Union { .. })
        | (Union { .. }, Scalar { .. })
        | (Union { .. }, Union { .. })
        | (Object { .. }, Object { .. })
        | (Object { .. }, Map { .. })
        | (Map { .. }, Object { .. })
        | (Map { .. }, Map { .. })
        | (Array { .. }, Array { .. })
    )
}

fn anyof_or_any(
    variants: Vec<InferredSchema>,
    nullable: bool,
    cfg: MergeConfig,
) -> InferredSchema {
    // If Any sneaked in, the whole thing collapses.
    if variants.iter().any(|v| matches!(v, InferredSchema::Any)) {
        return InferredSchema::Any;
    }

    // Deduplicate Never entries that may have been left by anyof_insert.
    let variants: Vec<InferredSchema> = variants
        .into_iter()
        .filter(|v| !matches!(v, InferredSchema::Never))
        .collect();

    // Single variant — unwrap and propagate nullable.
    if variants.len() == 1 {
        return variants.into_iter().next().unwrap().into_nullable_if(nullable);
    }

    // Cap check.
    if cfg.cap_union > 0 && variants.len() > cfg.cap_union {
        return InferredSchema::Any;
    }

    InferredSchema::AnyOf { variants, nullable }
}

// ---------------------------------------------------------------------------
// Object field merge
// ---------------------------------------------------------------------------

/// Merge two field maps via field-wise LUB.
///
/// Fields present in one map but not the other are treated as if the missing
/// side contributed `Never` — which makes them nullable via NullOnly promotion
/// when the other side has seen nulls, or simply present-but-not-always when not.
///
/// We handle "required" tracking via occurrence counts only; we do not emit
/// `required` arrays in JSON Schema output.
fn merge_fields(
    mut a: IndexMap<String, FieldInfo>,
    b: IndexMap<String, FieldInfo>,
    cfg: MergeConfig,
) -> IndexMap<String, FieldInfo> {
    for (key, fb) in b {
        match a.get_mut(&key) {
            Some(fa) => {
                // Field exists in both — LUB the schemas, sum occurrences
                let merged = lub(
                    std::mem::replace(&mut fa.schema, InferredSchema::Never),
                    fb.schema,
                    cfg,
                );
                fa.schema = merged;
                fa.occurrences += fb.occurrences;
            }
            None => {
                // Field only in b — insert as-is
                a.insert(key, fb);
            }
        }
    }
    a
}

// ---------------------------------------------------------------------------
// Helper: conditional nullable promotion
// ---------------------------------------------------------------------------

trait IntoNullableIf {
    fn into_nullable_if(self, nullable: bool) -> Self;
}

impl IntoNullableIf for InferredSchema {
    fn into_nullable_if(self, nullable: bool) -> Self {
        if nullable { self.into_nullable() } else { self }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use InferredSchema::*;

    fn cfg() -> MergeConfig { MergeConfig { cap_union: 5 } }

    #[test]
    fn never_is_identity() {
        let s = Scalar { ty: ScalarType::Str, nullable: false };
        assert!(matches!(lub(Never, s.clone(), cfg()), Scalar { .. }));
        assert!(matches!(lub(s, Never, cfg()), Scalar { .. }));
    }

    #[test]
    fn any_absorbs_all() {
        let s = Scalar { ty: ScalarType::Boolean, nullable: false };
        assert!(matches!(lub(Any, s.clone(), cfg()), Any));
        assert!(matches!(lub(s, Any, cfg()), Any));
    }

    #[test]
    fn null_makes_nullable() {
        let s = Scalar { ty: ScalarType::Integer, nullable: false };
        let result = lub(s, NullOnly, cfg());
        assert!(matches!(result, Scalar { nullable: true, .. }));
    }

    #[test]
    fn integer_float_promotes() {
        let i = Scalar { ty: ScalarType::Integer, nullable: false };
        let f = Scalar { ty: ScalarType::Float,   nullable: false };
        let result = lub(i, f, cfg());
        assert!(matches!(result, Scalar { ty: ScalarType::Float, .. }));
    }

    #[test]
    fn union_cap_collapses_to_any() {
        let mut schema = Never;
        let types = [
            ScalarType::Boolean,
            ScalarType::Integer,
            ScalarType::Float,
            ScalarType::Str,
        ];
        // cap=2 means >2 variants → Any
        let small_cfg = MergeConfig { cap_union: 2 };
        for ty in types {
            schema = lub(schema, Scalar { ty, nullable: false }, small_cfg);
        }
        assert!(matches!(schema, Any));
    }

    #[test]
    fn different_scalars_form_union() {
        let b = Scalar { ty: ScalarType::Boolean, nullable: false };
        let s = Scalar { ty: ScalarType::Str,     nullable: false };
        let result = lub(b, s, cfg());
        assert!(matches!(result, Union { .. }));
        if let Union { ref variants, .. } = result {
            assert!(variants.contains(&ScalarType::Boolean));
            assert!(variants.contains(&ScalarType::Str));
        }
    }

    #[test]
    fn scalar_union_integer_float_normalizes() {
        let mut variants = std::collections::BTreeSet::new();
        variants.insert(ScalarType::Float);
        let u = Union { variants, nullable: false };
        let i = Scalar { ty: ScalarType::Integer, nullable: false };
        let result = lub(u, i, cfg());
        match &result {
            Union { variants, .. } => assert!(!variants.contains(&ScalarType::Integer)),
            Scalar { ty: ScalarType::Float, .. } => {}
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn structural_conflict_produces_anyof() {
        // Object + Scalar should now produce AnyOf, not Any
        let o = Object { fields: indexmap::IndexMap::new(), nullable: false };
        let s = Scalar { ty: ScalarType::Str, nullable: false };
        let result = lub(o, s, cfg());
        assert!(matches!(result, AnyOf { .. }), "expected AnyOf, got {result:?}");
    }

    #[test]
    fn array_scalar_produces_anyof() {
        let a = Array { items: Box::new(Scalar { ty: ScalarType::Integer, nullable: false }), nullable: false };
        let s = Scalar { ty: ScalarType::Str, nullable: false };
        let result = lub(a, s, cfg());
        assert!(matches!(result, AnyOf { .. }), "expected AnyOf, got {result:?}");
    }

    #[test]
    fn anyof_merges_same_category() {
        // AnyOf([String, Object]) + another Object → AnyOf([String, Object])
        // The two Objects should merge field-wise, not append a second Object arm.
        use indexmap::indexmap;
        let o1 = Object {
            fields: indexmap! { "x".to_string() => FieldInfo { schema: Scalar { ty: ScalarType::Integer, nullable: false }, occurrences: 1 } },
            nullable: false,
        };
        let o2 = Object {
            fields: indexmap! { "x".to_string() => FieldInfo { schema: Scalar { ty: ScalarType::Integer, nullable: false }, occurrences: 1 } },
            nullable: false,
        };
        let s = Scalar { ty: ScalarType::Str, nullable: false };

        // Build AnyOf([String, Object(x:int)]) first
        let base = lub(s.clone(), o1, cfg());
        assert!(matches!(base, AnyOf { .. }));

        // Merge another Object — should merge into existing Object arm, not add a third variant
        let result = lub(base, o2, cfg());
        if let AnyOf { variants, .. } = &result {
            assert_eq!(variants.len(), 2, "expected 2 variants, got {}", variants.len());
        } else {
            panic!("expected AnyOf, got {result:?}");
        }
    }

    #[test]
    fn anyof_cap_collapses_to_any() {
        let small_cfg = MergeConfig { cap_union: 2 };
        let s = Scalar { ty: ScalarType::Str, nullable: false };
        let a = Array { items: Box::new(Never), nullable: false };
        let o = Object { fields: indexmap::IndexMap::new(), nullable: false };
        // 3 structural variants > cap=2 → Any
        let step1 = lub(s, a, small_cfg);
        let result = lub(step1, o, small_cfg);
        assert!(matches!(result, Any), "expected Any, got {result:?}");
    }

    #[test]
    fn anyof_nullable_promotion() {
        let s = Scalar { ty: ScalarType::Str, nullable: false };
        let a = Array { items: Box::new(Never), nullable: false };
        let base = lub(s, a, cfg());
        let result = lub(base, NullOnly, cfg());
        assert!(result.is_nullable(), "expected nullable AnyOf");
    }

    #[test]
    fn object_field_merge() {
        use indexmap::indexmap;

        let a = Object {
            fields: indexmap! {
                "x".to_string() => FieldInfo {
                    schema: Scalar { ty: ScalarType::Integer, nullable: false },
                    occurrences: 1,
                }
            },
            nullable: false,
        };
        let b = Object {
            fields: indexmap! {
                "x".to_string() => FieldInfo {
                    schema: Scalar { ty: ScalarType::Integer, nullable: false },
                    occurrences: 1,
                },
                "y".to_string() => FieldInfo {
                    schema: Scalar { ty: ScalarType::Str, nullable: false },
                    occurrences: 1,
                },
            },
            nullable: false,
        };
        let result = lub(a, b, cfg());
        if let Object { fields, .. } = result {
            assert!(fields.contains_key("x"));
            assert!(fields.contains_key("y"));
        } else {
            panic!("expected Object");
        }
    }
}
