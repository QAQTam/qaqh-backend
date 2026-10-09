//! ToolManager framework — tool registration, execution, and lifecycle.
//!
//! Submodules register handlers via `pub fn register(mgr: &mut ToolManager)`.

pub use qaqh_file_tools::confirm_apply;
pub use qaqh_file_tools::copy_range;
pub use qaqh_permission::conflict;
pub mod dashboard;
pub mod display;
pub use qaqh_file_tools::grep_tool;
pub use qaqh_file_tools::pending;
pub use qaqh_process_tools::exec;

pub use qaqh_file_tools::apply_patch;
pub use qaqh_file_tools::apply_patch_engine;

pub use qaqh_file_tools::arg_estimate;
pub mod authorization;
pub use qaqh_file_tools::code_delta;
pub use qaqh_file_tools::edit;
pub mod execution;
pub mod exec_audit;
pub use qaqh_file_tools::file_glob;
pub use qaqh_file_tools::file_mutate;
pub use qaqh_file_tools::file_query;
pub use qaqh_fs_core::file_cache;
pub use qaqh_fs_core::file_shared;
pub use qaqh_fs_core::file_state;
// Git 面板 API 已拆至 qaqh-git（P2 拆分）；以 crate 别名 re-export，
// `qaqh_workspace::git::*` 路径不变。
pub use qaqh_git as git;
// P2(d) 拆分：文件/进程工具组 crate。`qaqh_workspace::X` 旧路径全部经
// re-export 保持不变；门面留守 manager/runtime/registration/execution/
// authorization/audit/tool_side_fold/display/probe + todo/ask_user/skill/
// spy_tool/web/dashboard。
pub use qaqh_file_tools::read_image;
pub use qaqh_file_tools::support::{json_err, json_err_string, json_ok};
pub mod runtime;
mod safety;
pub mod skill;
// SDK 主体已拆至 qaqh-tool-core；tool_api 为 shim（re-export + boundary），
// tool_capabilities 直接 re-export（能力迁移表随 SDK 下沉）。
pub mod tool_api;
pub use qaqh_tool_core::tool_capabilities;
pub mod tool_side_fold;
mod web;

pub mod ask_user;

pub mod todo;

pub use qaqh_permission::workspace;
pub use qaqh_process_tools::process_inspect;
pub use qaqh_process_tools::process_registry;

pub mod registration;

#[cfg(any(test, feature = "test-harness"))]
pub mod probe;

pub mod manager;
/// Permission engine: tool categories, levels, trusted folders.
pub use qaqh_permission::permission;

pub mod audit;

pub use qaqh_file_tools::journal;
pub mod spy_tool;
// 壳探测引导：daemon 启动期调用一次（与 cache_system_path/detect_os_info 同批），
// 把「探测到的壳」钉进进程状态，保证 exec 的可用性探测与实际派生同源。
pub use exec::{bootstrap as bootstrap_exec_shell, register_shell as register_exec_shell};
pub use manager::{
    DYNAMIC_DESCRIPTION_LIMIT, DynamicTool, MCP_DYNAMIC_PREFIX, ToolExecMeta, ToolExecReport,
    ToolManager, ToolStats, build_dynamic_tool,
};
// PR-1-1 / B1: authorization & permission vocabulary at the crate root —
// loop-side references stay `qaqh_workspace::X` without naming submodules.
pub use authorization::{
    Admission, ApprovalError, AuthorizedToolCall, PermissionChallenge, ToolInvocation, admit,
    admit_with_context, authorize_call, authorize_call_with_context, trust_folder,
};
pub use permission::{
    PermissionDecision, PermissionLevel, PermissionRisk, ToolCategory, TrustedFolderSet,
    classify_risk, extract_target_paths, extract_target_paths_in, is_sensitive_session_path,
    normalize_lexically, patch_target_paths, path_within_dir, resolve_target_path,
    resolve_target_path_in,
};
pub use safety::SafetyVerdict;

/// Return current time as "UTC+8 YYYY-MM-DD HH:MM" (matching the [timeis:] prefix convention).
pub fn now_utc8() -> String {
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs() + 8 * 3600;
    let days = secs / 86400;
    let day_secs = secs % 86400;
    let hours = day_secs / 3600;
    let minutes = (day_secs % 3600) / 60;
    let (y, m, d) = qaqh_types::platform::civil_from_days(days as i64);
    format!("UTC+8 {y:04}-{m:02}-{d:02} {hours:02}:{minutes:02}")
}

pub use qaqh_tool_core::ToolRisk;

pub use qaqh_tool_core::JsonArgs;

pub use qaqh_types::{
    ContentRef, ToolContinuation, ToolError as CanonicalToolError, ToolModelPayload, ToolResult,
    ToolStatus,
};

// ── Global state ──

// ── 全局状态：已随权限引擎拆至 qaqh-permission（P2 拆分，研究文档 §4-c）。
// 进程级会话/工作区/取消状态 + actor thread-local + PLAN_BLOCKED 经下方
// re-export 保持 `qaqh_workspace::X` 路径兼容；TEST_RUNTIME_SERIAL 仅服务
// 门面自身测试（journal/confirm_apply/copy_range），留在此处。

/// Unit tests mutate process-wide runtime state. Keep those mutations
/// deterministic even when the Rust test harness runs modules in parallel.
pub static TEST_RUNTIME_SERIAL: std::sync::LazyLock<std::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(()));

pub use qaqh_permission::{
    CANCEL, CURRENT_SESSION, CURRENT_WORKSPACE, PLAN_BLOCKED, clear_actor_context, clear_cancel,
    current_session, current_workspace, display_path, is_actor_context, is_cancel,
    pop_thread_workspace, push_thread_workspace, remove_session_cancel, resolve_workspace_path,
    set_actor_context, set_cancel, set_current_session, set_session_cancel, set_workspace,
};

pub use qaqh_tool_core::{
    EXEC_PROGRESS_CHANNEL_CAPACITY, ExecOutputStream, ExecProgressEvent, ExecProgressSender,
    ExecProgressTotals, bounded_exec_progress_channel,
};

pub use qaqh_tool_core::ToolEffect;

/// Structured error type for tool operations.
///
/// Replaces `"[ERROR]"` / `"[PARTIAL]"` / `"[CANCELLED]"` prefix conventions
/// with a typed enum so callers can match on error categories instead of
/// string prefixes.
#[derive(Debug, Clone)]
pub enum ToolError {
    /// Tool manager not initialised or Mutex poisoned.
    ManagerUnavailable,
    /// Unknown tool name requested.
    UnknownTool { name: String },
    /// Session mismatch between authorization proof and active runtime.
    SessionMismatch,
    /// Permission denied for the requested operation.
    PermissionDenied { reason: String },
    /// Operation cancelled by user.
    Cancelled,
    /// Blocked by operating mode (e.g. PLAN mode blocks write/exec).
    BlockedByMode { mode: String, tool: String },
    /// Invalid or malformed arguments.
    InvalidArgs { message: String },
    /// IO / filesystem error.
    Io {
        tool: String,
        path: String,
        message: String,
    },
    /// Resource mismatch — invocation targets different resources than authorized.
    ResourceMismatch,
    /// Runtime context not initialised.
    RuntimeNotInitialized,
    /// Durable audit barrier failed; the handler must not run.
    AuditUnavailable { message: String },
    /// Result-side audit failed after execution; side effects are
    /// indeterminate and subsequent high-risk tools are quarantined.
    AuditQuarantined { message: String },
    /// Tool-specific error with a machine-readable code.
    ToolSpecific {
        tool: String,
        code: String,
        message: String,
    },
    /// Partial failure — operation did not complete.
    Partial { message: String },
    /// Generic internal error.
    Internal { message: String },
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ManagerUnavailable => {
                write!(
                    f,
                    "[ERROR] tool manager unavailable — poisoned or not initialised"
                )
            }
            Self::UnknownTool { name } => {
                write!(f, "[ERROR] Unknown tool: {name}")
            }
            Self::SessionMismatch => {
                write!(
                    f,
                    "[ERROR] session mismatch — authorization doesn't match active session"
                )
            }
            Self::PermissionDenied { reason } => {
                write!(f, "[PERMISSION_REQUIRED] {reason}")
            }
            Self::Cancelled => {
                write!(f, "[CANCELLED]")
            }
            Self::BlockedByMode { mode, tool } => {
                write!(f, "[BLOCKED] {mode} mode: '{tool}' is not allowed")
            }
            Self::InvalidArgs { message } => {
                write!(f, "[ERROR] invalid arguments: {message}")
            }
            Self::Io {
                tool,
                path,
                message,
            } => {
                write!(f, "[ERROR] {tool}: {path} — {message}")
            }
            Self::ResourceMismatch => {
                write!(
                    f,
                    "[ERROR] Resource mismatch — tool invocation targets different resources than authorized"
                )
            }
            Self::RuntimeNotInitialized => {
                write!(
                    f,
                    "[ERROR] Tool execution requires an initialized runtime context — call set_context() first"
                )
            }
            Self::AuditUnavailable { message } => {
                write!(f, "[ERROR] audit unavailable: {message}")
            }
            Self::AuditQuarantined { message } => {
                write!(f, "[ERROR] audit quarantined: {message}")
            }
            Self::ToolSpecific {
                tool,
                code,
                message,
            } => {
                write!(f, "[ERROR] {tool}: {code} — {message}")
            }
            Self::Partial { message } => {
                write!(f, "[PARTIAL] {message}")
            }
            Self::Internal { message } => {
                write!(f, "[ERROR] {message}")
            }
        }
    }
}

impl ToolError {
    /// Machine-readable 错误码（审计/遥测用）。
    ///
    /// 与 [`Self::into_result`] 的 code 单一来源：改这里即同时改模型面与
    /// 审计账本，避免两处映射漂移。
    pub fn code(&self) -> &'static str {
        match self {
            Self::ManagerUnavailable => "manager_unavailable",
            Self::UnknownTool { .. } => "unknown_tool",
            Self::SessionMismatch => "session_mismatch",
            Self::PermissionDenied { .. } => "permission_denied",
            Self::Cancelled => "cancelled",
            Self::BlockedByMode { .. } => "blocked_by_mode",
            Self::InvalidArgs { .. } => "invalid_arguments",
            Self::Io { .. } => "io_error",
            Self::ResourceMismatch => "resource_mismatch",
            Self::RuntimeNotInitialized => "runtime_not_initialized",
            Self::AuditUnavailable { .. } => "audit_unavailable",
            Self::AuditQuarantined { .. } => "audit_quarantined",
            Self::ToolSpecific { .. } => "tool_error",
            Self::Partial { .. } => "partial",
            Self::Internal { .. } => "internal_error",
        }
    }

    pub fn into_result(self) -> ToolResult {
        let message = self.to_string();
        let code = self.code();
        let (retryable, hint) = match &self {
            Self::ManagerUnavailable => (true, None),
            Self::UnknownTool { .. } => (false, None),
            Self::SessionMismatch => (false, None),
            Self::PermissionDenied { .. } => (false, None),
            Self::Cancelled => (false, None),
            Self::BlockedByMode { .. } => (false, None),
            Self::InvalidArgs { .. } => (false, None),
            Self::Io { .. } => (false, None),
            Self::ResourceMismatch => (false, None),
            Self::RuntimeNotInitialized => (false, None),
            Self::AuditUnavailable { .. } => (false, None),
            Self::AuditQuarantined { .. } => (false, None),
            Self::ToolSpecific { .. } => (false, None),
            Self::Partial { .. } => (false, None),
            Self::Internal { .. } => (true, None),
        };
        ToolResult::error_with(code, message, retryable, hint.map(str::to_string))
    }
}

// ── parse helpers ──

pub fn parse_arg(args: &str, key: &str) -> String {
    qaqh_types::arg::parse_arg(args, key).unwrap_or_default()
}

pub fn parse_arg_or(args: &str, key: &str, default: &str) -> String {
    qaqh_types::arg::parse_arg_or(args, key, default)
}

pub fn parse_opt(args: &str, key: &str) -> Option<String> {
    qaqh_types::arg::parse_arg(args, key)
}

#[cfg(test)]
mod schema_spot_check;

#[cfg(test)]
mod actor_context_tests {
    use super::*;

    #[test]
    fn actor_context_isolates_workspace_and_session_on_current_thread() {
        // Enter actor context and verify thread-local takes precedence.
        set_actor_context("/tmp/actor-ws", "actor-seed");
        assert_eq!(current_workspace(), "/tmp/actor-ws");
        assert_eq!(current_session().as_deref(), Some("actor-seed"));

        set_workspace("/tmp/actor-ws-2");
        set_current_session("actor-seed-2");
        assert_eq!(current_workspace(), "/tmp/actor-ws-2");
        assert_eq!(current_session().as_deref(), Some("actor-seed-2"));

        set_cancel(true);
        assert!(is_cancel());

        clear_actor_context();
        assert_ne!(current_workspace(), "/tmp/actor-ws-2");
        assert_ne!(current_session().as_deref(), Some("actor-seed-2"));
        assert!(!is_cancel());
    }
}
