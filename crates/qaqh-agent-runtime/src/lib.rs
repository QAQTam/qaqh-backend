//! Session execution: the loop, provider requests, tool dispatch and spawn quotas.
//!
//! `session_loop` owns turn scheduling and writes RingingV2 facts through
//! `SessionOwner`. `session_agent` assembles the provider and tool environment;
//! `agent::session_config` resolves catalog model selections.

#![cfg_attr(not(test), forbid(unsafe_code))]

pub mod agent;
pub mod handoff;
#[cfg(test)]
mod permission_reject_verify_tests;
pub mod progress;
pub mod provider_job;
pub mod quota_ledger;
pub mod session_agent;
pub mod session_hooks;
pub mod session_loop;
/// v4.5.1 任务1:回合收口持久化失败 Fail-Closed 支撑件(独立子模块)。
mod session_persist;
#[cfg(test)]
mod session_loop_tests;
pub mod session_title;
pub mod tool_job;

pub use progress::{AttemptBlocks, ProgressSink, ProgressStream};
pub use qaqh_process_tools::exec_session::{
    ExecSessionManager, StopResult as ExecSessionStopResult,
};
