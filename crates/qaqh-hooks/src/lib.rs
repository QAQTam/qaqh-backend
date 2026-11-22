//! QAQ-Harness 生命周期 hooks:事件词表、发现、信任、执行与五门派发。
//!
//! 职责边界:本 crate 只做「事件 → 决策」的纯机制层;会话状态、工具执行、
//! 事件面接线归 qaqh-agent-runtime / qaqh-session。对齐调研
//! docs/hooks-lifecycle-research-2026-11-22.md(用户拍板:trust 独立文件、
//! 事件枚举一次到位、wire 契约第一天定边界)。

pub mod config;
pub mod discovery;
pub mod dispatcher;
pub mod event;
pub mod matcher;
pub mod result;
pub mod runner;
pub mod trust;

pub use config::{FailurePolicy, HookEventsConfig, HookHandler, MatcherGroup};
pub use discovery::{Discovery, HookEntry, HookRegistry, HookSource};
pub use dispatcher::{Dispatch, HookRunNotice};
pub use event::{
    HookEventEnvelope, HookEventName, HookPayload, MAX_OUTPUT_SIZE, MAX_PAYLOAD_SIZE,
};
pub use result::{
    ObserveOutcome, PermissionOutcome, PostToolUseOutcome, PreToolUseOutcome, PromptOutcome,
    RunRecord, RunStatus, StopOutcome,
};
pub use runner::RunContext;
pub use trust::{project_hooks_digest, HookTrustStore};
