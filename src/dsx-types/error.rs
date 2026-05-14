//! Unified error type hierarchy for the DSX platform.
//!
//! # Design
//!
//! - [`DsxError`] is the top-level enum — every fallible public API boundary
//!   should return `Result<_, DsxError>`.
//! - Sub-enums (`IpcError`, `ApiError`, `ToolError`, `HealthError`) carry
//!   domain-specific variants and can be freely used within their owning crate.
//! - Conversion to `DsxError` is automatic via `#[from]`.
//!
//! Crate-internal code may still use `anyhow::Result` locally; the transition
//! to `DsxError` is required **only at public API boundaries** that cross
//! crate lines.

// ── Top-level ──

/// Unified error type for all DSX subsystems.
#[derive(Debug, thiserror::Error)]
pub enum DsxError {
    #[error("IPC error: {0}")]
    Ipc(#[from] IpcError),

    #[error("API error: {0}")]
    Api(#[from] ApiError),

    #[error("Tool execution error: {0}")]
    Tool(#[from] ToolError),

    #[error("Health monitoring error: {0}")]
    Health(#[from] HealthError),

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Internal error: {0}")]
    Internal(String),
}

impl DsxError {
    /// Short, one-line description suitable for status bars or log prefixes.
    pub fn short(&self) -> &str {
        match self {
            DsxError::Ipc(_) => "ipc",
            DsxError::Api(_) => "api",
            DsxError::Tool(_) => "tool",
            DsxError::Health(_) => "health",
            DsxError::Config(_) => "config",
            DsxError::Internal(_) => "internal",
        }
    }
}

// ── IPC ──

/// Errors originating from inter-process communication.
///
/// Used by `dsx-proto` and any crate that speaks the JSON-LP protocol.
#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error("Connection refused: {endpoint}")]
    ConnectionRefused { endpoint: String },

    #[error("Connection closed by peer")]
    ConnectionClosed,

    #[error("Read timeout after {secs}s")]
    ReadTimeout { secs: u64 },

    #[error("Write timeout after {secs}s")]
    WriteTimeout { secs: u64 },

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Protocol violation: {0}")]
    Protocol(String),
}

// ── API ──

/// Errors from LLM API requests.
///
/// Limits itself to `u16` / `String` / `u64` fields so `dsx-types` does not
/// need to depend on `reqwest` or any HTTP crate.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("HTTP {status}: {message}")]
    HttpError { status: u16, message: String },

    #[error("Rate limited — retry after {retry_after}s")]
    RateLimited { retry_after: u64 },

    #[error("Authentication failed — check API key")]
    Unauthorized,

    #[error("Insufficient balance")]
    InsufficientBalance,

    #[error("Model `{model}` is not available")]
    ModelNotAvailable { model: String },

    #[error("Request timed out")]
    Timeout,

    #[error("Network error: {0}")]
    Network(String),
}

impl ApiError {
    /// Map an HTTP status code to the most specific variant.
    pub fn from_status(status: u16, body: &str) -> Self {
        match status {
            401 | 403 => ApiError::Unauthorized,
            402 => ApiError::InsufficientBalance,
            429 => ApiError::RateLimited {
                retry_after: parse_retry_after(body).unwrap_or(5),
            },
            400 | 404 | 405 | 422 => ApiError::HttpError {
                status,
                message: body.into(),
            },
            code if code >= 500 => ApiError::HttpError {
                status,
                message: body.into(),
            },
            _ => ApiError::HttpError {
                status,
                message: body.into(),
            },
        }
    }
}

fn parse_retry_after(body: &str) -> Option<u64> {
    // Try common retry-after patterns in error bodies
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(secs) = v.get("retry_after").and_then(|v| v.as_u64()) {
            return Some(secs);
        }
        if let Some(secs) = v.get("retry-after").and_then(|v| v.as_u64()) {
            return Some(secs);
        }
        if let Some(secs) = v
            .get("error")
            .and_then(|e| e.get("retry_after"))
            .and_then(|v| v.as_u64())
        {
            return Some(secs);
        }
    }
    None
}

// ── Tool ──

/// Errors produced during tool execution.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("Tool `{name}` timed out after {secs}s")]
    Timeout { name: String, secs: u64 },

    #[error("Sandbox violation in `{name}`: {detail}")]
    SandboxViolation { name: String, detail: String },

    #[error("Tool `{name}` panicked: {msg}")]
    Panic { name: String, msg: String },

    #[error("Tool `{name}` exited with code {code}: {stderr}")]
    NonZeroExit {
        name: String,
        code: i32,
        stderr: String,
    },

    #[error("Tool not found: {name}")]
    NotFound { name: String },
}

// ── Health ──

/// Errors from the health monitoring subsystem.
#[derive(Debug, thiserror::Error)]
pub enum HealthError {
    #[error("Health check `{check}` failed: {detail}")]
    CheckFailed { check: String, detail: String },

    #[error("Circuit breaker `{breaker}` open (tripped at {pct:.0}% failure rate)")]
    CircuitBreakerTripped { breaker: String, pct: f64 },

    #[error("Liveness probe did not respond")]
    LivenessTimeout,

    #[error("Subsystem is degraded: {detail}")]
    Degraded { detail: String },
}
