//! Utility functions for the message loop, split into focused submodules:
//! - `datetime`  — epoch/date formatting (historical UTC+8 bias preserved)
//! - `telemetry` — token_stats.jsonl persistence (the loop's only fs side effect)
//! - `format`    — tool-call display/parsing and round-complete projection
//!
//! Re-exports below keep existing `util::` call sites stable.

mod datetime;
mod format;
mod telemetry;

pub(crate) use datetime::{chrono_local_date, chrono_local_datetime, epoch_to_date};
pub(crate) use format::{
    build_assistant_message, emit_round_complete_via_emitter, parse_tool_calls_from_response,
    resolve_effective_name,
};
pub(crate) use telemetry::record_token_usage;
