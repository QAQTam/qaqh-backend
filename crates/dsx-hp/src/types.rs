//! Core types for the dsx-hp daemon: verdict, errors, process metadata.
//!
//! All types here are `Serialize + Deserialize` for JSON-LP IPC framing.
//! No dependency on other hp modules (except `AgentEmotion` in `Verdict::Healthy`).

use serde::{Deserialize, Serialize};

use crate::emotion::AgentEmotion;

// ── Verdict — judge() output ──

/// Health verdict — the sole output of `judge()`, serialized as JSON-LP frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Verdict {
    /// All systems normal.
    Healthy {
        level: HealthLevel,
        emotion: Option<AgentEmotion>,
    },
    /// Degraded but operational — warnings present but no critical failure.
    Degraded {
        level: HealthLevel,
        reasons: Vec<String>,
        advice: String,
    },
    /// Requires intervention — circuit breaker tripped, API errors accumulating.
    Alert {
        severity: AlertSeverity,
        source: AlertSource,
        message: String,
    },
    /// Process presumed dead — heartbeat timeout exceeded.
    Dead {
        pid: u32,
        name: String,
        reason: String,
        since_secs: u64,
    },
}

impl Verdict {
    /// Returns `true` for verdicts that demand immediate attention.
    pub fn is_critical(&self) -> bool {
        matches!(
            self,
            Verdict::Dead { .. }
                | Verdict::Alert { severity: AlertSeverity::Critical, .. }
        )
    }
}

// ── Enums ──

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HealthLevel {
    Green,
    Yellow,
    Red,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AlertSeverity {
    Info,
    Warning,
    Critical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AlertSource {
    Liveness,
    Monitor,
    CircuitBreaker,
    Sentinel,
    Pipeline,
}

/// Process types that can register with the HP daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ProcessKind {
    Agent,
    Tools,
    Tui,
}

impl std::fmt::Display for ProcessKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProcessKind::Agent => write!(f, "agent"),
            ProcessKind::Tools => write!(f, "tools"),
            ProcessKind::Tui => write!(f, "tui"),
        }
    }
}

// ── Errors ──

/// Errors returned by the `HealthProbe` service.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HpError {
    #[allow(unused)]
    ProcessNotFound(u32),
    #[allow(unused)]
    DuplicateRegistration(u32),
    #[allow(unused)]
    Timeout,
    #[allow(unused)]
    Internal(String),
}

impl std::fmt::Display for HpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HpError::ProcessNotFound(pid) => write!(f, "process {pid} not found"),
            HpError::DuplicateRegistration(pid) => write!(f, "process {pid} already registered"),
            HpError::Timeout => write!(f, "operation timed out"),
            HpError::Internal(msg) => write!(f, "internal error: {msg}"),
        }
    }
}

impl std::error::Error for HpError {}

// ── Process state structs ──

/// Full health state for a single process (returned by `query()`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessHealth {
    pub pid: u32,
    pub kind: ProcessKind,
    pub name: String,
    pub alive: bool,
    /// Unix-epoch seconds of the last received heartbeat.
    pub last_heartbeat: u64,
    pub missed_heartbeats: u32,
}

/// Lightweight process descriptor for listing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessSummary {
    pub pid: u32,
    pub kind: ProcessKind,
    pub name: String,
    pub alive: bool,
    pub uptime_secs: u64,
}
