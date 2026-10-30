//! `qaqh-runtime` — daemon 侧运行时装配与 RPC 服务面。
//!
//! 2026-10-30 拆分后只剩三件事（约 9.5k 行）：
//!
//! | 模块 | 职责 |
//! |---|---|
//! | `registry/` | agent 生命周期登记表（spawn / close / 配额 / 父子树） |
//! | `host_impl` | `qaqh-subagent` 三个宿主 trait 的进程内实现 |
//! | `service/` + `ringing/service_methods` | `QaqhService` 与强类型 RPC 分发 |
//!
//! 另外两个大簇已经外移：
//!
//! - [`qaqh_agent_runtime`]：agent 状态机 / 引擎 / provider·tool job / 配额（`agent`、
//!   `session_agent`、`provider_job`、`tool_job`、`quota_ledger`、`activity`）；
//! - [`qaqh_ringing_runtime`]：Ringing 传输 / 投影 / 队列（`v2`、`hub`、`pending_store`
//!   …），含 `agent_graph`。
//!
//! 两者都在本 crate 根部**同名重导出**，因此 `qaqh_runtime::agent::…`、
//! `crate::session_agent::…` 这类既有路径全部保持不变，daemon 与集成测试零改动。

#![cfg_attr(not(test), forbid(unsafe_code))]

mod agent_catalog;
pub mod error;
mod host_impl;
mod registry;
pub mod ringing;
mod service;
mod subagent_supervisor;

// 已外移的执行层：同名重导出，路径不变。
pub use qaqh_agent_runtime::{
    activity, agent, provider_job, quota_ledger, session_agent, tool_job,
};
// 已外移的传输层：registry 仍需读 canonical 事实（`load_agent_graph` /
// `read_session_facts`）。
pub(crate) use qaqh_ringing_runtime::agent_graph;

pub use qaqh_agent_runtime::SessionActivityTracker;
pub use error::{QaqhError, QaqhResult};
#[doc(hidden)]
pub use host_impl::MAILBOX_ACTIVITY_PARSE_COUNT;
pub use registry::{AgentRegistry, cache_system_path, detect_os_info, detect_shell};
pub use qaqh_ringing_runtime::RingingHub;
pub use service::QaqhService;
