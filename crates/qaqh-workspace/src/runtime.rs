//! ToolManager lifecycle + process-manager access.
//!
//! P0-2 终局形态：本模块**零线程局部状态、零 ambient 视图**。旧的
//! `RUNTIME_CTX` / `ACTOR_TOOL_MANAGER` / `ToolExecutionScope::install()`
//! （每次工具调用人工维护 5 个 TLS 槽位的兼容视图）与 in-process actor
//! 残留（`install_actor_tool_manager`，生产调用者为零）一并退役——生产执行
//! 链是 `WorkspaceToolPort` 持有会话私有 `ToolManager` 并显式构造
//! `ToolCallContext`（见 `execution::execute_authorized_with_context`）。
//! 本模块只剩进程级快照面（daemon `skills.list_tools` 等基础设施查询用的
//! `TOOL_MANAGER`）与装配入口 [`init_tools`]。
//!
//! serve/CLI 的显式入口参数类型 [`ToolCtx`] 留守于此（它本身不是线程态——
//! 是调用方逐调用组装的参数；`sandbox` 字段 P0-2 起显式携带，不再读线程态）。

use qaqh_types::ToolDef;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

static TOOL_MANAGER: OnceLock<Mutex<crate::ToolManager>> = OnceLock::new();

pub(crate) fn with_manager<F, R>(f: F) -> Option<R>
where
    F: FnOnce(&mut crate::ToolManager) -> R,
{
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

/// 进程快照 manager 的裸访问（serve/CLI 显式入口用；P0-2 后这是 with_manager
/// 的唯一后端）。未初始化返回 `None`。
pub fn process_manager() -> Option<&'static Mutex<crate::ToolManager>> {
    TOOL_MANAGER.get()
}

#[cfg(test)]
pub(crate) fn register_test_probe(probe: crate::probe::ProbeTool) {
    with_manager(|manager| manager.register_probe(probe));
}

/// 运行时重设工具白名单（工具模式 Standard/Minimal/Custom 的入口）。
/// 空列表 = 全量（标准模式）；未知名自动剔除并 warn（复用 apply_init 语义）。
pub fn set_allowed_tools(tools: Vec<String>) {
    with_manager(|manager| manager.set_allowed(tools));
}

/// Initialize the process-global tool manager.
pub fn init_tools(
    session_id: &str,
    extra_registrars: &[crate::registration::ToolRegistrar],
    allowed_tools: Vec<String>,
    todo_host: std::sync::Arc<dyn crate::todo::TodoHost>,
) {
    let mut manager = crate::registration::build_tool_manager(extra_registrars, todo_host);
    // `session_id` 只用于日志：装配不写任何进程级/线程级会话状态。
    let _ = session_id;
    manager.apply_init(allowed_tools);
    let _ = TOOL_MANAGER.set(Mutex::new(manager));
    log::info!("qaqh: tool manager inited ({} tools)", all_tools().len());
}

/// Return the canonical workspace root used for authorization and execution.
///
/// P0-2：原形读线程态 `current_workspace()`；显式化后仅剩 serve/CLI 入口的
/// 「进程工作目录」兜底语义——`workspace_root` 为空或 "." 时回落 cwd。
pub(crate) fn active_workspace_root() -> PathBuf {
    let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    crate::permission::resolve_target_path(root)
}

pub fn all_tools() -> Vec<ToolDef> {
    with_manager(|manager| manager.filtered_defs()).unwrap_or_default()
}

/// MCP 动态层全量重建（M1-5；设计 §5.3 refresh 语义）。
///
/// 调用方：投影回归测试与进程快照面。根会话模型面不走本函数——回合边界经
/// `WorkspaceToolPort::apply_dynamic_projection` 直接重建会话私有 manager。
/// 返回被拒绝的条目数。
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

/// 按工具名调用作者声明的展示投影（09-18 展示契约 §3.4）。
///
/// 未注册投影、或进程快照 manager 未初始化时返回 `None`；调用方必须完整
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
    project_tool_display_from_result_payload(name, result)
        .or_else(|| project_tool_display(name, args, result.model_text()))
}

/// Project display from structured result data without decoding the model-call
/// arguments. Legacy display projectors can still use
/// [`project_tool_display_from_result`] as a compatibility fallback.
pub fn project_tool_display_from_result_payload(
    name: &str,
    result: &qaqh_types::ToolResult,
) -> Option<crate::tool_api::ToolDisplay> {
    result
        .display()
        .map(crate::tool_api::output::from_wire_display)
        .or_else(|| crate::display::project_typed_tool_display(name, &result.data))
}

/// 查询 handler 声明的能力类别（权限决策单一事实源）。
/// 未注册/未初始化返回 None——调用方回退保守默认（Write）。
pub fn lookup_category(name: &str) -> Option<crate::permission::ToolCategory> {
    // 内置 + 动态（MCP）两层：S3 沙箱按 category 拒绝必须覆盖 MCP 工具。
    with_manager(|manager| manager.category_of(name)).flatten()
}

/// Tool names from the **process** manager. Used by daemon-side snapshots
/// (e.g. `skills.list_tools`) that must stay stable while session ports run
/// their own managers.
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

/// serve/CLI 的显式工具执行入口参数（PR-3-2 / D5-G2）：调用方逐调用组装，
/// 不改任何进程/线程状态。P0-2：`sandbox` 显式携带（原形读线程态
/// `SUBAGENT_SANDBOX`——兼容视图已退役）。
#[derive(Clone, Debug)]
pub struct ToolCtx {
    pub session_id: String,
    pub permission_level: u8,
    /// Workspace root for this execution. `None` = 进程 cwd（CLI 场景）。
    pub workspace_root: Option<String>,
    /// 生效沙箱模式。`None` = Main。
    pub sandbox: Option<crate::tool_api::SandboxMode>,
}

impl ToolCtx {
    /// Context for an already-admitted caller (CLI/tests): full permission,
    /// current process workspace, Main sandbox.
    pub fn admitted(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            permission_level: 3,
            workspace_root: None,
            sandbox: None,
        }
    }
}
