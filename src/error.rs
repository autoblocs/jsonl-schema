use thiserror::Error;
use std::path::PathBuf;

#[derive(Debug, Error)]
pub enum SchemaError {
    #[error("I/O error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("Directory does not exist or is not a directory: {0}")]
    NotADirectory(PathBuf),

    #[error("No .jsonl files found under the provided directories")]
    NoFilesFound,

    #[error("JSON serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("Thread pool error: {0}")]
    ThreadPool(String),
}

/// A non-fatal warning collected during processing.
#[derive(Debug, Clone)]
pub struct Warning {
    pub file: PathBuf,
    pub line: usize,
    pub message: String,
}

impl std::fmt::Display for Warning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[WARN] {}:{} — {}", self.file.display(), self.line, self.message)
    }
}
