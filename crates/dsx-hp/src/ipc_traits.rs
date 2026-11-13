//! IPC contract trait — `HealthProbe`.
//!
//! The trait that the HP daemon exposes to the TCP IPC layer (B02).
//! The IPC dispatcher receives JSON-LP frames, calls the corresponding
//! `HealthProbe` method, and serializes the response back.
//!
//! This trait is intentionally synchronous: the HP daemon's core is
//! single-threaded and stateful. Async wrapping happens at the IPC
//! boundary (tokio::spawn per connection, then call into the sync impl).

use crate::types::{HpError, ProcessHealth, ProcessKind, ProcessSummary, Verdict};

/// The health probe service contract.
///
/// Implemented by the HP daemon core (`HealthService` in `main.rs`).
/// Each method corresponds to one or more JSON-LP frame types.
pub trait HealthProbe: Send + Sync {
    /// Register a process for heartbeat tracking.
    ///
    /// Mapped from `HpRegister` IPC frame.
    fn register(&mut self, kind: ProcessKind, name: &str, pid: u32) -> Result<(), HpError>;

    /// Record a heartbeat from a registered process.
    ///
    /// Mapped from `HealthQuery` IPC frame with `type: "heartbeat"`.
    fn heartbeat(&mut self, pid: u32) -> Result<(), HpError>;

    /// Unregister a process (graceful shutdown).
    fn unregister(&mut self, pid: u32) -> Result<(), HpError>;

    /// Run the full health judgment pipeline.
    ///
    /// Mapped from `HealthQuery` IPC frame with `type: "judge"`.
    fn judge(&self) -> Vec<Verdict>;

    /// Get detailed health state for a single process.
    fn query(&self, pid: u32) -> Result<ProcessHealth, HpError>;

    /// List all registered processes with summary info.
    fn list_processes(&self) -> Vec<ProcessSummary>;
}
