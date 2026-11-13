//! ToolManager 初始化构造器。
//!
//! 各 Team 的 `register()` 在此组装。D01 可后续按需调整注册顺序或增加全局配置。

use super::ToolManager;
use super::exec;
use super::explore;
use super::web;
use super::file;
use super::task;
use super::plan;


/// 构造并注册全部工具 handler，返回初始化后的 ToolManager。
pub fn build_tool_manager() -> ToolManager {
    let mut mgr = ToolManager::new();

    // ── Team D01: 系统工具 ──
    exec::register(&mut mgr);
    explore::register(&mut mgr);
    web::register(&mut mgr);

    // ── Team D02: 文件操作 ──
    file::register(&mut mgr);

    // ── Team D03: 任务/计划 ──
    task::register(&mut mgr);
    plan::register(&mut mgr);


    mgr
}
