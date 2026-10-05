//! Tool runtime state and ToolManager lifecycle.
//!
//! Knife-1 step 2: per-actor state moved from process-wide `static`s into
//! **thread-local** slots. Each in-process actor runs its Loop on its own daemon
//! thread and tool execution is synchronous on that actor thread, so
//! [`RUNTIME_CTX`], [`ACTOR_TOOL_MANAGER`], [`AGENT_MODE`], the sandbox flag and
//! the tool-result fold policy live per-thread and give concurrent actors real
//! isolation without `ACTOR_SERIAL`. The process-level [`TOOL_MANAGER`] stays as
//! the stable fallback for non-actor threads (daemon `skills.list_tools`,
//! CLI).

use qaqh_types::ToolDef;
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::tool_api::context::{SandboxMode, ToolCallContext};

/// Unified runtime security context used for session binding and admission.
#[derive(Clone)]
pub struct RuntimeContext {
    pub active_session: String,
    pub permission_level: u8,
}

thread_local! {
    static RUNTIME_CTX: std::cell::RefCell<Option<RuntimeContext>> = const { std::cell::RefCell::new(None) };
}

static TOOL_MANAGER: OnceLock<Mutex<crate::ToolManager>> = OnceLock::new();

// Optional in-process actor manager, **per actor thread**.
//
// When installed it shadows the process manager for every `with_manager`
// caller on that thread — including tool threads spawned by that actor's turn
// while they resolve tools on the actor thread. Each actor installs its own
// before its Loop and clears on exit, so concurrent actors do not share a
// tool allowlist or mutate each other's stats.
thread_local! {
    static ACTOR_TOOL_MANAGER: std::cell::RefCell<Option<Arc<Mutex<crate::ToolManager>>>> = const { std::cell::RefCell::new(None) };
}

// Agent operating mode: 0=Code(默认), 1=Plan, 2=Code(旧编码兼容). Per-actor.
thread_local! {
    static AGENT_MODE: Cell<u8> = const { Cell::new(0) };
}

// Explicit per-call cancellation override. This is installed only while a
// `ToolCallContext` is executing and is checked before the legacy actor/global
// flags, so handlers that still call `is_cancel()` observe the same token as
// the runtime-owned cancellation tree.
thread_local! {
    static TOOL_CALL_CANCEL: RefCell<Option<Arc<AtomicBool>>> = const { RefCell::new(None) };
}

pub fn set_context(session: &str, permission_level: u8) {
    RUNTIME_CTX.with(|ctx| {
        *ctx.borrow_mut() = Some(RuntimeContext {
            active_session: session.to_string(),
            permission_level,
        });
    });
}

pub fn clear_context() {
    RUNTIME_CTX.with(|ctx| *ctx.borrow_mut() = None);
}

pub fn context() -> Option<RuntimeContext> {
    RUNTIME_CTX.with(|ctx| ctx.borrow().clone())
}

pub fn set_mode(mode: u8) {
    AGENT_MODE.with(|slot| slot.set(mode));
}

/// Snapshot the current agent mode for an explicit runtime context.
pub fn current_mode() -> u8 {
    AGENT_MODE.with(|slot| slot.get())
}

pub(crate) fn explicit_cancel_flag() -> Option<Arc<AtomicBool>> {
    TOOL_CALL_CANCEL.with(|slot| slot.borrow().clone())
}

pub(crate) fn explicit_cancel_is_set() -> Option<bool> {
    explicit_cancel_flag().map(|flag| flag.load(Ordering::SeqCst))
}

/// Install one explicit [`ToolCallContext`] as the ambient compatibility view
/// for the current tool worker.
///
/// The explicit context is the source of truth. The thread-local slots are
/// populated only because legacy handlers still read them; the guard restores
/// every previous value on drop.
pub fn install_tool_call_context(ctx: &ToolCallContext) -> ToolCallContextGuard {
    let previous_runtime = RUNTIME_CTX.with(|slot| {
        let previous = slot.borrow().clone();
        *slot.borrow_mut() = Some(RuntimeContext {
            active_session: ctx.session_id.clone(),
            permission_level: ctx.permission_level as u8,
        });
        previous
    });
    let previous_workspace = crate::ACTOR_WORKSPACE.with(|slot| {
        slot.borrow_mut()
            .replace(ctx.workspace_root.to_string_lossy().to_string())
    });
    let previous_session =
        crate::ACTOR_SESSION.with(|slot| slot.borrow_mut().replace(ctx.session_id.clone()));
    let previous_actor_cancel = crate::ACTOR_CANCEL.with(|slot| slot.replace(false));
    let previous_mode = AGENT_MODE.with(|slot| {
        let previous = slot.get();
        slot.set(match ctx.mode {
            crate::tool_api::AgentMode::Code => 0,
            crate::tool_api::AgentMode::Plan => 1,
        });
        previous
    });
    let previous_sandbox = crate::authorization::is_subagent_sandbox();
    crate::authorization::set_subagent_sandbox(matches!(ctx.sandbox, SandboxMode::Subagent));
    let previous_cancel =
        TOOL_CALL_CANCEL.with(|slot| slot.borrow_mut().replace(ctx.cancellation.shared_flag()));

    ToolCallContextGuard {
        previous_runtime,
        previous_workspace,
        previous_session,
        previous_actor_cancel,
        previous_mode,
        previous_sandbox,
        previous_cancel,
    }
}

/// Restores the thread-local compatibility view captured by
/// [`install_tool_call_context`].
pub struct ToolCallContextGuard {
    previous_runtime: Option<RuntimeContext>,
    previous_workspace: Option<String>,
    previous_session: Option<String>,
    previous_actor_cancel: bool,
    previous_mode: u8,
    previous_sandbox: bool,
    previous_cancel: Option<Arc<AtomicBool>>,
}

impl Drop for ToolCallContextGuard {
    fn drop(&mut self) {
        RUNTIME_CTX.with(|slot| *slot.borrow_mut() = self.previous_runtime.take());
        crate::ACTOR_WORKSPACE.with(|slot| {
            *slot.borrow_mut() = self.previous_workspace.take();
        });
        crate::ACTOR_SESSION.with(|slot| {
            *slot.borrow_mut() = self.previous_session.take();
        });
        crate::ACTOR_CANCEL.with(|slot| slot.set(self.previous_actor_cancel));
        AGENT_MODE.with(|slot| slot.set(self.previous_mode));
        crate::authorization::set_subagent_sandbox(self.previous_sandbox);
        TOOL_CALL_CANCEL.with(|slot| {
            *slot.borrow_mut() = self.previous_cancel.take();
        });
    }
}

/// Worker scope carrying one explicit tool context plus the non-SDK runtime
/// state that still lives in thread-local storage (ToolManager and fold
/// policy).
///
/// The explicit context remains authoritative for
/// workspace/session/mode/sandbox/cancellation; manager and policy are
/// captured on the actor thread and restored on the worker.
#[derive(Clone)]
pub struct ToolExecutionScope {
    context: ToolCallContext,
    manager: Option<Arc<Mutex<crate::ToolManager>>>,
    policy: Arc<dyn crate::tool_side_fold::ToolResultFoldPolicy>,
}

impl ToolExecutionScope {
    /// Capture the actor-thread manager/policy and bind them to an explicit
    /// tool context.
    pub fn capture(context: ToolCallContext) -> Self {
        Self {
            context,
            manager: ACTOR_TOOL_MANAGER.with(|slot| slot.borrow().clone()),
            policy: crate::tool_side_fold::policy(),
        }
    }

    /// Explicit context bound to this worker scope.
    pub fn context(&self) -> &ToolCallContext {
        &self.context
    }

    /// Install the manager/policy compatibility view and the explicit tool
    /// context on the current thread.
    pub fn install(&self) -> ToolExecutionScopeGuard {
        let previous_manager = ACTOR_TOOL_MANAGER
            .with(|slot| std::mem::replace(&mut *slot.borrow_mut(), self.manager.clone()));
        let previous_policy = crate::tool_side_fold::policy();
        crate::tool_side_fold::set_thread_policy(self.policy.clone());
        let context_guard = install_tool_call_context(&self.context);
        ToolExecutionScopeGuard {
            _context_guard: context_guard,
            previous_manager,
            previous_policy,
        }
    }
}

/// Restores manager/policy/context state captured by
/// [`ToolExecutionScope::install`].
pub struct ToolExecutionScopeGuard {
    _context_guard: ToolCallContextGuard,
    previous_manager: Option<Arc<Mutex<crate::ToolManager>>>,
    previous_policy: Arc<dyn crate::tool_side_fold::ToolResultFoldPolicy>,
}

impl Drop for ToolExecutionScopeGuard {
    fn drop(&mut self) {
        ACTOR_TOOL_MANAGER.with(|slot| {
            *slot.borrow_mut() = self.previous_manager.take();
        });
        crate::tool_side_fold::set_thread_policy(self.previous_policy.clone());
    }
}

/// Explicit tool-execution context (PR-3-2 / D5-G2): the caller (agent tool
/// dispatch, CLI) assembles one per execution instead of
/// mutating process/thread state first. Threaded through the execute path;
/// the workspace installs it for the duration of the call and restores the
/// previous ambient state on drop.
#[derive(Clone, Debug)]
pub struct ToolCtx {
    pub session_id: String,
    pub permission_level: u8,
    /// Agent operating mode (0=Code, 1=Plan, 2=Code legacy alias) recorded at dispatch time.
    pub mode: u8,
    /// Workspace root for this execution. `None` = keep the current process
    /// workspace (the in-process agent path).
    pub workspace_root: Option<String>,
}

impl ToolCtx {
    /// Context for an already-admitted caller (CLI/tests): full permission,
    /// Code mode, current process workspace.
    pub fn admitted(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            permission_level: 3,
            mode: 0,
            workspace_root: None,
        }
    }
}

/// RAII guard restoring the thread's previous ambient context.
pub struct ToolCtxGuard {
    previous: Option<RuntimeContext>,
    restore_mode: u8,
}

impl Drop for ToolCtxGuard {
    fn drop(&mut self) {
        RUNTIME_CTX.with(|ctx| *ctx.borrow_mut() = self.previous.take());
        AGENT_MODE.with(|slot| slot.set(self.restore_mode));
    }
}

/// Install `ctx` as the ambient runtime context for this thread until the
/// returned guard drops. The execute path calls this so tool handlers can
/// keep reading ambient state while the *caller* stays explicit.
pub fn install_tool_ctx(ctx: &ToolCtx) -> ToolCtxGuard {
    let previous = RUNTIME_CTX.with(|slot| {
        let previous = slot.borrow().clone();
        *slot.borrow_mut() = Some(RuntimeContext {
            active_session: ctx.session_id.clone(),
            permission_level: ctx.permission_level,
        });
        previous
    });
    let restore_mode = AGENT_MODE.with(|slot| {
        let previous = slot.get();
        slot.set(ctx.mode);
        previous
    });
    ToolCtxGuard {
        previous,
        restore_mode,
    }
}

/// Bind only the active session (permission stays unknown post-admission).
/// Used by `execute_authorized` where the [`AuthorizedToolCall`](crate::AuthorizedToolCall)
/// itself is the execution context.
pub fn bind_session(session_id: &str) -> ToolCtxGuard {
    install_tool_ctx(&ToolCtx {
        session_id: session_id.to_string(),
        permission_level: 0,
        mode: AGENT_MODE.with(|slot| slot.get()),
        workspace_root: None,
    })
}

/// 运行时重设工具白名单（工具模式 Standard/Minimal/Custom 的入口）。
/// 空列表 = 全量（标准模式）；未知名自动剔除并 warn（复用 apply_init 语义）。
pub fn set_allowed_tools(tools: Vec<String>) {
    with_manager(|manager| manager.set_allowed(tools));
}

/// Snapshot of the legacy per-actor tool runtime state on the actor thread.
///
/// Production tool workers now carry [`ToolExecutionScope`], which binds an
/// explicit `ToolCallContext` to the remaining manager/policy state. This
/// compatibility type is retained for legacy callers and thread-boundary tests.
#[derive(Clone, Default)]
pub struct ActorToolScope {
    runtime: Option<RuntimeContext>,
    manager: Option<Arc<Mutex<crate::ToolManager>>>,
    mode: u8,
    sandbox: bool,
    /// 会话工作区快照（BUG-2026-09-12-05）：派生工具线程不继承 actor 线程
    /// 的 ACTOR_WORKSPACE thread-local，不搬运则 grep/glob/read 相对路径与
    /// exec 缺省 cwd 全部锚定到 daemon 进程 cwd。capture 在 actor 线程上取
    /// `current_workspace()` 快照，install 时经 push/pop 写入工具线程。
    workspace: Option<String>,
    /// 工具结果折叠策略（Standard / NoFold）。随工具模式按会话切换，因此必须
    /// 跟着 actor 走——留在进程级 static 会让一个会话的 minimal 模式关掉其它
    /// 会话的命令输出截断。
    policy: Option<Arc<dyn crate::tool_side_fold::ToolResultFoldPolicy>>,
}

impl ActorToolScope {
    /// Capture the current (actor) thread's per-actor tool state.
    pub fn capture() -> Self {
        Self {
            runtime: context(),
            manager: ACTOR_TOOL_MANAGER.with(|slot| slot.borrow().clone()),
            mode: AGENT_MODE.with(|slot| slot.get()),
            sandbox: crate::authorization::is_subagent_sandbox(),
            policy: Some(crate::tool_side_fold::policy()),
            workspace: Some(crate::current_workspace()),
        }
    }

    /// Install this scope onto the current thread (a spawned tool worker),
    /// restoring the caller's previous thread-local state when the guard drops.
    pub fn install(&self) -> ActorToolScopeGuard {
        let previous = Self::capture();
        RUNTIME_CTX.with(|slot| *slot.borrow_mut() = self.runtime.clone());
        ACTOR_TOOL_MANAGER.with(|slot| *slot.borrow_mut() = self.manager.clone());
        AGENT_MODE.with(|slot| slot.set(self.mode));
        crate::authorization::set_subagent_sandbox(self.sandbox);
        // 会话工作区跨线程搬运（BUG-2026-09-12-05）：push 返回被覆盖的旧值，
        // Drop 时经 pop 恢复，保证 guard 语义与其它 thread-local 对称。
        let previous_workspace = crate::push_thread_workspace(self.workspace.clone());
        if let Some(policy) = &self.policy {
            crate::tool_side_fold::set_thread_policy(policy.clone());
        }
        ActorToolScopeGuard {
            previous,
            previous_workspace: Some(previous_workspace),
        }
    }
}

/// Restores the pre-install thread-local state on drop.
pub struct ActorToolScopeGuard {
    previous: ActorToolScope,
    /// install 时被覆盖的本线程旧 workspace 值（push 的返回值），Drop 恢复。
    previous_workspace: Option<Option<String>>,
}

impl Drop for ActorToolScopeGuard {
    fn drop(&mut self) {
        RUNTIME_CTX.with(|slot| *slot.borrow_mut() = self.previous.runtime.clone());
        ACTOR_TOOL_MANAGER.with(|slot| *slot.borrow_mut() = self.previous.manager.clone());
        AGENT_MODE.with(|slot| slot.set(self.previous.mode));
        crate::authorization::set_subagent_sandbox(self.previous.sandbox);
        if let Some(previous_workspace) = self.previous_workspace.take() {
            crate::pop_thread_workspace(previous_workspace);
        }
        if let Some(policy) = &self.previous.policy {
            crate::tool_side_fold::set_thread_policy(policy.clone());
        }
    }
}

/// Initialize the process-global tool manager.
pub fn init_tools(
    session_id: &str,
    extra_registrars: &[crate::registration::ToolRegistrar],
    allowed_tools: Vec<String>,
) {
    let mut manager = crate::registration::build_tool_manager(extra_registrars);
    manager.apply_init(allowed_tools, session_id);
    let _ = TOOL_MANAGER.set(Mutex::new(manager));
    crate::file_cache::clear();
    crate::file_state::clear();
    log::info!("qaqh: tool manager inited ({} tools)", all_tools().len());
}

/// Install a private manager for one in-process actor (per-actor thread-local).
///
/// Unlike [`init_tools`], this does not mutate the daemon/worker process
/// manager. The caller is responsible for clearing it with
/// [`clear_actor_tool_manager`] when the actor exits. Because it is
/// thread-local, concurrent actors each get their own manager.
pub fn install_actor_tool_manager(manager: crate::ToolManager) {
    ACTOR_TOOL_MANAGER.with(|slot| {
        *slot.borrow_mut() = Some(Arc::new(Mutex::new(manager)));
    });
    crate::file_cache::clear();
    crate::file_state::clear();
    log::info!("qaqh: in-process actor tool manager installed");
}

/// Remove the in-process actor manager, falling back to the process manager.
/// Call on the same actor thread as [`install_actor_tool_manager`].
pub fn clear_actor_tool_manager() {
    ACTOR_TOOL_MANAGER.with(|slot| {
        *slot.borrow_mut() = None;
    });
    crate::file_cache::clear();
    crate::file_state::clear();
    log::info!("qaqh: in-process actor tool manager cleared");
}

pub(crate) fn with_manager<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&mut crate::ToolManager) -> R,
{
    let actor_mgr = ACTOR_TOOL_MANAGER.with(|slot| slot.borrow().clone());
    if let Some(actor_mgr) = actor_mgr {
        let mut guard = lock_manager(&actor_mgr);
        return Some(f(&mut guard));
    }
    let mgr = TOOL_MANAGER.get()?;
    let mut guard = lock_manager(mgr);
    Some(f(&mut guard))
}

fn lock_manager(
    manager: &Mutex<crate::ToolManager>,
) -> std::sync::MutexGuard<'_, crate::ToolManager> {
    match manager.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            log::warn!("[TOOLS] ToolManager Mutex poisoned — recovering with into_inner()");
            poisoned.into_inner()
        }
    }
}

#[cfg(test)]
pub(crate) fn register_test_handler(handler: crate::ToolHandler) {
    with_manager(|manager| manager.register(handler));
}

/// Return the canonical workspace root used for authorization and execution.
pub(crate) fn active_workspace_root() -> PathBuf {
    let workspace = crate::current_workspace();
    let root = if workspace.is_empty() || workspace == "." {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    } else {
        PathBuf::from(workspace)
    };
    crate::permission::resolve_target_path(root)
}

pub fn all_tools() -> Vec<ToolDef> {
    let defs = with_manager(|manager| manager.filtered_defs()).unwrap_or_default();
    if image_tool_enabled() {
        defs
    } else {
        // 端点不支持视觉输入（未声明 supports_image_tool）时，
        // read_image 不进入模型工具清单。
        defs.into_iter()
            .filter(|def| def.function.name != "read_image")
            .collect()
    }
}

/// 回合边界 MCP 动态层全量重建（M1-5；设计 §5.3 refresh 语义）。
///
/// 调用方：qaqh-runtime 在 run_lap 开头拿到 qaqh-mcp 投影批次后调用——
/// 先 `clear_dynamic` 再逐条 `register_dynamic`（碰撞拒绝的批次条目计数
/// 跳过并告警，不阻断其余）。必须在与 [`install_actor_tool_manager`]
/// 相同的 actor 线程上调用（thread-local manager）。返回被拒绝的条目数。
pub fn replace_dynamic_tools(batch: Vec<(String, crate::DynamicTool)>) -> usize {
    with_manager(|manager| {
        manager.clear_dynamic();
        let mut rejected = 0usize;
        for (name, tool) in batch {
            if manager.register_dynamic(name.clone(), tool).is_err() {
                rejected += 1;
                log::warn!("[TOOLS] dynamic registration rejected (collision): {name:?}");
            }
        }
        // PR-M2-2 观察项 ①：动态层换名后用 raw 原始名单重过滤——custom/minimal
        // 工具模式白名单中含 MCP 工具名时，refresh 后不再静默失效。
        manager.reapply_allowed_after_dynamic_change();
        rejected
    })
    .unwrap_or(0)
}

/// LSP 动态层增量合并（`lsp` 聚合工具 enabled 即在场；不清现有 dynamic——
/// MCP per-server 工具不受影响）。碰撞拒绝计数跳过并告警。调用方与
/// [`replace_dynamic_tools`] 同线程约束。返回被拒绝的条目数。
pub fn merge_dynamic_tools(batch: Vec<(String, crate::DynamicTool)>) -> usize {
    with_manager(|manager| {
        let mut rejected = 0usize;
        for (name, tool) in batch {
            // 已在册同名（热重载/重复 take）→ 跳过，不计拒绝（幂等）。
            if manager.dynamic_names().iter().any(|n| n == &name) {
                continue;
            }
            if manager.register_dynamic(name.clone(), tool).is_err() {
                rejected += 1;
                log::warn!("[TOOLS] dynamic merge rejected (collision): {name:?}");
            }
        }
        manager.reapply_allowed_after_dynamic_change();
        rejected
    })
    .unwrap_or(0)
}

/// 当前配置的 provider endpoint 是否接受图片输入（read_image 工具开关）。
///
/// PR-1-10 / D2：能力快照由宿主注入（[`set_image_capability`]：daemon
/// 未注入时（单元测试 / 未装配进程）默认放行——工具可见性交给注册方，
/// 执行路径的自然错误兜底真实不支持的场景。
pub fn image_tool_enabled() -> bool {
    image_caps().map(|c| c.endpoint).unwrap_or(true)
}

/// 当前 (provider, endpoint, model) 组合是否接受图片输入。
///
/// 比端点级 [`image_tool_enabled`] 更精确：路由器端点（如 OpenRouter）的
/// 模型异构，文本-only 模型需要在此处被拒绝，而不是让带图请求打到上游
/// 换回一个不透明的 400。快照语义同上（PR-1-10）。
pub fn image_model_supported() -> bool {
    image_caps().map(|c| c.model).unwrap_or(true)
}

#[derive(Clone, Copy)]
struct ImageCaps {
    endpoint: bool,
    model: bool,
}

static IMAGE_CAPS: Mutex<Option<ImageCaps>> = Mutex::new(None);

/// 注入图片能力快照（PR-1-10 / D2）。宿主在装配 / reload 时
/// 以当前配置计算后调用；快照存活期内工具调用路径不再触碰磁盘。
pub fn set_image_capability(endpoint_enabled: bool, model_supported: bool) {
    *IMAGE_CAPS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(ImageCaps {
        endpoint: endpoint_enabled,
        model: model_supported,
    });
}

fn image_caps() -> Option<ImageCaps> {
    *IMAGE_CAPS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 按工具名调用作者声明的展示投影（09-18 展示契约 §3.4）。
///
/// 未注册投影、或当前线程没有可用 manager 时返回 `None`；调用方必须完整
/// 回退旧字段（H16），不得自行解析工具输出。
pub fn project_tool_display(
    name: &str,
    args: &serde_json::Value,
    output: &str,
) -> Option<crate::tool_api::ToolDisplay> {
    with_manager(|manager| manager.project_display(name, args, output)).flatten()
}

/// 优先从 typed canonical payload 生成 display，失败时回退旧输出投影。
pub fn project_tool_display_from_result(
    name: &str,
    args: &serde_json::Value,
    result: &qaqh_types::ToolResult,
) -> Option<crate::tool_api::ToolDisplay> {
    result
        .display()
        .map(crate::tool_api::output::from_wire_display)
        .or_else(|| crate::display::project_typed_tool_display(name, args, &result.data))
        .or_else(|| project_tool_display(name, args, result.model_text()))
}

/// Rehydrate a canonical display payload without re-parsing tool text.
pub fn project_tool_display_from_wire(
    display: &qaqh_types::ToolResultDisplay,
) -> crate::tool_api::ToolDisplay {
    crate::tool_api::output::from_wire_display(display)
}

/// 查询 handler 声明的能力类别（权限决策单一事实源）。
/// 未注册/未初始化返回 None——调用方回退保守默认（Write）。
pub fn lookup_category(name: &str) -> Option<crate::permission::ToolCategory> {
    // 内置 + 动态（MCP）两层：S3 沙箱按 category 拒绝必须覆盖 MCP 工具。
    with_manager(|manager| manager.category_of(name)).flatten()
}

/// Tool names from the **process** manager, ignoring any installed actor
/// manager. Used by daemon-side snapshots (e.g. `skills.list_tools`) that must
/// stay stable while an in-process subagent actor temporarily shadows the
/// manager for its own tool execution.
pub fn process_all_tool_names() -> Vec<String> {
    let Some(manager) = TOOL_MANAGER.get() else {
        return Vec::new();
    };
    let guard = lock_manager(manager);
    guard
        .all_defs()
        .iter()
        .map(|definition| definition.function.name.clone())
        .collect()
}

pub fn global_stats() -> crate::ToolStats {
    with_manager(|manager| manager.stats()).unwrap_or_default()
}

pub fn files_read() -> Vec<String> {
    global_stats().files_read
}

pub fn files_written() -> Vec<String> {
    global_stats().files_written
}

pub fn cancel_current_tool() {
    with_manager(|manager| manager.cancel_tool(None));
}

pub fn shutdown_tools() {
    log::info!("qaqh: tool manager shut down");
}
