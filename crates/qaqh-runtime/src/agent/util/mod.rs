//! Utility functions for the message loop, split into focused submodules:
//! - `datetime`  — epoch/date formatting (historical UTC+8 bias preserved)
//! - `format`    — tool-call display/parsing and round-complete projection
//!
//! Re-exports below keep existing `util::` call sites stable.

mod datetime;
mod format;

pub(crate) use datetime::{chrono_local_date, chrono_local_datetime, epoch_to_date};
pub(crate) use format::{
    canonical_call_id, canonical_interaction_id, canonical_turn_id, emit_round_complete_via_emitter,
    interaction_id_matches, permission_id_matches,
};
