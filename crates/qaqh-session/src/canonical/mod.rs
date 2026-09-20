//! Canonical `events.jsonl` storage contracts.
//!
//! The fact schema itself lives in [`crate::session_fact_v2`]. This module
//! owns durable storage identity, writer fencing and commit high-water.

mod log;
mod reader;
mod types;

pub use log::{
    CanonicalError, CanonicalLog, EVENTS_COMMIT_FILE, EVENTS_FILE, EVENTS_LOCK_FILE,
    EVENTS_POISON_FILE, WRITER_FENCE_FILE,
};
pub use reader::CommittedFactReader;
pub use types::{
    AppendRejected, EVENTS_COMMIT_SCHEMA, EVENTS_POISON_SCHEMA, EventsCommit, EventsPoison,
    WRITER_FENCE_SCHEMA, WriterFence, WriterId, WriterLease,
};
