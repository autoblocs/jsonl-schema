use std::path::{Path, PathBuf};
use walkdir::WalkDir;
use crate::error::SchemaError;

/// Recursively collect all `.jsonl` files under `dirs`.
///
/// Rules:
/// - Only `.jsonl` extension (case-sensitive)
/// - Symlinks are NOT followed
/// - Directories that do not exist → SchemaError::NotADirectory
/// - Results are sorted for deterministic processing order
pub fn collect_jsonl_files(dirs: &[PathBuf]) -> Result<Vec<PathBuf>, SchemaError> {
    let mut files: Vec<PathBuf> = Vec::new();

    for dir in dirs {
        if !dir.is_dir() {
            return Err(SchemaError::NotADirectory(dir.clone()));
        }
        collect_from_dir(dir, &mut files);
    }

    files.sort();
    Ok(files)
}

fn collect_from_dir(dir: &Path, out: &mut Vec<PathBuf>) {
    let walker = WalkDir::new(dir)
        .follow_links(false)    // never follow symlinks
        .same_file_system(false) // allow crossing mount points within the tree
        .into_iter();

    for entry in walker.filter_map(|e| e.ok()) {
        let path = entry.path();

        // Skip symlinks explicitly (walkdir's follow_links=false already
        // prevents descent, but the symlink entry itself may appear)
        if entry.file_type().is_symlink() {
            continue;
        }

        if entry.file_type().is_file() && has_jsonl_extension(path) {
            out.push(path.to_path_buf());
        }
    }
}

fn has_jsonl_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e == "jsonl")
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn setup() -> TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn finds_jsonl_files_recursively() {
        let tmp = setup();
        let root = tmp.path();

        fs::create_dir(root.join("sub")).unwrap();
        fs::write(root.join("a.jsonl"), b"").unwrap();
        fs::write(root.join("sub/b.jsonl"), b"").unwrap();
        fs::write(root.join("sub/c.json"), b"").unwrap();  // should be ignored
        fs::write(root.join("sub/d.txt"), b"").unwrap();   // should be ignored

        let files = collect_jsonl_files(&[root.to_path_buf()]).unwrap();
        assert_eq!(files.len(), 2);
        assert!(files.iter().any(|f| f.ends_with("a.jsonl")));
        assert!(files.iter().any(|f| f.ends_with("b.jsonl")));
    }

    #[test]
    fn rejects_nonexistent_dir() {
        let result = collect_jsonl_files(&[PathBuf::from("/nonexistent/path/xyz")]);
        assert!(matches!(result, Err(SchemaError::NotADirectory(_))));
    }

    #[test]
    fn empty_dir_returns_no_files() {
        let tmp = setup();
        let files = collect_jsonl_files(&[tmp.path().to_path_buf()]).unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn results_are_sorted() {
        let tmp = setup();
        let root = tmp.path();
        fs::write(root.join("z.jsonl"), b"").unwrap();
        fs::write(root.join("a.jsonl"), b"").unwrap();
        fs::write(root.join("m.jsonl"), b"").unwrap();

        let files = collect_jsonl_files(&[root.to_path_buf()]).unwrap();
        let names: Vec<_> = files.iter()
            .map(|f| f.file_name().unwrap().to_str().unwrap())
            .collect();
        assert_eq!(names, ["a.jsonl", "m.jsonl", "z.jsonl"]);
    }
}
