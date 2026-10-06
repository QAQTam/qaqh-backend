//! QAQ-Harness 进程工具组（P2 crate split from qaqh-workspace）。
//!
//! 承载 exec（方案 A 独占命令入口）与 process_registry / process_inspect。
//! 依赖 qaqh-sandbox（沙箱执行）、qaqh-policy、qaqh-file-tools（exec 结果
//! 落 journal）、qaqh-tool-core（SDK）。
//!
//! 对门面的两条单向回接线：
//! - exec 输出 token 上限经 [`hooks::set_exec_max_output_tokens`] 注入
//!   （单一事实源是执行线程 thread-local 的 fold policy，回调仍在调用线程
//!   读取，语义不变）；
//! - `process` 工具的 display 投影经 [`hooks::set_project_process`] 注入
//!   （投影实现留守门面 `display::project_process`）。

pub mod exec;
pub mod hooks;
pub mod process_inspect;
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
