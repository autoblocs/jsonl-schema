use std::path::PathBuf;

use clap::{Parser, Subcommand};
use jsonl_schema::{run, Config, validate};

// ---------------------------------------------------------------------------
// CLI definition
// ---------------------------------------------------------------------------

/// Infer and validate JSON schemas from JSONL files.
#[derive(Parser, Debug)]
#[command(
    name    = "jsonl-schema",
    version,
    about   = "Stream-infers and validates JSON Schema (draft-07) from JSONL files",
    long_about = None,
)]
struct Args {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Infer a unified JSON Schema from directories of JSONL files
    Infer(InferArgs),
    /// Validate JSONL logs against a schema
    Validate(ValidateArgs),
}

#[derive(Parser, Debug)]
struct InferArgs {
    /// One or more directories to scan recursively for .jsonl files.
    /// All files are merged into a single unified schema.
    #[arg(long = "dirs", required = true, num_args = 1.., value_name = "DIR")]
    dirs: Vec<PathBuf>,

    /// Maximum nesting depth. Objects/arrays beyond this depth emit `{}` (Any).
    #[arg(long = "depth", default_value_t = 20, value_name = "N")]
    depth: usize,

    /// Maximum number of scalar types in a union before collapsing to `{}` (Any).
    #[arg(long = "cap-union", default_value_t = 5, value_name = "N")]
    cap_union: usize,

    /// Explicit dot-path(s) of object nodes to force-convert to Map
    /// (additionalProperties), regardless of key count.
    /// Dot-separated field names; use [] for array item descent.
    /// Example: --map-paths snapshot.trackedFileBackups modelUsage
    #[arg(long = "map-paths", value_name = "PATH", num_args = 0..)]
    map_paths: Vec<String>,

    /// Number of threads for parallel file processing.
    /// Defaults to the number of logical CPUs when set to 0.
    #[arg(long = "threads", default_value_t = 0, value_name = "N")]
    threads: usize,

    /// Output file path. If omitted, schema is printed to stdout.
    #[arg(long = "output", short = 'o', value_name = "FILE")]
    output: Option<PathBuf>,

    /// Print warnings to stderr (default: true).
    #[arg(long = "no-warnings", action = clap::ArgAction::SetFalse)]
    warnings: bool,

    /// Pretty-print the JSON Schema output (default: true).
    #[arg(long = "compact", action = clap::ArgAction::SetTrue)]
    compact: bool,
}

#[derive(Parser, Debug)]
struct ValidateArgs {
    /// One or more directories to scan recursively for .jsonl files.
    #[arg(long = "dirs", required = true, num_args = 1.., value_name = "DIR")]
    dirs: Vec<PathBuf>,

    /// Path to schema.json file for validation.
    #[arg(long = "input", required = true, value_name = "FILE")]
    input: PathBuf,

    /// Output file path. If omitted, report is printed to stdout.
    #[arg(long = "output", short = 'o', value_name = "FILE")]
    output: Option<PathBuf>,

    /// Number of threads for parallel file processing.
    /// Defaults to the number of logical CPUs when set to 0.
    #[arg(long = "threads", default_value_t = 0, value_name = "N")]
    threads: usize,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() {
    let args = Args::parse();

    match args.command {
        Commands::Infer(infer_args) => run_infer(infer_args),
        Commands::Validate(validate_args) => run_validate(validate_args),
    }
}

fn run_infer(args: InferArgs) {
    let cfg = Config {
        dirs:          args.dirs,
        max_depth:     args.depth,
        cap_union:     args.cap_union,
        map_paths:     args.map_paths,
        threads:       args.threads,
    };

    match run(&cfg) {
        Ok(result) => {
            // Print warnings to stderr
            if args.warnings {
                for w in &result.warnings {
                    eprintln!("{w}");
                }
            }

            // Stats to stderr so stdout stays clean for piping
            eprintln!(
                "Processed {} record(s) across {} file(s) — {} warning(s)",
                result.record_count,
                result.file_count,
                result.warning_count,
            );

            // Serialize schema
            let output = if args.compact {
                serde_json::to_string(&result.schema)
            } else {
                serde_json::to_string_pretty(&result.schema)
            };

            let json_str = match output {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("ERROR: failed to serialize schema: {e}");
                    std::process::exit(1);
                }
            };

            // Write to file or stdout
            match &args.output {
                Some(path) => {
                    if let Err(e) = std::fs::write(path, &json_str) {
                        eprintln!("ERROR: failed to write {}: {e}", path.display());
                        std::process::exit(1);
                    }
                    eprintln!("Schema written to {}", path.display());
                }
                None => {
                    println!("{json_str}");
                }
            }
        }

        Err(e) => {
            eprintln!("ERROR: {e}");
            std::process::exit(1);
        }
    }
}

fn run_validate(args: ValidateArgs) {
    let cfg = validate::ValidateConfig {
        dirs: args.dirs,
        schema_path: args.input,
        threads: args.threads,
    };

    match validate::run(&cfg) {
        Ok(result) => {
            // Stats to stderr
            eprintln!(
                "Validated {} record(s) across {} file(s)",
                result.total_records,
                result.total_files,
            );

            // Serialize report
            let report = serde_json::json!({
                "valid": result.valid,
                "total_records": result.total_records,
                "total_files": result.total_files,
                "valid_records": result.valid_records,
                "invalid_records": result.invalid_records,
                "errors": result.errors.iter().map(|e| {
                    serde_json::json!({
                        "file": e.file.display().to_string(),
                        "line": e.line,
                        "reason": e.reason,
                    })
                }).collect::<Vec<_>>(),
            });

            let json_str = serde_json::to_string_pretty(&report).unwrap();

            // Write to file or stdout
            match &args.output {
                Some(path) => {
                    if let Err(e) = std::fs::write(path, &json_str) {
                        eprintln!("ERROR: failed to write {}: {e}", path.display());
                        std::process::exit(1);
                    }
                    eprintln!("Report written to {}", path.display());
                }
                None => {
                    println!("{json_str}");
                }
            }

            if !result.valid {
                std::process::exit(1);
            }
        }

        Err(e) => {
            eprintln!("ERROR: {e}");
            std::process::exit(1);
        }
    }
}
