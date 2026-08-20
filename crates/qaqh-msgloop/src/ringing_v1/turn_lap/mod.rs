//! Loop turn 阶段拆分（knife-7 S2）。
//!
//! `run_lap` 从 `engine_turn.rs` 逐步拆分到此目录，按数据流主线分阶段：
//!
//! - `gate.rs`   —— gate lap：provider 构建 → 模型请求 → 流式事件聚合/错误归一
//! - `parse.rs`  —— 解析模型响应 → assistant 消息入流（A2 step4 已迁）
//! - `admit.rs`  —— 工具 admit/权限/ask/plan 审查 + 已授权批执行（A2 step5 已迁）
//! - `backfill.rs` —— 工具结果回填 → skills/ContinueTurn 判定（A2 step6 已迁）
//!
//! 纯重构优先：每阶段独立落地、行为不变，靠现有测试兜底。

pub(crate) mod admit;
pub(crate) mod backfill;
pub(crate) mod gate;
pub(crate) mod parse;
