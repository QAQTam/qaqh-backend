//! ToolManager framework — tool registration, execution, and lifecycle.
//!
//! Submodules register handlers via `pub fn register(mgr: &mut ToolManager)`.

pub mod confirm_apply;
pub mod conflict;
pub mod copy_range;
pub mod dashboard;
pub mod display;
pub mod exec;
pub mod grep_tool;
pub mod pending;

pub mod apply_patch;
pub mod apply_patch_engine;

pub mod arg_estimate;
pub mod authorization;
mod code_delta;
pub mod edit;
pub mod execution;
pub mod file_cache;
pub mod file_glob;
pub mod file_mutate;
pub mod file_query;
pub mod file_shared;
pub mod file_state;
pub mod git;
pub mod read_image;
pub mod runtime;
mod safety;
pub mod skill;
pub mod tool_api;
pub mod tool_capabilities;
pub mod tool_side_fold;
mod web;

pub mod ask_user;

pub mod todo;

pub mod process_inspect;
pub mod process_registry;
pub mod workspace;

pub mod registration;

#[cfg(any(test, feature = "test-harness"))]
pub mod probe;

pub mod manager;
/// Permission engine: tool categories, levels, trusted folders.
pub mod permission;

pub mod audit;

pub mod journal;
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
    classify_risk, extract_target_paths, is_sensitive_session_path, normalize_lexically,
    patch_target_paths, path_within_dir, resolve_target_path,
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

/// Build a JSON success response for tools that only need a status message.
/// Extra fields can be added via `extra`.
pub fn json_ok(extra: serde_json::Value) -> String {
    let mut v = serde_json::json!({"timeis": now_utc8(), "status": "ok"});
    if let Some(obj) = v.as_object_mut() {
        if let Some(ext) = extra.as_object() {
            for (k, val) in ext {
                obj.insert(k.clone(), val.clone());
            }
        } else if !extra.is_null() {
            obj.insert("content".to_string(), extra);
        }
    }
    v.to_string()
}

/// Build a structured error [`ToolResult`] (canonical `ToolError` fields:
/// code / message / retryable / hint). Historic name retained; the legacy
/// JSON-envelope string form only survives in `todo.rs` (its `Err(String)`
/// channel crosses into `qaqh-runtime::service` — see `todo_err` there).
pub fn json_err(
    code: impl Into<String>,
    message: impl Into<String>,
    hint: impl Into<String>,
) -> ToolResult {
    let hint = hint.into();
    ToolResult::error_with(code, message, false, Some(hint).filter(|h| !h.is_empty()))
}

/// Legacy string-JSON error envelope — ONLY for functions whose error channel
/// is `String` (todo.rs `Err(String)` crosses into `qaqh-runtime::service`;
/// web.rs `web_fetch` returns `String`). Do not use in new code: prefer
/// [`json_err`] which returns a structured [`ToolResult`].
pub fn json_err_string(
    code: impl Into<String>,
    message: impl Into<String>,
    hint: impl Into<String>,
) -> String {
    serde_json::json!({
        "timeis": now_utc8(),
        "status": "error",
        "code": code.into(),
        "message": message.into(),
        "hint": hint.into(),
    })
    .to_string()
}

/// Risk level for tool operations, replacing per-handler safety functions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolRisk {
    ReadOnly,
    Write,
    Destructive,
    Administrative,
}

// ── JsonArgs trait: typed access to tool arguments ──

pub trait JsonArgs {
    fn s(&self, key: &str) -> String;
    fn s_or(&self, key: &str, default: &str) -> String;
    fn opt_bool(&self, key: &str) -> Option<bool>;
}

impl JsonArgs for serde_json::Value {
    fn s(&self, key: &str) -> String {
        self.get(key)
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_default()
    }
    fn s_or(&self, key: &str, default: &str) -> String {
        self.get(key)
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| default.to_string())
    }
    fn opt_bool(&self, key: &str) -> Option<bool> {
        let val = self.get(key)?;
        val.as_bool()
            .or_else(|| val.as_str().and_then(|s| s.parse::<bool>().ok()))
    }
}

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{LazyLock, Mutex, RwLock};

pub use qaqh_types::{
    ContentRef, ToolContinuation, ToolError as CanonicalToolError, ToolModelPayload, ToolResult,
    ToolStatus,
};

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

/// Resolve the session whose cancel state this thread is operating under:
/// actor session first（actor 线程），then the runtime ctx bound by the
/// execute path（工具 worker 线程），else None（进程级路径）。
fn bound_cancel_session() -> Option<String> {
    if let Some(local) = ACTOR_SESSION.with(|slot| slot.borrow().clone()) {
        return Some(local);
    }
    crate::runtime::context().map(|ctx| ctx.active_session)
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

/// A single non-blocking execution-output update for the frontend.
///
/// `seq` is monotonic per execution and represents the order in which pipe
/// reader threads observed chunks.  Cross-stream ordering is therefore local
/// observation order, while ordering within a stream is exact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecProgressEvent {
    pub tool_call_id: String,
    pub stream: ExecOutputStream,
    pub seq: u64,
    pub chunk: String,
    /// 截至本帧的累计观测字节（成功入队 + 被有界 channel 丢弃）。
    /// 由 [`ExecProgressSender`] 统一填写，构造方填 0 即可。
    pub bytes_total: u64,
}

/// 进度字节总量句柄：与 sender 共享计数器；sender 全部 drop 后仍可读终值。
#[derive(Clone, Debug, Default)]
pub struct ExecProgressTotals {
    emitted: std::sync::Arc<std::sync::atomic::AtomicU64>,
    dropped: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl ExecProgressTotals {
    /// 累计观测字节（09-18 展示契约 §5.1：含被丢弃与被尾部裁剪的部分）。
    pub fn total_bytes(&self) -> u64 {
        self.emitted.load(std::sync::atomic::Ordering::Relaxed)
            + self.dropped.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn dropped_bytes(&self) -> u64 {
        self.dropped.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecOutputStream {
    Stdout,
    Stderr,
}

impl ExecOutputStream {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
}

/// Bounded, lossy progress sender. Pipe readers must never wait for a slow UI.
#[derive(Clone)]
pub struct ExecProgressSender {
    tx: std::sync::mpsc::SyncSender<ExecProgressEvent>,
    emitted_bytes: std::sync::Arc<std::sync::atomic::AtomicU64>,
    dropped_bytes: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl ExecProgressSender {
    pub fn try_send(&self, mut event: ExecProgressEvent) {
        let bytes = event.chunk.len() as u64;
        let emitted = self
            .emitted_bytes
            .fetch_add(bytes, std::sync::atomic::Ordering::Relaxed)
            + bytes;
        event.bytes_total = emitted
            + self
                .dropped_bytes
                .load(std::sync::atomic::Ordering::Relaxed);
        if self.tx.try_send(event).is_err() {
            // 通道满：这批字节未被消费，从 emitted 挪到 dropped（总量口径不变）。
            self.emitted_bytes
                .fetch_sub(bytes, std::sync::atomic::Ordering::Relaxed);
            self.dropped_bytes
                .fetch_add(bytes, std::sync::atomic::Ordering::Relaxed);
        }
    }

    pub fn dropped_bytes(&self) -> u64 {
        self.dropped_bytes
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 与本 sender 共享计数的总量句柄（供 runtime 在 seal 时读终值）。
    pub fn totals(&self) -> ExecProgressTotals {
        ExecProgressTotals {
            emitted: self.emitted_bytes.clone(),
            dropped: self.dropped_bytes.clone(),
        }
    }
}

pub const EXEC_PROGRESS_CHANNEL_CAPACITY: usize = 256;

pub fn bounded_exec_progress_channel() -> (
    ExecProgressSender,
    std::sync::mpsc::Receiver<ExecProgressEvent>,
) {
    let (tx, rx) = std::sync::mpsc::sync_channel(EXEC_PROGRESS_CHANNEL_CAPACITY);
    let dropped_bytes = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let emitted_bytes = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    (
        ExecProgressSender {
            tx,
            emitted_bytes,
            dropped_bytes,
        },
        rx,
    )
}

/// Trusted, typed state transitions emitted by tool handlers.
///
/// Keeping this wrapper generic lets the runtime add other effect families
/// without widening the textual tool-result protocol.
#[derive(Clone, Debug)]
pub enum ToolEffect {
    Skill(qaqh_skills::SkillEffect),
    /// A subagent actor has been created and is waiting for its canonical
    /// `SubagentSpawned` fact before the task is delivered.
    SubagentSpawned {
        session_id: String,
        child_session_id: String,
        name: String,
        task_text: String,
        timeout_secs: u64,
        parent_session_id: String,
        parent_agent_path: String,
        child_agent_path: String,
        process_id: u32,
        spawn_tools: Vec<String>,
        spawn_model: Option<String>,
        spawn_base_url: Option<String>,
        spawn_max_tokens: Option<u32>,
        spawn_ephemeral: bool,
        spawn_timeout_secs: u64,
    },
}

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
