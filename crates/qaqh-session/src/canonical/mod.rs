//! Canonical `events.jsonl` storage contracts.
//!
//! The fact schema itself lives in [`crate::session_fact_v2`]. This module
//! owns durable storage identity, writer fencing and commit high-water.

mod clock;
mod log;
mod reader;
mod recovery;
mod store;
mod types;

pub use clock::{
    CONTENT_CLOCK_FILE, CONTENT_CLOCK_SCHEMA, CONTENT_DIR, ContentClock, ContentClockRecord,
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
pub use store::{AppendOutcome, CanonicalSessionStore};
pub use types::{
    AppendRejected, EVENTS_COMMIT_SCHEMA, EVENTS_POISON_SCHEMA, EventsCommit, EventsPoison,
    UPGRADE_FENCE_SCHEMA, UpgradeFence, UpgradeState, WRITER_FENCE_SCHEMA, WriterFence, WriterId,
    WriterLease,
};
