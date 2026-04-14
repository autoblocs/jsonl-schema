use std::path::PathBuf;

use clap::Parser;
use jsonl_schema::{run, Config};

// ---------------------------------------------------------------------------
// CLI definition
// ---------------------------------------------------------------------------

/// Infer a unified JSON Schema from one or more directories of JSONL files.
#[derive(Parser, Debug)]
#[command(
    name    = "jsonl-schema",
    version,
    about   = "Stream-infers a JSON Schema (draft-07) from JSONL files",
    long_about = None,
)]
struct Args {
    /// One or more directories to scan recursively for .jsonl files.
    #[arg(long = "dirs", required = true, num_args = 1.., value_name = "DIR")]
    dirs: Vec<PathBuf>,

    /// Maximum nesting depth. Objects/arrays beyond this depth emit `{}` (Any).
    #[arg(long = "depth", default_value_t = 20, value_name = "N")]
    depth: usize,

    /// Maximum number of scalar types in a union before collapsing to `{}` (Any).
    #[arg(long = "cap-union", default_value_t = 5, value_name = "N")]
    cap_union: usize,

    /// Object-to-map threshold: objects accumulating more than N distinct keys
    /// across all records are inferred as dynamic maps (additionalProperties).
    /// Set to 0 to disable.
    #[arg(long = "map-threshold", default_value_t = 20, value_name = "N")]
    map_threshold: usize,

    /// Number of threads for parallel file processing.
    /// Defaults to the number of logical CPUs when set to 0.
    #[arg(long = "threads", default_value_t = 0, value_name = "N")]
    threads: usize,

    /// Output file path. If omitted, schema is printed to stdout.
    #[arg(long = "output", short = 'o', value_name = "FILE")]
    output: Option<PathBuf>,

    /// Suppress per-line warnings on stderr.
    #[arg(long = "no-warnings", action = clap::ArgAction::SetFalse)]
    warnings: bool,

    /// Emit minified JSON instead of pretty-printed.
    #[arg(long = "compact", action = clap::ArgAction::SetTrue)]
    compact: bool,
}

fn main() {
    let args = Args::parse();

    let cfg = Config {
        dirs:          args.dirs,
        max_depth:     args.depth,
        cap_union:     args.cap_union,
        map_threshold: args.map_threshold,
        threads:       args.threads,
    };

    match run(&cfg) {
        Ok(result) => {
            if args.warnings {
                for w in &result.warnings {
                    eprintln!("{w}");
                }
            }

            eprintln!(
                "Processed {} record(s) across {} file(s) — {} warning(s)",
                result.record_count,
                result.file_count,
                result.warning_count,
            );

            let json_str = if args.compact {
                serde_json::to_string(&result.schema)
            } else {
                serde_json::to_string_pretty(&result.schema)
            }
            .unwrap_or_else(|e| {
                eprintln!("ERROR: failed to serialize schema: {e}");
                std::process::exit(1);
            });

            match &args.output {
                Some(path) => {
                    if let Err(e) = std::fs::write(path, &json_str) {
                        eprintln!("ERROR: failed to write {}: {e}", path.display());
                        std::process::exit(1);
                    }
                    eprintln!("Schema written to {}", path.display());
                }
                None => println!("{json_str}"),
            }
        }

        Err(e) => {
            eprintln!("ERROR: {e}");
            std::process::exit(1);
        }
    }
}
