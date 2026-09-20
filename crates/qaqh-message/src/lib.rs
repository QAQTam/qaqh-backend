//! qaqh-message: structured conversation state with state-machine lifecycle.
//!
//! `MessageStore` is the single source of truth for messages.
//! Every `push_*` returns `bool` — `true` when the push completed the current turn (last step all tools satisfied, none pending).

pub mod context_flow;
pub mod effect;
pub mod legacy_writer;
pub mod store;
pub mod wal;

pub use context_flow::{
    CompactBehavior, ContextFlow, ContextSource, FlowError, FlowRole, IngestReceipt,
    IngestTraceEntry, LifecyclePolicy, PendingIngest, Sink, Timing, UndoBehavior, Visibility,
    builtin,
};
pub use effect::{PendingTool, PersistOp};
pub use store::{MessageStore, StepToolResult, Turn};
pub use wal::{WalReadError, WalReader, WalWriter, checkpoint_file, open_reader};

/// Deterministic WAL read-fault injection for downstream tests
/// (BUG-2026-09-13-06). Compiled only for test builds: the caller-side
/// regression test needs the same fault the reader's own tests exercise, and a
/// real disk cannot be asked to fail on a specific read.
#[cfg(any(test, feature = "test-harness"))]
pub mod wal_fault {
    pub use crate::wal::fault_harness::{FaultArm, FaultPlan, arm};

    /// Bytes in `session_dir/messages.wal` before the fault fires.
    pub use crate::wal::fault_harness::prefix_bytes;
}
