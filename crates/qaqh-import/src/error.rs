use std::path::PathBuf;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, ImportError>;

#[derive(Debug, Error)]
pub enum ImportError {
    #[error("I/O at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("source changed while capturing: {0}")]
    SourceChanged(PathBuf),
    #[error("unsupported source: {0}")]
    UnsupportedSource(String),
    #[error("source path is outside every allowed root: {0}")]
    OutsideAllowedRoot(PathBuf),
    #[error("source contains a symbolic link: {0}")]
    SymbolicLink(PathBuf),
    #[error("package is corrupt: {0}")]
    CorruptPackage(String),
    #[error("invalid transcript graph: {0}")]
    InvalidGraph(String),
    #[error("resource limit exceeded: {0}")]
    ResourceLimit(String),
    #[error("cancelled")]
    Cancelled,
    #[error("referenced attachment is missing: {0}")]
    MissingReference(String),
    #[error("package already exists: {0}")]
    PackageExists(PathBuf),
}

pub(crate) fn io_error(path: impl Into<PathBuf>, source: std::io::Error) -> ImportError {
    ImportError::Io {
        path: path.into(),
        source,
    }
}
