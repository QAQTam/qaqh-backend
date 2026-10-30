//! `qaqh-agent-runtime::agent` — 会话级共享件的薄层。
//!
//! 2026-10-30 二轮清扫：旧 Loop 状态机整簇删除（`AgentState`、会话生命周期、
//! 工具结果对账、引擎协议类型）。它们的构造者与消费者都在旧状态机内部，
//! 生产路径从不构造 `AgentState`——本模块只剩薄驱动仍需要的五件事：
//!
//! | 模块 | 职责 |
//! |---|---|
//! | `session_config.rs` | 会话有效配置解析（profile / BYOK / effort） |
//! | `prompt.rs` | 系统提示词与 OS / 工具信息注入 |
//! | `liveness.rs` | worker 存活与心跳 |
//! | `types.rs` | `CancelToken`（父子任务取消树） |
//! | `util/` | canonical interaction id |
//!
//! 驱动循环本身在 `session_agent/`；工具执行上下文在
//! `qaqh-workspace::runtime`，不再经本模块中转。

pub mod liveness;
pub mod prompt;
pub mod session_config;
pub mod types;
pub mod util;
