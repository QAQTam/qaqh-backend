//! Permission engine and execution-scoped workspace/session context.

#![cfg_attr(not(test), forbid(unsafe_code))]
pub mod command_class;
pub mod conflict;
pub mod permission;
pub mod workspace;

pub use command_class::{
    ExecCommandClass, ExecShellKind, classify_exec_args, classify_exec_command, default_shell_kind,
    exec_argument_tokens, exec_shell_kind,
};
pub use permission::{
    PermissionDecision, PermissionLevel, PermissionRisk, ToolCategory, TrustedFolderSet,
    classify_risk, extract_target_paths, extract_target_paths_in, is_self_administration_path,
    normalize_lexically, patch_target_paths, path_within_dir, resolve_target_path,
    resolve_target_path_in,
};

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Bound by the tool execution scope and restored when that scope exits.
#[derive(Clone, Default)]
pub struct ExecutionContext {
    pub session_id: Option<String>,
    pub workspace: Option<String>,
    pub cancellation: Arc<AtomicBool>,
}

thread_local! {
    static EXECUTION_CONTEXT: RefCell<Option<ExecutionContext>> = const { RefCell::new(None) };
}

pub fn execution_context() -> Option<ExecutionContext> {
    EXECUTION_CONTEXT.with(|slot| slot.borrow().clone())
}

pub fn replace_execution_context(context: Option<ExecutionContext>) -> Option<ExecutionContext> {
    EXECUTION_CONTEXT.with(|slot| std::mem::replace(&mut *slot.borrow_mut(), context))
}

pub fn set_current_session(session_id: &str) {
    EXECUTION_CONTEXT.with(|slot| {
        slot.borrow_mut()
            .get_or_insert_with(Default::default)
            .session_id = Some(session_id.into());
    });
}

pub fn set_actor_context(workspace: &str, session: &str) {
    replace_execution_context(Some(ExecutionContext {
        session_id: Some(session.into()),
        workspace: Some(workspace.into()),
        cancellation: Arc::new(AtomicBool::new(false)),
    }));
}

pub fn clear_actor_context() {
    replace_execution_context(None);
}

pub fn push_thread_workspace(workspace: Option<String>) -> Option<String> {
    EXECUTION_CONTEXT.with(|slot| {
        std::mem::replace(
            &mut slot
                .borrow_mut()
                .get_or_insert_with(Default::default)
                .workspace,
            workspace,
        )
    })
}

pub fn pop_thread_workspace(previous: Option<String>) {
    push_thread_workspace(previous);
}

pub fn is_actor_context() -> bool {
    current_session().is_some()
}

pub fn current_session() -> Option<String> {
    execution_context().and_then(|context| context.session_id)
}

pub fn current_workspace() -> String {
    execution_context()
        .and_then(|context| context.workspace)
        .unwrap_or_default()
}

pub fn set_cancel(value: bool) {
    EXECUTION_CONTEXT.with(|slot| {
        slot.borrow_mut()
            .get_or_insert_with(Default::default)
            .cancellation
            .store(value, Ordering::Release);
    });
}

pub fn is_cancel() -> bool {
    execution_context().is_some_and(|context| context.cancellation.load(Ordering::Acquire))
}

pub fn clear_cancel() {
    set_cancel(false);
}

/// Unit tests mutate process-wide runtime state. Keep those mutations
/// deterministic even when the Rust test harness runs modules in parallel.
#[cfg(test)]
pub(crate) static TEST_RUNTIME_SERIAL: std::sync::LazyLock<std::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(()));

/// Tools blocked in PLAN mode. 分类与 handler 声明的 category 对应
/// （Read 类不入表；Write/Exec/Net 按语义列名）。
pub const PLAN_BLOCKED: &[&str] = &[
    "edit",
    "exec_command",
    "write_stdin",
    // Todo v3：write/update 写类受 plan 阻断；todo_list 是读，plan 模式
    // 需要查看任务清单，不放名单。
    "todo_update",
    "todo_write",
];

pub fn set_workspace(path: &str) {
    push_thread_workspace(Some(path.into()));
}

/// Resolve a path against the workspace root.
/// If the path is already absolute, return it lexically normalized.
/// If the workspace is empty or ".", return the path as-is (OS cwd resolution).
/// Otherwise, join the workspace root with the relative path.
///
/// T-8-3②（安全审查 P1-3）：绝对路径此前**原样返回**，于是 `/ws/./a.rs` 与
/// `/ws/a.rs` 派生两套账本键，read 建立的基线对 write/edit 不可见
/// （STALE_FILE 误判）。现在统一做词法归一（去 `.` / `..` / 冗余分隔符）。
///
/// 只做词法归一、**不** `canonicalize`：`std::fs::canonicalize` 在 Windows
/// 返回 `\\?\` verbatim 前缀，会与既有键形态分叉（BUG-2026-09-13-16 的
/// 根因）；符号链接解析改在账本层（`file_state::state_key`）best-effort 做。
pub fn resolve_workspace_path(path: &str) -> String {
    use std::path::Path;
    if path.is_empty() {
        return path.to_string();
    }
    let p = Path::new(path);
    if p.is_absolute() {
        return crate::permission::normalize_lexically(p)
            .to_string_lossy()
            .to_string();
    }
    let ws = current_workspace();
    if ws.is_empty() || ws == "." {
        return path.to_string();
    }
    let joined = Path::new(&ws).join(p);
    // Normalise: strip redundant `.`/`..` components via iterator
    // (e.g. D:\foo\./bar → D:\foo\bar).
    crate::permission::normalize_lexically(&joined)
        .to_string_lossy()
        .to_string()
}

/// Convert an absolute path into a display-friendly relative path.
///
/// Strips the workspace root prefix and uses `/` separators for
/// cross-platform consistency (like Git, VS Code, etc.).
///
/// # Examples
/// ```ignore
/// // workspace = D:\project\QAQ-Harness
/// display_path("D:\\project\\QAQ-Harness\\crates\\foo\\bar.rs") → "crates/foo/bar.rs"
/// display_path("/home/user/project/src/main.rs")          → "src/main.rs"
/// ```
pub fn display_path(abs_path: &str) -> String {
    // Normalise input: strip redundant . components
    let normalized: std::path::PathBuf = std::path::Path::new(abs_path).components().collect();
    let norm_str = normalized.to_string_lossy();

    let ws = current_workspace();
    let ws = ws.trim_end_matches(['/', '\\']);

    if !ws.is_empty() && ws != "." {
        // Case-insensitive prefix match on Windows。注意不能用 to_lowercase
        // 副本做 starts_with 再按原长切片：Unicode 小写映射可能改变字节长度，
        // 非 ASCII 路径会 panic。NTFS 大小写折叠仅限 ASCII，逐字节等长比较
        // 同时保证切点是 char boundary。
        if norm_str.len() >= ws.len()
            && norm_str.as_bytes()[..ws.len()].eq_ignore_ascii_case(ws.as_bytes())
        {
            let (_, tail) = norm_str.split_at(ws.len());
            let rel = tail.trim_start_matches(['/', '\\']);
            return rel.replace('\\', "/");
        }
    }
    // Not under workspace — return normalised path with forward slashes
    norm_str.replace('\\', "/")
}
