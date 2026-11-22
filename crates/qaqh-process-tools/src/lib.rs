//! QAQ-Harness 进程工具组（P2 crate split from qaqh-workspace）。
//!
//! 承载执行会话、共享进程控制原语与旧执行器内部实现。
//! 依赖 qaqh-sandbox（沙箱执行）、qaqh-policy、qaqh-file-tools（exec 结果
//! 落 journal）、qaqh-tool-core（SDK）。
//!
//! P0-2：门面注入钩子（`hooks::EXEC_MAX_OUTPUT_CHARS` / `PROJECT_PROCESS`
//! 两个 `OnceLock<fn()>`）随线程态穿越一并退役——exec 输出上限随
//! [`qaqh_tool_core::tool_api::ToolCallContext::exec_max_output_chars`] 显式
//! 到达（fold policy 在执行入口注入），`process` 显示投影单一事实源收归
//! [`process_inspect::project_process`]。

pub mod exec;
pub mod exec_session;
#[cfg(test)]
mod process_inspect;
pub mod process_registry;

// ── 根 re-export：让搬移后的模块内 `crate::x` 路径保持可解析 ──

pub use qaqh_file_tools::file_mutate;
pub use qaqh_file_tools::support::{json_err, json_err_string, json_ok};
pub use qaqh_permission::permission;
pub use qaqh_permission::{current_session, current_workspace, is_cancel, resolve_workspace_path};
pub use qaqh_tool_core::ToolRisk;
pub use qaqh_tool_core::tool_api;
pub use qaqh_tool_core::{
    EXEC_PROGRESS_CHANNEL_CAPACITY, ExecOutputStream, ExecProgressEvent, ExecProgressSender,
    ExecProgressTotals, bounded_exec_progress_channel,
};
pub use qaqh_types::platform::now_utc8;
pub use qaqh_types::{ToolResult, ToolStatus};
