//! Tool SDK v1 — 工具契约核心类型（base spec §5-§9；09-19 补充稿 §2-§5）。
//!
//! 本模块是 Tool SDK v1 的落点（09-15 plan P1）：**类型先行、不接生产工具**；
//! 迁移期 legacy `ToolHandler` 经 `LegacyToolAdapter` 接入（plan P2），
//! 新工具只能实现 [`TypedTool`]。
//!
//! ## 子模块
//!
//! | 模块 | 内容 |
//! |---|---|
//! | [`display`] | 展示投影类型（09-18 展示契约唯一事实源） |
//! | [`descriptor`] | [`ToolName`] / [`ToolDescriptor`] / 暴露面 / 来源 / 预算 |
//! | [`capabilities`] | [`ToolCapabilities`]（编排能力声明） |
//! | [`context`] | [`ToolCallContext`] / 取消 / 来源 / 模式 |
//! | [`error`] | [`ToolError`] / [`FatalToolError`] / [`ToolExecutionError`] |
//! | [`output`] | [`ToolProjection`] / [`ToolOutcome`] / 投影与指标 |
//! | [`progress`] | [`ToolProgress`] / [`ProgressSink`] |
//! | [`typed`] / [`erased`] | [`TypedTool`] / [`ErasedTool`] |
//! | [`legacy`] | [`LegacyToolAdapter`]（v1 `ToolHandler` 桥接 + `ToolResult`→[`ToolOutcome`] 映射） |
//! | [`boundary`] | 工具 ↔ loop 执行边界（[`ExecuteBatch`] / [`BatchOutcome`]） |
//! | [`args`] | canonical 字段名 + 共享参数类型 |
//!
//! ## 兼容性
//!
//! - 原 `tool_api.rs`（展示投影）已原样迁入 [`display`]；既有
//!   `crate::tool_api::ToolDisplay` 等路径经 re-export 保持不变。
//! - 展示结构以 09-18 展示契约为唯一事实源；wire 映射由
//!   `qaqh-runtime::timeline::wire_display` 唯一负责（契约 §3.1/§3.4）。

pub mod args;
pub mod boundary;
pub mod capabilities;
pub mod context;
pub mod descriptor;
pub mod display;
pub mod erased;
pub mod error;
pub mod legacy;
pub mod output;
pub mod progress;
pub mod typed;

pub use args::{CommandArgs, OffsetLimit, PathArg, PatternArg};
pub use boundary::{
    BatchContext, BatchOutcome, CallOutcome, ExecuteBatch, InteractionDecision, PendingInteraction,
    PendingKind, ResumeInteraction,
};
pub use capabilities::{Concurrency, ToolCapabilities};
pub use context::{
    AgentMode, CancellationToken, SandboxMode, SandboxSpec, ToolCallContext, ToolCallSource,
};
pub use descriptor::{
    DescriptorError, Namespace, OutputBudget, ToolDescriptor, ToolExposure, ToolName, ToolSource,
};
pub use display::{
    PathOp, ToolBody, ToolDisplay, ToolDisplayFn, ToolDisplayOutcome, ToolHeader, ToolMetrics,
    ToolTerminalState,
};
pub use erased::ErasedTool;
pub use error::{
    FatalToolError, ToolError, ToolErrorCode, ToolErrorCodeError, ToolErrorKind, ToolExecutionError,
};
pub use legacy::{LegacyCallOutcome, LegacyToolAdapter, map_tool_result};
pub use output::{
    ToolContentBlock, ToolExecutionMetrics, ToolModelProjection, ToolOutcome, ToolOutputValue,
    ToolProjection, ToolStatus,
};
pub use progress::{ProgressSink, ProgressStream, ToolProgress};
pub use typed::{TypedTool, TypedToolAdapter};
