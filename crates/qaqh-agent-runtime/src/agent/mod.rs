//! `qaqh-agent-runtime::agent` — 单会话 agent 的状态机、引擎与提示词装配。
//!
//! 本模块随 `qaqh-agent-runtime` 从 `qaqh-runtime` 拆出（2026-10-30）。它只依赖
//! `qaqh-agentloop-core` 的决策原语，**不引用 daemon 侧的 ringing 投影/队列层**
//! （对本 crate 内 ringing 传输层的引用命中必须为 0；这是搬运时保留的评审检查单）。
//!
//! ## 布局
//!
//! | 层 | 路径 | 职责 |
//! |---|---|---|
//! | State | `state/` | `AgentState`、会话生命周期、工具注册器 |
//! | Engines | `engine_session.rs` | 会话 / 模型配置解析（`session_effective_config`） |
//! | Prompt | `prompt.rs` | 系统提示词与 OS / 工具信息注入 |
//! | Liveness | `liveness.rs` | worker 存活与心跳 |
//! | Recovery | `tool_recovery.rs` | 崩溃后的工具结果对账 |
//! | Utilities | `util/` | id 规范化与时间格式化 |
//!
//! ## 历史清理（2026-10-30）
//!
//! 旧 Loop（`loop_core` / `loop_*` / `spawn` / `actor`）已在 `session_agent`
//! 驱动接管后删除，本模块文档此前仍在描述它们，现已同步。同一轮把零引用的
//! 残骸一并删掉：`plugins/`（compact / misc / dashboard）、`context`、
//! `engine_title`、`lifecycle_port`、`compaction_port`、`request_context`、
//! `resource_publish`、`stream_continuation`、`workspace_audit`。

pub mod engine_session;
pub mod liveness;
pub mod prompt;
pub mod state;
pub mod tool_recovery;
pub mod types;
pub mod util;
