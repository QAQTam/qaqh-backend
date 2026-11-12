//! todo::split — 模型工具面的注册入口。
//!
//! 模型侧只有两个工具:`todo_write`（纯文本全量覆写,`list` 是唯一参数）与
//! `todo_update`（按 title 单条翻转）。**参数形态锁在 `typed.rs` 的 args 上**
//! （`deny_unknown_fields`）,执行体也在 typed;这里不再垫任何拆装层——曾经
//! 垫着的 `handle_write`/`handle_update` + `reject_fields` 白名单与 typed 的
//! 形态锁逐条重复,那层壳没有剩下任何职责,已随其退役。
//!
//! Display 的 fallback 注册保留:历史 transcript 回放仍要渲染已退役的
//! `todo_list` 调用。`actions::todo_set_for`/`todo_list_for` 保留全量能力,
//! HTTP service 面与 CLI 直访不受模型工具形态约束。

// ═══════════════════════════════════════════════════════
// Registration
// ═══════════════════════════════════════════════════════

pub fn register(mgr: &mut crate::ToolManager) {
    // 本版本起模型工具面收敛为 todo_write / todo_update 双工具：完整列表
    // 由 write/update 自身的 output.items 携带（全量快照），只读 list 从
    // 模型面退役（`todo_list_for` 保留给 HTTP service / CLI 直访）。
    // Display 的 fallback 注册保留——历史 transcript 回放仍需渲染旧调用。
    mgr.register_display("todo_write", crate::display::project_todo_write);
    mgr.register_display("todo_update", crate::display::project_todo_update);
    mgr.register_display("todo_list", crate::display::project_todo_list);
    mgr.register_typed(super::typed::TodoWriteTool);
    mgr.register_typed(super::typed::TodoUpdateTool);
}
