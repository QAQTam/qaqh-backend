//! QAQ-Harness 文件工具组（P2 crate split from qaqh-workspace）。
//!
//! 承载全部「以文件系统为对象」的内置工具实现：write/delete/file_mutate、
//! read/file_query、glob、grep、edit、apply_patch 全家（engine/arg_estimate/
//! code_delta/copy_range/confirm_apply/pending）、journal 审计账本、read_image。
//!
//! 组内历史环（`journal ↔ file_mutate`、`file_query → edit::core`）随同 crate
//! 自然合法化。对门面（qaqh-workspace）只经两条单向边回接：
//! - 根 re-export：`tool_api` / `file_shared` / `file_state` / `permission` 全局
//!   等，使搬移后的模块内部 `crate::x` 路径零改动可解析；
//! - [`hooks`]：线程态运行时快照（agent mode / permission level / subagent
//!   sandbox / 视觉模型能力）由门面注入（OnceLock，先例是 qaqh-permission 的
//!   cancel-session resolver）。
//!
//! `ToolManager` 留守门面；`register()` 粘合层经 `RegistersTyped` trait 接受，
//! 组内测试经 dev-dependency 环引用门面具体 manager。

pub mod apply_patch;
pub mod apply_patch_engine;
pub mod arg_estimate;
pub mod code_delta;
pub mod confirm_apply;
pub mod copy_range;
pub mod edit;
pub mod file_glob;
pub mod file_mutate;
pub mod file_query;
pub mod grep_tool;
pub mod hooks;
pub mod journal;
pub mod pending;
pub mod read_image;
pub mod support;

// ── 根 re-export：让搬移后的模块内 `crate::x` 路径保持可解析 ──

pub use qaqh_tool_core::ToolRisk;
/// Tool SDK（schema/描述符/投影的单一事实源在 qaqh-tool-core）。
pub use qaqh_tool_core::tool_api;
pub use qaqh_types::{ToolResult, ToolStatus};

/// FS 核心层（file_shared/cache/state 单一事实源）。
pub use qaqh_fs_core::{file_cache, file_shared, file_state};

/// 权限引擎与其进程级全局状态（current_workspace/session、cancel 等）。
pub use qaqh_permission::permission;
pub use qaqh_permission::{
    CURRENT_WORKSPACE, current_session, current_workspace, is_cancel, resolve_workspace_path,
    set_workspace,
};

/// 时间与错误信封助手（门面同名函数的单一事实源版本）。
pub use qaqh_types::platform::now_utc8;
pub use support::{json_err, json_err_string, json_ok};
