//! ToolManager 初始化构造器。
//!
//! 各模块的 `register()` 在此组装。外部注册器通过 `extra_registrars` 注入。

use super::ToolManager;
use super::exec;
use super::web;

use super::apply_patch;
use super::file_mutate;
use super::file_query;

use super::ask_user;
use super::process_inspect;
use super::task;
use super::todo;

use super::skill;

/// 工具注册器函数签名。
pub type ToolRegistrar = fn(&mut ToolManager);

/// 构造并注册全部工具 handler，返回初始化后的 ToolManager。
/// `extra_registrars` 允许外部 crate（如 deepx-subagent）注入工具。
pub fn build_tool_manager(extra_registrars: &[ToolRegistrar]) -> ToolManager {
    let mut mgr = ToolManager::new();

    // ── 系统工具 ──
    exec::register(&mut mgr);
    web::register(&mut mgr);

    // ── 文件操作 ──
    apply_patch::register(&mut mgr);
    file_mutate::register(&mut mgr);
    file_query::register(&mut mgr);

    // ── 任务 (已废弃，请使用 todo) ──
    task::register(&mut mgr);

    // ── Todo (统一 task/plan/goal) ──
    todo::register(&mut mgr);

    // ── 交互 ──
    ask_user::register(&mut mgr);

    // ── 进程巡查 ──
    process_inspect::register(&mut mgr);

    // ── Agent Skills ──
    skill::register(&mut mgr);

    // ── Memory (跨会话记忆) ──
    #[cfg(feature = "memory")]
    crate::memory::register(&mut mgr);

    // ── 外部注册器 ──
    for reg in extra_registrars {
        reg(&mut mgr);
    }

    mgr
}
