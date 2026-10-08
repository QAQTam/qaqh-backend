//! Canonical `events.jsonl` storage contracts.
//!
//! The fact schema itself lives in [`crate::session_fact_v2`]. This module
//! owns durable storage identity, writer fencing and commit high-water.

mod blobs;
mod clock;
mod identity;
mod log;
mod reader;
mod recovery;
mod recovery_executor;
mod recovery_state;
mod replay_window;
mod store;
mod tool_ledger;
mod types;

pub use blobs::{BLOBS_DIR, SessionBlobStore, stable_workspace_resource_id};
pub use clock::{
    CONTENT_CLOCK_FILE, CONTENT_CLOCK_SCHEMA, CONTENT_DIR, ContentClock, ContentClockRecord,
};
pub use identity::{
    CANONICAL_IDENTITY_FILE, CANONICAL_IDENTITY_SCHEMA, CanonicalIdentityError,
    CanonicalSessionIdentity, causation_for_command, generate_session_id, generate_ulid,
    ulid_from_text,
};
pub use log::{
    CanonicalError, CanonicalLog, EVENTS_COMMIT_FILE, EVENTS_FILE, EVENTS_LOCK_FILE,
    EVENTS_POISON_FILE, UPGRADE_FENCE_FILE, WRITER_FENCE_FILE,
};
pub use reader::CommittedFactReader;
pub use recovery::{
    RECOVERY_INTENT_FILE, RECOVERY_INTENT_SCHEMA, RecoveryBatchKey, RecoveryIntent,
    RecoveryIntentStatus, RecoveryIntentWriteOutcome, load_recovery_intent,
    persist_recovery_intent, recovery_input_fingerprint, recovery_intent_path,
    remove_recovery_intent_if_stale, sha256_content_hash,
};
pub use recovery_executor::{
    RecoveryExecution, RecoveryExecutionError, RecoveryExecutionOutcome, execute_recovery_intent,
    plan_recovery_intent,
};
pub use recovery_state::{
    CanonicalRecoveryState, CommitRepairOutcome, inspect_recovery_state, repair_commit_marker,
};
pub use replay_window::{
    DEFAULT_REPLAY_WINDOW_RETENTION_MS, DEFAULT_SNAPSHOT_MAX_AGE_MS, DEFAULT_WINDOW_CAPACITY_BYTES,
    DEFAULT_WINDOW_CAPACITY_FACTS, REPLAY_WINDOW_FILE, REPLAY_WINDOW_SCHEMA, ReplayWindowConfig,
    ReplayWindowManifest, ReplayWindowReason, SnapshotStatus, load_replay_window_manifest,
    recover_replay_window_manifest, replay_window_path,
};
pub use store::{AppendOutcome, CanonicalSessionStore};
pub use tool_ledger::{
    DriverClaimOutcome, DriverReleaseOutcome, FactCausation, ToolLedger, ToolLedgerEntry,
    ToolLedgerError, ToolReconciliationEvidence, ToolRecoveryDisposition,
};
pub use types::{
    AppendRejected, EVENTS_COMMIT_SCHEMA, EVENTS_POISON_SCHEMA, EventsCommit, EventsPoison,
    UPGRADE_FENCE_SCHEMA, UpgradeFence, UpgradeState, WRITER_FENCE_SCHEMA, WriterFence, WriterId,
    WriterLease,
};
