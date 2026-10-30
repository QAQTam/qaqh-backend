//! `qaqh-agent-runtime` — 单会话 agent 的运行时（状态机 / 引擎 / 任务与配额）。
//!
//! 2026-10-30 从 `qaqh-runtime` 拆出。拆出时该簇对 core **零回指**：
//!
//! ```text
//! agent/        → （无内部依赖）
//! session_agent/→ provider_job, tool_job（同 crate）
//! tool_job      → quota_ledger（同 crate）
//! provider_job  → （无）
//! activity      → （无）
//! ```
//!
//! 对外暴露给 `qaqh-runtime` 的只有：`AgentState` / `CancelToken` /
//! `WorkerLiveness` / `session_effective_config` / `canonical_interaction_id` /
//! `Assembly`（provider 装配）/ `WorkspaceToolPort` / `QuotaSnapshot` / `SessionAgent`。
//!
//! ## 布局
//!
//! - `agent/`：`AgentState`、会话生命周期、系统提示词、工具恢复
//! - `session_agent/`：驱动循环（`SessionAgent` / `AgentHandle` / `AgentHost`）
//! - `provider_job` / `tool_job`：模型请求与工具执行的作业封装
//! - `quota_ledger` / `activity`：配额账本与活动追踪

#![cfg_attr(not(test), forbid(unsafe_code))]

pub mod activity;
pub mod agent;
pub mod provider_job;
pub mod quota_ledger;
pub mod session_agent;
pub mod tool_job;

pub use activity::SessionActivityTracker;
