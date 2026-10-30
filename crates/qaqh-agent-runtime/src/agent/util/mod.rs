//! Canonical id helpers（原 util 单体的唯一存活部分）。
//!
//! 2026-10-30：`datetime`（epoch/UTC+8 格式化）、`format_tool_args_display`、
//! `call_/turn_` 幂等映射、`*_id_matches`、`emit_round_complete_via_emitter`
//! 的唯一调用点都在旧 Loop 的引擎里，已随状态机删除。现在只剩
//! [`canonical_interaction_id`]：`qaqh-runtime::registry` 用它把 wire 上的
//! tool_call_id 映射成 canonical 账本里的 `InteractionId`。

mod format;

pub use format::canonical_interaction_id;
