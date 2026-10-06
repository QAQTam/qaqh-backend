//! Tool SDK v1 — 工具契约核心类型（base spec §5-§9；09-19 补充稿 §2-§5）。
//!
//! 本模块是 Tool SDK v1 的落点（09-15 plan P1）：类型先行，typed 工具经
//! `register_typed` 接入生产注册表；新工具只能实现 [`TypedTool`]。
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
//! | [`schema`] | 紧凑 schema 生成器（SDK v2：schema 单一事实源 = 类型） |
//! | [`dynamic`] | [`DynamicToolAdapter`]（MCP/LSP 动态工具 typed 适配器） |
//! | [`result`] | [`map_tool_result`]（v1 `ToolResult` → [`ToolOutcome`] 映射 + 进度桥） |
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
pub mod capabilities;
pub mod context;
pub mod descriptor;
pub mod display;
pub mod dynamic;
pub mod erased;
pub mod error;
pub mod output;
pub mod progress;
pub mod result;
pub mod schema;
pub mod typed;

pub use args::{CommandArgs, OffsetLimit, PathArg, PatternArg};
pub use capabilities::{Concurrency, ToolCapabilities};
pub use context::{
    AgentMode, CancellationToken, SandboxMode, SandboxSpec, ToolCallContext, ToolCallSource,
};
pub use descriptor::{
    DescriptorError, Namespace, OutputBudget, ToolDescriptor, ToolExposure, ToolMeta, ToolName,
    ToolSource,
};
pub use display::{
    PathOp, ToolBody, ToolDisplay, ToolDisplayFn, ToolDisplayOutcome, ToolHeader, ToolMetrics,
    ToolTerminalState,
};

pub use dynamic::{DynamicDispatch, DynamicToolAdapter};
pub use erased::ErasedTool;
pub use error::{
    FatalToolError, ToolError, ToolErrorCode, ToolErrorCodeError, ToolErrorKind, ToolExecutionError,
};
pub use output::{
    ToolContentBlock, ToolExecutionMetrics, ToolModelProjection, ToolOutcome, ToolOutputValue,
    ToolProjection, ToolStatus,
};
pub use progress::{ProgressSink, ProgressStream, ToolProgress};
pub use result::map_tool_result;
pub use typed::{TypedTool, TypedToolAdapter};
