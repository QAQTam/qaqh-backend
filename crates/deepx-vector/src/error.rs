use thiserror::Error;

/// Errors that can occur in the vector/embedding pipeline.
#[derive(Error, Debug)]
pub enum VectorError {
    /// Failed to download or load the embedding model.
    #[error("model error: {0}")]
    Model(String),

    /// Tokenization failed.
    #[error("tokenizer error: {0}")]
    Tokenizer(String),

    /// Embedding inference failed.
    #[error("inference error: {0}")]
    Inference(String),

    /// Vector store operation failed.
    #[error("store error: {0}")]
    Store(String),

    /// Chunking operation failed.
    #[error("chunk error: {0}")]
    Chunk(String),

    /// Invalid input (wrong dimensions, empty text, etc.).
    #[error("invalid input: {0}")]
    InvalidInput(String),

    /// I/O error.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Convenience type alias for results from this crate.
pub type VectorResult<T> = std::result::Result<T, VectorError>;
