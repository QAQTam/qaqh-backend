//! QAQ-Harness 文件工具组（P2 crate split from qaqh-workspace）。
//!
//! 承载全部「以文件系统为对象」的内置工具实现：write/delete/file_mutate、
//! read/file_query、glob、grep、edit、apply_patch 全家（engine/arg_estimate/
//! code_delta/copy_range/confirm_apply/pending）、journal 审计账本、read_image。
//!
//! 组内历史环（`journal ↔ file_mutate`、`file_query → edit::core`）随同 crate
//! 自然合法化。对门面（qaqh-workspace）只经单向边回接：
//! - 根 re-export：`tool_api` / `file_shared` / `permission` 全局等，使搬移后
//!   的模块内部 `crate::x` 路径零改动可解析；
//! - [`FileRuntime`]：P0-2 后账本与读缓存是**实例状态**——`ToolManager` 构造
//!   时创建一份、全部文件工具共享同一 `Arc`，会话私有 manager 各持一份，
//!   跨会话基线互踩与「一键 clear 全进程基线」的污染机理物理消灭。
//!
//! `ToolManager` 留守门面；`register()` 粘合层经 `RegistersTyped` trait 接受，
//! 组内测试经 [`qaqh_tool_core::tool_api::DescriptorRegistry`] 断言注册面
//! （v4.5 P2-1a 拆除 dev-dependency 反向环）；与具体 `ToolManager` 的耦合
//! 验证保留在门面 `tests/registration_surface.rs`。

#![cfg_attr(not(test), deny(unsafe_code))]
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
pub mod file_shared;
pub mod file_state;
pub mod grep_tool;
pub mod journal;
pub(crate) mod match_diag;
pub mod pending;
pub mod read_image;
pub mod support;
pub(crate) mod walk_limit;

// ── 根 re-export：让搬移后的模块内 `crate::x` 路径保持可解析 ──

pub use qaqh_tool_core::ToolRisk;
/// Tool SDK（schema/描述符/投影的单一事实源在 qaqh-tool-core）。
pub use qaqh_tool_core::tool_api;
pub use qaqh_types::{ToolResult, ToolStatus};

/// FS 核心层（原独立 crate `qaqh-fs-core`，2026-10-31 并入）：file_shared /
/// file_state 的单一事实源就在本 crate 根。
pub use file_shared::{
    LineIndex, READ_MAX_BYTES, READ_MAX_CHARS, READ_MAX_LINE_CHARS, READ_MAX_LINES, content_hash,
    is_binary_read_error, normalize_newlines,
};

/// 会话私有文件运行时状态（账本 + 读缓存；P0-2 后无任何进程级单例）。
pub use file_state::{FileRuntime, ledger_key};

/// 权限引擎（路径解析/风险分类；P0-2 后不再 re-export 任何线程态函数）。
pub use qaqh_permission::permission;

/// 时间与错误信封助手（门面同名函数的单一事实源版本）。
pub use qaqh_types::platform::now_utc8;
pub use support::{json_err, json_err_string, json_ok};
