use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::fs::File;

use rayon::prelude::*;
use serde_json::Value;

use crate::error::{SchemaError, ValidationError};
use crate::discover;

// ---------------------------------------------------------------------------
// Validation config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ValidateConfig {
    pub dirs: Vec<PathBuf>,
    pub schema_path: PathBuf,
    pub threads: usize,
}

// ---------------------------------------------------------------------------
// Validation result
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct ValidateResult {
    pub valid: bool,
    pub total_records: u64,
    pub total_files: usize,
    pub valid_records: u64,
    pub invalid_records: u64,
    pub errors: Vec<ValidationError>,
}

// ---------------------------------------------------------------------------
// Per-file validation result
// ---------------------------------------------------------------------------

struct FileValidationResult {
    valid_count: u64,
    invalid_count: u64,
    errors: Vec<ValidationError>,
}

// ---------------------------------------------------------------------------
// Main validation entry point
// ---------------------------------------------------------------------------

pub fn run(cfg: &ValidateConfig) -> Result<ValidateResult, SchemaError> {
    let files = discover::collect_jsonl_files(&cfg.dirs)?;

    if files.is_empty() {
        return Err(SchemaError::NoFilesFound);
    }

    let file_count = files.len();

    // Load and compile schema
    let schema_str = std::fs::read_to_string(&cfg.schema_path).map_err(|e| {
        SchemaError::SchemaLoadError(cfg.schema_path.clone(), e.to_string())
    })?;

    let schema_json: Value = serde_json::from_str(&schema_str)
        .map_err(|e| SchemaError::SchemaLoadError(cfg.schema_path.clone(), e.to_string()))?;

    let validator = match jsonschema::JSONSchema::compile(&schema_json) {
        Ok(v) => v,
        Err(e) => return Err(SchemaError::InvalidSchema(e.to_string())),
    };

    // Build thread pool
    let pool = {
        let mut builder = rayon::ThreadPoolBuilder::new();
        if cfg.threads > 0 {
            builder = builder.num_threads(cfg.threads);
        }
        builder.build().map_err(|e| SchemaError::ThreadPool(e.to_string()))?
    };

    // Validate files in parallel
    let results: Vec<Result<FileValidationResult, SchemaError>> = pool.install(|| {
        files
            .par_iter()
            .map(|file_path| validate_file(file_path, &validator))
            .collect()
    });

    // Collect all results
    let mut total_records: u64 = 0;
    let mut valid_records: u64 = 0;
    let mut invalid_records: u64 = 0;
    let mut all_errors: Vec<ValidationError> = Vec::new();

    for result in results {
        let file_result = result?;
        valid_records += file_result.valid_count;
        invalid_records += file_result.invalid_count;
        total_records += file_result.valid_count + file_result.invalid_count;
        all_errors.extend(file_result.errors);
    }

    let valid = invalid_records == 0;

    Ok(ValidateResult {
        valid,
        total_records,
        total_files: file_count,
        valid_records,
        invalid_records,
        errors: all_errors,
    })
}

// ---------------------------------------------------------------------------
// Per-file validation
// ---------------------------------------------------------------------------

fn validate_file(
    file_path: &Path,
    validator: &jsonschema::JSONSchema,
) -> Result<FileValidationResult, SchemaError> {
    let file = File::open(file_path).map_err(|e| SchemaError::Io {
        path: file_path.to_path_buf(),
        source: e,
    })?;

    let reader = BufReader::new(file);
    let mut valid_count: u64 = 0;
    let mut invalid_count: u64 = 0;
    let mut errors: Vec<ValidationError> = Vec::new();

    for (line_num, line) in reader.lines().enumerate() {
        let line_idx = line_num + 1; // 1-indexed for user display

        let line_str = line.map_err(|e| SchemaError::Io {
            path: file_path.to_path_buf(),
            source: e,
        })?;

        // Skip empty lines
        if line_str.trim().is_empty() {
            continue;
        }

        let value: Value = match serde_json::from_str(&line_str) {
            Ok(v) => v,
            Err(e) => {
                invalid_count += 1;
                errors.push(ValidationError {
                    file: file_path.to_path_buf(),
                    line: line_idx,
                    reason: format!("Invalid JSON: {}", e),
                });
                continue;
            }
        };

        if validator.is_valid(&value) {
            valid_count += 1;
        } else {
            invalid_count += 1;
            let reason = "Schema validation failed".to_string();
            errors.push(ValidationError {
                file: file_path.to_path_buf(),
                line: line_idx,
                reason,
            });
        }
    }

    Ok(FileValidationResult {
        valid_count,
        invalid_count,
        errors,
    })
}
