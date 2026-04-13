# jsonl-schema

Infers a unified [JSON Schema (draft-07)](https://json-schema.org/draft-07/json-schema-release-notes.html) from one or more directories of `.jsonl` files.

Designed for homogeneous datasets where files share a common record structure. All files are merged into a single schema via a streaming lattice join — memory usage is proportional to schema width and depth, not to file size or record count.

## Features

- **Fully streaming** — processes files line by line, never loads a full file into memory
- **Handles all valid JSON** — scalars, objects, arrays, nulls, mixed types at any nesting level
- **Union types** — conflicting scalar types become `{ "type": ["string", "integer"] }`; configurable cap before collapsing to `{}`
- **Null promotion** — fields observed as both a type and `null` become `{ "type": ["string", "null"] }` rather than a union
- **Numeric sub-typing** — `integer` and `number` are distinguished; `integer + number → number`
- **Recursive directory scan** — walks multiple directories, symlinks never followed
- **Graceful degradation** — malformed lines emit a warning and are skipped; processing continues

## Build

```bash
cargo build --release
# binary at: ./target/release/jsonl-schema
```

Requires Rust 1.75+.

## Usage

```bash
jsonl-schema --dirs <DIR> [DIR ...] [OPTIONS]
```

### Options

| Flag                     | Default  | Description                                              |
| ------------------------ | -------- | -------------------------------------------------------- |
| `--dirs <DIR>...`        | required | One or more directories to scan recursively              |
| `--depth <N>`            | `20`     | Max nesting depth; nodes beyond this emit `{}`           |
| `--cap-union <N>`        | `5`      | Max scalar variants in a union before collapsing to `{}` |
| `--output <FILE>` / `-o` | stdout   | Write schema to file instead of stdout                   |
| `--no-warnings`          | —        | Suppress warnings on stderr                              |
| `--compact`              | —        | Emit minified JSON instead of pretty-printed             |

### Examples

```bash
# Single directory, output to file
jsonl-schema --dirs ./data --output schema.json

# Multiple directories merged into one schema
jsonl-schema --dirs ./data/2024 ./data/2025 --output schema.json

# Tighter union cap, deeper nesting allowed
jsonl-schema --dirs ./data --depth 30 --cap-union 3 -o schema.json

# Pipe to jq
jsonl-schema --dirs ./data --compact | jq '.properties'
```

Stats are always written to stderr; schema goes to stdout or `--output`. This keeps the tool pipeable.

## Type Lattice

The schema is computed by folding all records through a least-upper-bound (LUB) operation on the following lattice:

```text
              Any  (top — union cap exceeded, depth exceeded, or structural conflict)
             /   \
          Union   ...
         / | \ \
      Str Int Float Bool  Object  Array
                    \              |
                  (field-wise LUB) (item-wise LUB)
            Never  (bottom — no observations yet)
```

Key rules:

- `LUB(T, Never) = T` — Never is the identity element
- `LUB(T, Null) = T nullable` — null promotes nullability, not union membership
- `LUB(Integer, Float) = Float` — numeric sub-lattice promotion
- `LUB(Object, Object)` — field-wise recursive merge; fields present in only some records are included but not required
- `LUB(Array, Array)` — unified item schema (all elements across all records folded together)
- `LUB(Object, Scalar)` → `Any` — structural conflicts collapse immediately

## Output

Fields missing from some records appear in `properties` without a `required` entry — the schema is descriptive, not prescriptive. `additionalProperties` is intentionally omitted.

Example input:

```jsonl
{"name": "alice", "age": 30, "score": 9.5}
{"name": "bob",   "age": 25, "score": null}
{"name": "carol", "age": 40, "score": 8.1, "tag": "vip"}
```

Example output:

```json
{
  "type": "object",
  "properties": {
    "name": { "type": "string" },
    "age": { "type": "integer" },
    "score": { "type": ["number", "null"] },
    "tag": { "type": "string" }
  }
}
```

## Module Structure

```text
src/
  schema.rs    — InferredSchema type and lattice definitions
  infer.rs     — serde_json::Value → InferredSchema
  merge.rs     — LUB implementation
  emit.rs      — InferredSchema → JSON Schema value
  discover.rs  — recursive .jsonl file discovery
  lib.rs       — streaming pipeline and integration tests
  main.rs      — CLI (clap)
  error.rs     — error and warning types
```

## Caveats

- Only `.jsonl` files are processed (case-sensitive extension match)
- All files are assumed to describe the same entity type; mixing unrelated schemas produces a valid but broad result
- `{}` in the output means "no constraint" — either the node exceeded depth/union limits, or no records were observed at that position
