//! Utility functions for the agent loop, split into focused submodules:
//! - `datetime`  — epoch/date formatting (historical UTC+8 bias preserved)
//! - `format`    — tool-call display/parsing and round-complete projection
//!
//! 这里从 `pub(crate) use` 改成 `pub use`：模块随 `qaqh-agent-runtime` 拆出后，
//! 其中 `canonical_interaction_id` 由 `qaqh-runtime` 的 registry 跨 crate 调用，
//! 其余条目保持公开以免出现「只为消除未用告警而删 API」的噪音。

mod datetime;
mod format;

pub use datetime::{chrono_local_date, chrono_local_datetime, epoch_to_date};
pub use format::{
    canonical_call_id, canonical_interaction_id, canonical_turn_id,
    emit_round_complete_via_emitter, interaction_id_matches, permission_id_matches,
};
