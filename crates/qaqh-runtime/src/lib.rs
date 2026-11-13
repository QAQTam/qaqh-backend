//! `qaqh-runtime` — daemon 侧运行时装配与 RPC 服务面。
//!
//! | 模块 | 职责 |
//! |---|---|
//! | `registry/` | agent 生命周期登记表（spawn / close / 配额 / 父子树） |
//! | `host_impl` | `qaqh-subagent` 三个宿主 trait 的进程内实现 |
//! | `service/` + `ringing/service_methods` | `QaqhService` 与强类型 RPC 分发 |
//!
//! Agent execution lives in `qaqh-agent-runtime`; Ringing transport state lives
//! in `qaqh-ringing-runtime`. This crate keeps service assembly and RPC handlers.

// 全 crate 禁 unsafe；仅两处「启动期 PATH 改写」与「malloc_trim 归还堆页」
// 例外，各自带 SAFETY 注释并就地 `#[allow(unsafe_code)]`。
#![cfg_attr(not(test), deny(unsafe_code))]

mod agent_catalog;
pub mod error;
mod host_impl;
mod goal_host;
mod handoff_fork_schedule;
mod registry;
pub mod ringing;
mod service;
mod session_meta;
mod subagent_supervisor;

pub use error::{QaqhError, QaqhResult};
pub use registry::{AgentRegistry, cache_system_path, detect_os_info, detect_shell};
pub use service::QaqhService;
