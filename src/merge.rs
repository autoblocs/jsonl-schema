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

        // Two scalars — same type, numeric promotion, or true union
        (Scalar { ty: ta, nullable: na }, Scalar { ty: tb, nullable: nb }) => {
            let nullable = na || nb;
            match scalar_lub(ta, tb) {
                // Same type or Integer+Float → single scalar
                ScalarLub::Scalar(ty) => Scalar { ty, nullable },
                // Incompatible types → start a Union
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

        // Two unions — merge variant sets, check cap
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

        // Object + Array, Object + Scalar, Array + Scalar — structural conflict.
        // Any is the only sound upper bound. Nullability is irrelevant since
        // Any already accepts null implicitly in our lattice.
        _ => Any,
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
// Union cap enforcement
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
    if variants.len() > cfg.cap_union {
        InferredSchema::Any
    } else {
        InferredSchema::Union { variants, nullable }
    }
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
        // Union already has Float; adding Integer should stay Float scalar
        let mut variants = std::collections::BTreeSet::new();
        variants.insert(ScalarType::Float);
        let u = Union { variants, nullable: false };
        let i = Scalar { ty: ScalarType::Integer, nullable: false };
        let result = lub(u, i, cfg());
        // Integer is subsumed by Float → should remain a single-variant Union
        // or collapse to Scalar(Float) — either way Integer must not appear
        match &result {
            Union { variants, .. } => assert!(!variants.contains(&ScalarType::Integer)),
            Scalar { ty: ScalarType::Float, .. } => {}
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn structural_conflict_collapses_to_any() {
        let o = Object { fields: indexmap::IndexMap::new(), nullable: false };
        let s = Scalar { ty: ScalarType::Str, nullable: false };
        assert!(matches!(lub(o, s, cfg()), Any));
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
