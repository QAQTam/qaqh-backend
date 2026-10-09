//! qaqh-permission — 权限引擎 + 进程级会话/工作区/取消状态（P2 crate 拆分）。
//!
//! 自 `qaqh-workspace` 拆出（研究文档 §4-c），解掉原 `lib.rs ↔ permission`
//! 全局互指环：
//!
//! - [`permission`]：权限引擎（category/level、trusted folders、路径分类）；
//! - [`conflict`]：工具间资源冲突判定；
//! - [`workspace`]：`.qaqh/` 项目目录解析与进程 cwd 语义；
//! - crate 根：进程级全局状态（CANCEL / CURRENT_SESSION / CURRENT_WORKSPACE、
//!   actor thread-local 上下文）及其读写入口。
//!
//! ## runtime 环的解法
//!
//! `is_cancel` 的会话归属解析原直读 `qaqh-workspace::runtime` 的 TLS ctx——
//! 这正是要打断的反向边。现在改为 resolver 钩子（[`set_cancel_session_resolver`]）：
//! 门面在 `runtime::set_context` 首次调用时注册一个读 ctx 的 fn 指针，本 crate
//! 不再依赖门面。

pub mod conflict;
pub mod permission;
pub mod workspace;

pub use permission::{
    PermissionDecision, PermissionLevel, PermissionRisk, ToolCategory, TrustedFolderSet,
    classify_risk, extract_target_paths, extract_target_paths_in, is_sensitive_session_path,
    normalize_lexically, patch_target_paths, path_within_dir, resolve_target_path,
    resolve_target_path_in,
};

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{LazyLock, Mutex, RwLock};

// ── Global state ──

/// Global cancel flag for tool execution.
///
/// Set at the start of every interrupt (Cancel, session switch, shutdown)
/// from the reader thread and the main loop. Checked by bridge before
/// executing each tool. Reset via [`clear_cancel`] at the top of the runtime's
/// `handle_user_input` so it is per-turn, not cross-session.
pub static CANCEL: AtomicBool = AtomicBool::new(false);
pub static CURRENT_SESSION: Mutex<Option<String>> = Mutex::new(None);

pub fn set_current_session(session_id: &str) {
    if ACTOR_SESSION.with(|slot| slot.borrow().is_some()) {
        ACTOR_SESSION.with(|slot| *slot.borrow_mut() = Some(session_id.to_string()));
        return;
    }
    let mut guard = CURRENT_SESSION.lock().unwrap_or_else(|e| e.into_inner());
    *guard = Some(session_id.to_string());
}

pub static CURRENT_WORKSPACE: RwLock<String> = RwLock::new(String::new());

thread_local! {
    static ACTOR_WORKSPACE: RefCell<Option<String>> = const { RefCell::new(None) };
    static ACTOR_SESSION: RefCell<Option<String>> = const { RefCell::new(None) };
    static ACTOR_CANCEL: Cell<bool> = const { Cell::new(false) };
}

/// Enter an actor-scoped workspace/session context.
///
/// In-process session and subagent actors call this before running their Loop
/// so workspace/session/cancel state is isolated per actor thread. Once set,
/// `set_workspace` / `set_current_session` write to the thread-local slot;
/// reads prefer the thread-local slot and fall back to the process-wide globals.
pub fn set_actor_context(workspace: &str, session: &str) {
    ACTOR_WORKSPACE.with(|slot| *slot.borrow_mut() = Some(workspace.to_string()));
    ACTOR_SESSION.with(|slot| *slot.borrow_mut() = Some(session.to_string()));
    ACTOR_CANCEL.with(|slot| slot.set(false));
}

/// Leave actor-scoped state and restore process-wide behavior.
pub fn clear_actor_context() {
    ACTOR_WORKSPACE.with(|slot| *slot.borrow_mut() = None);
    ACTOR_SESSION.with(|slot| *slot.borrow_mut() = None);
    ACTOR_CANCEL.with(|slot| slot.set(false));
}

/// Push a workspace value onto THIS thread's actor-workspace slot, returning
/// the previous value (None when the thread had none).
///
/// Tool execution runs on spawned OS threads which never inherit the actor
/// thread's `ACTOR_WORKSPACE` thread-local (see `ActorToolScope`); the
/// capture/install pair uses this accessor to carry the session workspace
/// across the thread boundary (BUG-2026-09-12-05: without it,
/// `current_workspace()` on a tool thread falls back to the process global —
/// always empty in the daemon — and every relative path resolves against the
/// daemon cwd instead of the session workspace). Must be paired with
/// [`pop_thread_workspace`] from the guard's Drop.
pub fn push_thread_workspace(workspace: Option<String>) -> Option<String> {
    ACTOR_WORKSPACE.with(|slot| {
        let previous = slot.borrow().clone();
        *slot.borrow_mut() = workspace;
        previous
    })
}

/// Restore the previous actor-workspace slot value captured by
/// [`push_thread_workspace`]. The TLS slot exists on every thread, so this is
/// safe on spawned tool threads as well.
pub fn pop_thread_workspace(previous: Option<String>) {
    ACTOR_WORKSPACE.with(|slot| *slot.borrow_mut() = previous);
}

/// True when the calling thread runs inside an actor context (multi-actor
/// daemon). Callers that would otherwise touch process-global resources
/// (e.g. `std::env::set_current_dir`) must skip those mutations here —
/// process cwd is shared across all concurrent actors in the daemon.
pub fn is_actor_context() -> bool {
    ACTOR_SESSION.with(|slot| slot.borrow().is_some())
}

/// Returns the current session seed, preferring actor-local state.
pub fn current_session() -> Option<String> {
    let local = ACTOR_SESSION.with(|slot| slot.borrow().clone());
    if local.is_some() {
        return local;
    }
    CURRENT_SESSION
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// Returns the current workspace root, preferring actor-local state.
pub fn current_workspace() -> String {
    let local = ACTOR_WORKSPACE.with(|slot| slot.borrow().clone());
    if let Some(workspace) = local {
        return workspace;
    }
    CURRENT_WORKSPACE
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// Set the actor-local or process-wide cancel flag.
///
/// PR-3-4 解析顺序：actor 线程（有 actor session）写本地 Cell；否则按会话
/// 键控表；两者皆无（进程级路径，如 daemon shutdown）写全局 flag。per-call
/// 取消经 `ToolCallContext.cancellation` 的共享 Arc 传递（见 ToolManager
/// inflight 表），不再走线程局部。
pub fn set_cancel(value: bool) {
    if ACTOR_SESSION.with(|slot| slot.borrow().is_some()) {
        ACTOR_CANCEL.with(|slot| slot.set(value));
    } else if let Some(session) = bound_cancel_session() {
        set_session_cancel(&session, value);
    } else {
        CANCEL.store(value, std::sync::atomic::Ordering::SeqCst);
    }
}

/// 会话键控取消表（PR-3-4）：工具 worker 线程没有 actor context，旧的进程级
/// CANCEL 使一个会话的 interrupt 会误伤其它会话在途工具。按会话键控后互不
/// 干扰；全局 CANCEL 仅保留给无会话路径（daemon shutdown）。
static SESSION_CANCELS: LazyLock<Mutex<HashMap<String, bool>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Set the session-scoped cancel flag（会话级 interrupt 的规范入口；
/// registry interrupt / 权限挑战取消经此写入，不影响其它会话）。
pub fn set_session_cancel(session: &str, value: bool) {
    let mut guard = SESSION_CANCELS.lock().unwrap_or_else(|e| e.into_inner());
    guard.insert(session.to_string(), value);
}

/// Remove the session's cancel entry entirely（会话关闭后的规范清理入口）。
/// `set_session_cancel(_, false)` 只把值置假、表项仍在——种子频繁进出的
/// 场景（临时子代理、idle unload）会让 SESSION_CANCELS 无界增长；关闭
/// 路径整项移除，保证键控表规模与活跃会话数同阶。
pub fn remove_session_cancel(session: &str) {
    SESSION_CANCELS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(session);
}

fn session_cancelled(session: &str) -> bool {
    let guard = SESSION_CANCELS.lock().unwrap_or_else(|e| e.into_inner());
    guard.get(session).copied().unwrap_or(false)
}

/// 工具 worker 线程的会话归属解析钩子：由门面（qaqh-workspace runtime）在
/// `set_context` 首次调用时注册，读取执行路径绑定的 TLS ctx。本 crate 不依赖
/// 门面（原 `lib.rs ↔ runtime` 反向边由此打断）。
static CANCEL_SESSION_RESOLVER: RwLock<Option<fn() -> Option<String>>> = RwLock::new(None);

/// 注册会话归属解析钩子（幂等，后注册不覆盖先注册）。
pub fn set_cancel_session_resolver(resolver: fn() -> Option<String>) {
    let mut guard = CANCEL_SESSION_RESOLVER
        .write()
        .unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        *guard = Some(resolver);
    }
}

/// Resolve the session whose cancel state this thread is operating under:
/// actor session first（actor 线程），then the runtime ctx bound by the
/// execute path（工具 worker 线程，经 resolver 钩子），else None（进程级路径）。
fn bound_cancel_session() -> Option<String> {
    if let Some(local) = ACTOR_SESSION.with(|slot| slot.borrow().clone()) {
        return Some(local);
    }
    let resolver = CANCEL_SESSION_RESOLVER
        .read()
        .unwrap_or_else(|e| e.into_inner());
    resolver.and_then(|resolve| resolve())
}

/// Read the effective cancel flag: actor-local → session-keyed → process-wide.
pub fn is_cancel() -> bool {
    if ACTOR_SESSION.with(|slot| slot.borrow().is_some()) {
        return ACTOR_CANCEL.with(|slot| slot.get());
    }
    if let Some(session) = bound_cancel_session() {
        return session_cancelled(&session);
    }
    CANCEL.load(std::sync::atomic::Ordering::SeqCst)
}

/// 清除本线程所属会话的两层取消标记（actor 本地 + 进程全局 + 会话表项）。
/// C2：全局 CANCEL 曾被 daemon 非 actor 线程（registry interrupt 预置、
/// manager.cancel_tool(None)）置位，而清零点都在 actor 线程只写本地，
/// 导致全局 flag 一旦置位永无人复位、所有工具秒拒 Cancelled。
/// 所有清零路径必须走这里，保证各层同步归零；会话表项只清本线程所属
/// 会话，其它会话的取消状态不受影响（PR-3-4 隔离语义）。
pub fn clear_cancel() {
    ACTOR_CANCEL.with(|slot| slot.set(false));
    CANCEL.store(false, std::sync::atomic::Ordering::SeqCst);
    if let Some(session) = bound_cancel_session() {
        set_session_cancel(&session, false);
    }
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
    "exec",
    "process",
    // Todo v3：write/update 写类受 plan 阻断；todo_list 是读，plan 模式
    // 需要查看任务清单，不放名单。
    "todo_update",
    "todo_write",
];

pub fn set_workspace(path: &str) {
    if ACTOR_SESSION.with(|slot| slot.borrow().is_some()) {
        ACTOR_WORKSPACE.with(|slot| *slot.borrow_mut() = Some(path.to_string()));
        return;
    }
    let mut ws = CURRENT_WORKSPACE.write().unwrap_or_else(|e| e.into_inner());
    *ws = path.to_string();
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
        return crate::permission::normalize_lexically(p).to_string_lossy().to_string();
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
