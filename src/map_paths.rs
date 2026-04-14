//! Post-inference map-path override.
//!
//! After the full schema has been inferred, selected object nodes can be
//! forcibly converted to Map (additionalProperties) regardless of key count.
//!
//! # Path syntax
//!
//! Paths are dot-separated field names, with `[]` to descend into array items:
//!
//! ```text
//! snapshot.trackedFileBackups
//! data.normalizedMessages[].message
//! ```
//!
//! Paths are matched against the `InferredSchema` tree. If the node at the
//! path is an `Object`, it is converted to a `Map` whose value schema is the
//! LUB of all its field schemas. If the node is already a `Map`, or is `Any`,
//! it is left as-is. Any other node type is silently ignored (the path may
//! refer to a field that is absent in some files).

use crate::merge::{lub, MergeConfig};
use crate::schema::{InferredSchema};

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Walk `schema` and convert every `Object` node whose dot-path matches an
/// entry in `paths` into a `Map`. Operates recursively, modifying the tree
/// in place (via owned value passing).
pub fn apply_map_paths(
    schema: InferredSchema,
    paths: &[String],
    cfg: MergeConfig,
) -> InferredSchema {
    if paths.is_empty() {
        return schema;
    }

    // Parse each path string into a Vec<Segment> once up front.
    let parsed: Vec<Vec<Segment>> = paths.iter().map(|p| parse_path(p)).collect();

    apply(schema, &parsed, cfg)
}

// ---------------------------------------------------------------------------
// Path segment
// ---------------------------------------------------------------------------

/// A single step in a map-path.
#[derive(Debug, Clone, PartialEq)]
enum Segment {
    /// Descend into a named field of an Object.
    Field(String),
    /// Descend into the items schema of an Array.
    Items,
}

/// Parse `"snapshot.trackedFileBackups"` → `[Field("snapshot"), Field("trackedFileBackups")]`
/// Parse `"data.msgs[].content"` → `[Field("data"), Field("msgs"), Items, Field("content")]`
fn parse_path(path: &str) -> Vec<Segment> {
    let mut segments = Vec::new();

    for part in path.split('.') {
        if part.is_empty() {
            continue;
        }
        if let Some(field) = part.strip_suffix("[]") {
            // e.g. "msgs[]" → Field("msgs") then Items
            if !field.is_empty() {
                segments.push(Segment::Field(field.to_string()));
            }
            segments.push(Segment::Items);
        } else {
            segments.push(Segment::Field(part.to_string()));
        }
    }

    segments
}

// ---------------------------------------------------------------------------
// Tree walk
// ---------------------------------------------------------------------------

/// Recursively apply all parsed paths to `schema`.
/// `paths` contains only the paths that are still active at this node —
/// we advance each path one segment at a time as we descend.
fn apply(schema: InferredSchema, paths: &[Vec<Segment>], cfg: MergeConfig) -> InferredSchema {
    match schema {
        InferredSchema::Object { fields, nullable } => {
            apply_to_object(fields, nullable, paths, cfg)
        }
        InferredSchema::Array { items, nullable } => {
            apply_to_array(items, nullable, paths, cfg)
        }
        // Recurse into each AnyOf variant — a map-path might match inside
        // one of the structural alternatives.
        InferredSchema::AnyOf { variants, nullable } => {
            let variants = variants
                .into_iter()
                .map(|v| apply(v, paths, cfg))
                .collect();
            InferredSchema::AnyOf { variants, nullable }
        }

        // All other variants have no children to recurse into.
        other => other,
    }
}

fn apply_to_object(
    mut fields: indexmap::IndexMap<String, crate::schema::FieldInfo>,
    nullable: bool,
    paths: &[Vec<Segment>],
    cfg: MergeConfig,
) -> InferredSchema {
    // Partition paths:
    // - paths with an empty tail → this node should become a Map
    // - paths with Field(name) as first segment → recurse into that field
    let mut force_map = false;
    // field_name → remaining path tails to push down into that field
    let mut child_paths: std::collections::HashMap<String, Vec<Vec<Segment>>> =
        std::collections::HashMap::new();

    for path in paths {
        match path.as_slice() {
            // Empty path means "convert this node".
            [] => force_map = true,
            [Segment::Field(name), rest @ ..] => {
                child_paths
                    .entry(name.clone())
                    .or_default()
                    .push(rest.to_vec());
            }
            // Items segment at an Object node — doesn't match, ignore.
            [Segment::Items, ..] => {}
        }
    }

    // Recurse into children first (so deeper conversions happen before
    // a potential force_map folds everything into one value schema).
    for (key, fi) in fields.iter_mut() {
        if let Some(child_tails) = child_paths.get(key) {
            fi.schema = apply(
                std::mem::replace(&mut fi.schema, InferredSchema::Never),
                child_tails,
                cfg,
            );
        }
    }

    if force_map {
        // Fold all field schemas into one unified value schema via LUB,
        // then return a Map node.
        let value_schema = fields
            .into_values()
            .fold(InferredSchema::Never, |acc, fi| lub(acc, fi.schema, cfg));
        InferredSchema::Map {
            value_schema: Box::new(value_schema),
            nullable,
        }
    } else {
        InferredSchema::Object { fields, nullable }
    }
}

fn apply_to_array(
    items: Box<InferredSchema>,
    nullable: bool,
    paths: &[Vec<Segment>],
    cfg: MergeConfig,
) -> InferredSchema {
    // Collect tails for paths that start with Items.
    let item_tails: Vec<Vec<Segment>> = paths
        .iter()
        .filter_map(|path| match path.as_slice() {
            [Segment::Items, rest @ ..] => Some(rest.to_vec()),
            _ => None,
        })
        .collect();

    if item_tails.is_empty() {
        return InferredSchema::Array { items, nullable };
    }

    let new_items = apply(*items, &item_tails, cfg);
    InferredSchema::Array {
        items: Box::new(new_items),
        nullable,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::merge::MergeConfig;
    use crate::schema::{FieldInfo, InferredSchema, ScalarType};
    use indexmap::indexmap;

    fn cfg() -> MergeConfig {
        MergeConfig { cap_union: 5 }
    }

    fn str_scalar() -> InferredSchema {
        InferredSchema::Scalar { ty: ScalarType::Str, nullable: false }
    }

    fn int_scalar() -> InferredSchema {
        InferredSchema::Scalar { ty: ScalarType::Integer, nullable: false }
    }

    fn make_object(fields: indexmap::IndexMap<String, FieldInfo>) -> InferredSchema {
        InferredSchema::Object { fields, nullable: false }
    }

    #[test]
    fn parse_simple_path() {
        let p = parse_path("snapshot.trackedFileBackups");
        assert_eq!(p, vec![
            Segment::Field("snapshot".into()),
            Segment::Field("trackedFileBackups".into()),
        ]);
    }

    #[test]
    fn parse_path_with_array() {
        let p = parse_path("data.msgs[].content");
        assert_eq!(p, vec![
            Segment::Field("data".into()),
            Segment::Field("msgs".into()),
            Segment::Items,
            Segment::Field("content".into()),
        ]);
    }

    #[test]
    fn parse_leading_array() {
        // "[].field" — items first, then field
        let p = parse_path("[].field");
        assert_eq!(p, vec![
            Segment::Items,
            Segment::Field("field".into()),
        ]);
    }

    #[test]
    fn top_level_path_converts_object_to_map() {
        let schema = make_object(indexmap! {
            "a".to_string() => FieldInfo { schema: str_scalar(), occurrences: 1 },
            "b".to_string() => FieldInfo { schema: int_scalar(), occurrences: 1 },
        });

        // Path "" or just the field name at root — here we test direct top-level
        // by wrapping in a parent object and using a single-segment path.
        let root = make_object(indexmap! {
            "files".to_string() => FieldInfo { schema: schema, occurrences: 1 },
        });

        let result = apply_map_paths(root, &["files".to_string()], cfg());
        if let InferredSchema::Map { .. } = result {
            // Root was converted — correct
        } else if let InferredSchema::Object { fields, .. } = &result {
            // The "files" child should have been converted
            assert!(
                matches!(fields["files"].schema, InferredSchema::Map { .. }),
                "expected files to be Map, got: {:?}", fields["files"].schema
            );
        }
    }

    #[test]
    fn nested_path_converts_only_target() {
        let inner = make_object(indexmap! {
            "path1".to_string() => FieldInfo { schema: str_scalar(), occurrences: 1 },
            "path2".to_string() => FieldInfo { schema: str_scalar(), occurrences: 1 },
        });
        let outer = make_object(indexmap! {
            "backups".to_string() => FieldInfo { schema: inner, occurrences: 1 },
            "other".to_string()   => FieldInfo { schema: int_scalar(), occurrences: 1 },
        });

        let result = apply_map_paths(outer, &["backups".to_string()], cfg());

        if let InferredSchema::Object { fields, .. } = &result {
            // "backups" should be Map
            assert!(
                matches!(fields["backups"].schema, InferredSchema::Map { .. }),
                "expected backups to be Map"
            );
            // "other" should remain Scalar
            assert!(
                matches!(fields["other"].schema, InferredSchema::Scalar { .. }),
                "expected other to remain Scalar"
            );
        } else {
            panic!("expected root to remain Object");
        }
    }

    #[test]
    fn array_descent_converts_item_field() {
        let item_schema = make_object(indexmap! {
            "k1".to_string() => FieldInfo { schema: str_scalar(), occurrences: 1 },
            "k2".to_string() => FieldInfo { schema: str_scalar(), occurrences: 1 },
        });
        let root = make_object(indexmap! {
            "list".to_string() => FieldInfo {
                schema: InferredSchema::Array {
                    items: Box::new(item_schema),
                    nullable: false,
                },
                occurrences: 1,
            },
        });

        let result = apply_map_paths(root, &["list[]".to_string()], cfg());

        if let InferredSchema::Object { fields, .. } = &result {
            if let InferredSchema::Array { items, .. } = &fields["list"].schema {
                assert!(
                    matches!(items.as_ref(), InferredSchema::Map { .. }),
                    "expected array items to be Map"
                );
            } else {
                panic!("expected list to be Array");
            }
        } else {
            panic!("expected root to remain Object");
        }
    }

    #[test]
    fn empty_paths_is_noop() {
        let schema = make_object(indexmap! {
            "x".to_string() => FieldInfo { schema: str_scalar(), occurrences: 1 },
        });
        let original = format!("{schema:?}");
        let result = apply_map_paths(schema, &[], cfg());
        assert_eq!(format!("{result:?}"), original);
    }

    #[test]
    fn nonexistent_path_is_noop() {
        let schema = make_object(indexmap! {
            "x".to_string() => FieldInfo { schema: str_scalar(), occurrences: 1 },
        });
        let result = apply_map_paths(schema, &["does_not_exist".to_string()], cfg());
        assert!(matches!(result, InferredSchema::Object { .. }));
    }
}
