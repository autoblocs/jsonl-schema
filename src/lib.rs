pub mod discover;
pub mod emit;
pub mod error;
pub mod infer;
pub mod merge;
pub mod schema;

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::fs::File;

use rayon::prelude::*;
use serde_json::Value;

use crate::error::{SchemaError, Warning};
use crate::infer::{infer_value, InferConfig};
use crate::merge::{lub, MergeConfig};
use crate::schema::{Accumulator, InferredSchema};

// ---------------------------------------------------------------------------
// Public config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Config {
    pub dirs: Vec<PathBuf>,
    pub max_depth: usize,
    pub cap_union: usize,
    /// Maximum distinct keys before an Object flips to a Map. 0 = disabled.
    pub map_threshold: usize,
    /// Number of threads. 0 = rayon default (logical CPU count).
    pub threads: usize,
}

// ---------------------------------------------------------------------------
// Public result
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct InferResult {
    pub schema: serde_json::Value,
    pub record_count: u64,
    pub file_count: usize,
    pub warning_count: u64,
    pub warnings: Vec<Warning>,
}

// ---------------------------------------------------------------------------
// Per-file intermediate result
// ---------------------------------------------------------------------------

struct FileResult {
    schema: InferredSchema,
    record_count: u64,
    warning_count: u64,
    warnings: Vec<Warning>,
}

// ---------------------------------------------------------------------------
// Main entry point
// ---------------------------------------------------------------------------

pub fn run(cfg: &Config) -> Result<InferResult, SchemaError> {
    let files = discover::collect_jsonl_files(&cfg.dirs)?;

    if files.is_empty() {
        return Err(SchemaError::NoFilesFound);
    }

    let file_count = files.len();
    let infer_cfg = InferConfig {
        max_depth: cfg.max_depth,
        merge: MergeConfig {
            cap_union: cfg.cap_union,
            map_threshold: cfg.map_threshold,
        },
    };

    // Build thread pool — custom size if requested, otherwise rayon default.
    let pool = {
        let mut builder = rayon::ThreadPoolBuilder::new();
        if cfg.threads > 0 {
            builder = builder.num_threads(cfg.threads);
        }
        builder.build().map_err(|e| SchemaError::ThreadPool(e.to_string()))?
    };

    // Phase 1: process each file in parallel, producing one FileResult each.
    let file_results: Vec<FileResult> = pool.install(|| {
        files
            .par_iter()
            .map(|path| process_file(path, infer_cfg))
            .collect::<Result<Vec<_>, SchemaError>>()
    })?;

    // Phase 2: sequential reduction of partial schemas via LUB.
    // N here is file count, not record count — fast regardless.
    let mut root = InferredSchema::Never;
    let mut record_count: u64 = 0;
    let mut warning_count: u64 = 0;
    let mut warnings: Vec<Warning> = Vec::new();

    for fr in file_results {
        root = lub(root, fr.schema, infer_cfg.merge);
        record_count += fr.record_count;
        warning_count += fr.warning_count;
        warnings.extend(fr.warnings);
    }

    let schema = emit::to_json_schema(&root);

    Ok(InferResult {
        schema,
        record_count,
        file_count,
        warning_count,
        warnings,
    })
}

// ---------------------------------------------------------------------------
// File processor — runs inside the thread pool
// ---------------------------------------------------------------------------

fn process_file(path: &Path, cfg: InferConfig) -> Result<FileResult, SchemaError> {
    let file = File::open(path).map_err(|e| SchemaError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;

    let reader = BufReader::new(file);
    let mut acc = Accumulator::default();
    let mut warnings: Vec<Warning> = Vec::new();

    for (line_idx, line_result) in reader.lines().enumerate() {
        let line_number = line_idx + 1;

        let line = match line_result {
            Ok(l) => l,
            Err(e) => {
                acc.warning_count += 1;
                warnings.push(Warning {
                    file: path.to_path_buf(),
                    line: line_number,
                    message: format!("I/O error reading line: {e}"),
                });
                continue;
            }
        };

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let value: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                acc.warning_count += 1;
                warnings.push(Warning {
                    file: path.to_path_buf(),
                    line: line_number,
                    message: format!("JSON parse error: {e}"),
                });
                continue;
            }
        };

        let record_schema = infer_value(&value, cfg.max_depth, cfg);
        acc.root = lub(
            std::mem::replace(&mut acc.root, InferredSchema::Never),
            record_schema,
            cfg.merge,
        );
        acc.record_count += 1;
    }

    Ok(FileResult {
        schema: acc.root,
        record_count: acc.record_count,
        warning_count: acc.warning_count,
        warnings,
    })
}

// ---------------------------------------------------------------------------
// Integration tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn tmp_dir() -> TempDir {
        tempfile::tempdir().unwrap()
    }

    fn config(dirs: Vec<PathBuf>) -> Config {
        Config {
            dirs,
            max_depth: 20,
            cap_union: 5,
            map_threshold: 20,
            threads: 1,
        }
    }

    #[test]
    fn homogeneous_records() {
        let tmp = tmp_dir();
        fs::write(
            tmp.path().join("data.jsonl"),
            r#"{"name":"alice","age":30}
{"name":"bob","age":25}
"#,
        ).unwrap();
        let result = run(&config(vec![tmp.path().to_path_buf()])).unwrap();
        assert_eq!(result.record_count, 2);
        assert_eq!(result.warning_count, 0);
        let props = &result.schema["properties"];
        assert_eq!(props["name"]["type"], "string");
        assert_eq!(props["age"]["type"], "integer");
    }

    #[test]
    fn nullable_field_when_sometimes_null() {
        let tmp = tmp_dir();
        fs::write(
            tmp.path().join("data.jsonl"),
            r#"{"x":1}
{"x":null}
"#,
        ).unwrap();
        let result = run(&config(vec![tmp.path().to_path_buf()])).unwrap();
        let arr = result.schema["properties"]["x"]["type"].as_array().unwrap();
        assert!(arr.contains(&serde_json::json!("integer")));
        assert!(arr.contains(&serde_json::json!("null")));
    }

    #[test]
    fn missing_field_across_records() {
        let tmp = tmp_dir();
        fs::write(
            tmp.path().join("data.jsonl"),
            r#"{"x":1}
{"x":2,"y":"hello"}
"#,
        ).unwrap();
        let result = run(&config(vec![tmp.path().to_path_buf()])).unwrap();
        let props = &result.schema["properties"];
        assert!(props.get("y").is_some());
        assert_eq!(props["x"]["type"], "integer");
    }

    #[test]
    fn malformed_lines_skipped_with_warning() {
        let tmp = tmp_dir();
        fs::write(
            tmp.path().join("data.jsonl"),
            r#"{"x":1}
NOT JSON AT ALL
{"x":3}
"#,
        ).unwrap();
        let result = run(&config(vec![tmp.path().to_path_buf()])).unwrap();
        assert_eq!(result.record_count, 2);
        assert_eq!(result.warning_count, 1);
    }

    #[test]
    fn blank_lines_ignored() {
        let tmp = tmp_dir();
        fs::write(
            tmp.path().join("data.jsonl"),
            r#"{"x":1}

{"x":2}
"#,
        ).unwrap();
        let result = run(&config(vec![tmp.path().to_path_buf()])).unwrap();
        assert_eq!(result.record_count, 2);
        assert_eq!(result.warning_count, 0);
    }

    #[test]
    fn bare_scalar_records() {
        let tmp = tmp_dir();
        fs::write(
            tmp.path().join("scalars.jsonl"),
            r#""hello"
42
"world"
"#,
        ).unwrap();
        let result = run(&config(vec![tmp.path().to_path_buf()])).unwrap();
        assert_eq!(result.record_count, 3);
        assert_eq!(result.warning_count, 0);
        let types = result.schema["type"].as_array().unwrap();
        assert!(types.contains(&serde_json::json!("string")));
        assert!(types.contains(&serde_json::json!("integer")));
    }

    #[test]
    fn integer_and_float_promotes_to_number() {
        let tmp = tmp_dir();
        fs::write(
            tmp.path().join("nums.jsonl"),
            r#"{"x":1}
{"x":3.14}
"#,
        ).unwrap();
        let result = run(&config(vec![tmp.path().to_path_buf()])).unwrap();
        assert_eq!(result.schema["properties"]["x"]["type"], "number");
    }

    #[test]
    fn heterogeneous_field_type_forms_union() {
        let tmp = tmp_dir();
        fs::write(
            tmp.path().join("data.jsonl"),
            r#"{"v":1}
{"v":"hello"}
"#,
        ).unwrap();
        let result = run(&config(vec![tmp.path().to_path_buf()])).unwrap();
        let types = result.schema["properties"]["v"]["type"].as_array().unwrap();
        assert!(types.contains(&serde_json::json!("integer")));
        assert!(types.contains(&serde_json::json!("string")));
    }

    #[test]
    fn nested_object_schema() {
        let tmp = tmp_dir();
        fs::write(
            tmp.path().join("data.jsonl"),
            r#"{"user":{"id":1,"name":"alice"}}
{"user":{"id":2,"name":"bob"}}
"#,
        ).unwrap();
        let result = run(&config(vec![tmp.path().to_path_buf()])).unwrap();
        let user = &result.schema["properties"]["user"];
        assert_eq!(user["type"], "object");
        assert_eq!(user["properties"]["id"]["type"], "integer");
        assert_eq!(user["properties"]["name"]["type"], "string");
    }

    #[test]
    fn array_of_objects_unified() {
        let tmp = tmp_dir();
        fs::write(
            tmp.path().join("data.jsonl"),
            r#"{"items":[{"id":1},{"id":2}]}
"#,
        ).unwrap();
        let result = run(&config(vec![tmp.path().to_path_buf()])).unwrap();
        let items_schema = &result.schema["properties"]["items"]["items"];
        assert_eq!(items_schema["type"], "object");
        assert_eq!(items_schema["properties"]["id"]["type"], "integer");
    }

    #[test]
    fn map_threshold_flips_object_to_map() {
        let tmp = tmp_dir();
        let lines: String = (0..6)
            .map(|i| format!("{{\"key_{i}\": {i}}}\n"))
            .collect();
        fs::write(tmp.path().join("data.jsonl"), lines).unwrap();
        let mut cfg = config(vec![tmp.path().to_path_buf()]);
        cfg.map_threshold = 5;
        let result = run(&cfg).unwrap();
        assert!(
            result.schema.get("additionalProperties").is_some(),
            "expected additionalProperties, got: {}",
            result.schema
        );
        assert!(result.schema.get("properties").is_none());
    }

    #[test]
    fn map_threshold_disabled_at_zero() {
        let tmp = tmp_dir();
        let lines: String = (0..50)
            .map(|i| format!("{{\"key_{i}\": {i}}}\n"))
            .collect();
        fs::write(tmp.path().join("data.jsonl"), lines).unwrap();
        let mut cfg = config(vec![tmp.path().to_path_buf()]);
        cfg.map_threshold = 0;
        let result = run(&cfg).unwrap();
        assert!(result.schema.get("properties").is_some());
        assert!(result.schema.get("additionalProperties").is_none());
    }

    #[test]
    fn parallel_multiple_files_merged() {
        let tmp1 = tmp_dir();
        let tmp2 = tmp_dir();
        fs::write(tmp1.path().join("a.jsonl"), r#"{"x":1}"#).unwrap();
        fs::write(tmp2.path().join("b.jsonl"), r#"{"y":"hello"}"#).unwrap();
        let mut cfg = config(vec![
            tmp1.path().to_path_buf(),
            tmp2.path().to_path_buf(),
        ]);
        cfg.threads = 2;
        let result = run(&cfg).unwrap();
        assert_eq!(result.record_count, 2);
        let props = &result.schema["properties"];
        assert!(props.get("x").is_some());
        assert!(props.get("y").is_some());
    }

    #[test]
    fn no_files_found_error() {
        let tmp = tmp_dir();
        let err = run(&config(vec![tmp.path().to_path_buf()]));
        assert!(matches!(err, Err(SchemaError::NoFilesFound)));
    }
}
