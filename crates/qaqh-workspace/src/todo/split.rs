//! todo::split — todo 工具三件套（v4 全量覆写形态，2026-09-12）。
//!
//! `todo_write`（全量覆写：items 即完整清单，每条带必填 status）+
//! `todo_update`（单条状态/证据轻量通道）+ `todo_list`（只读查看）。
//! ID 体系保留（分配式、单调不复用；条目可显式引用既有 id）。对齐
//! Codex update_plan 的覆写语义 + prompt 合同（exactly-one-in-progress），
//! 保留 QAQH 自有的 ID 与 evidence 语义。
//!
//! 底层契约（`exec_todo_create(positioned)` / `todo_set_for` /
//! `todo_list_for`）保留全量能力：HTTP service 面与 CLI 直访不受工具形态
//! 约束。

use serde_json::Value;

use crate::ToolResult;

use super::actions::{exec_todo_set, exec_todo_write};

// ═══════════════════════════════════════════════════════
// Handlers（字段校验表 = 各工具形态的白名单补集）
// ═══════════════════════════════════════════════════════

pub(crate) fn tool_result(result: Result<String, String>) -> ToolResult {
    match result {
        Ok(content) => ToolResult::ok(content),
        Err(content) => {
            // 旧 Err(String) 通道：内容通常是 json_err_string 信封
            // ({code,message,hint})。解包还原真实错误码（not_found /
            // invalid_input）——否则模型只看到泛型 tool_error，无法自愈。
            match serde_json::from_str::<serde_json::Value>(&content) {
                Ok(value) if value.get("code").is_some() && value.get("message").is_some() => {
                    let code = value["code"].as_str().unwrap_or("tool_error").to_string();
                    let message = value["message"].as_str().unwrap_or(&content).to_string();
                    let hint = value["hint"].as_str().map(str::to_string);
                    ToolResult::error_with(code, message, false, hint)
                }
                _ => ToolResult::error(content),
            }
        }
    }
}

/// 跨形态字段拒绝（INVALID_INPUT）。原 dispatch.rs 的守卫函数，随聚合
/// 退役迁入本文件。
pub(crate) fn reject_fields(args: &Value, fields: &[&str], tool: &str) -> Result<(), String> {
    let present: Vec<&str> = fields
        .iter()
        .copied()
        .filter(|field| args.get(*field).is_some())
        .collect();
    if present.is_empty() {
        Ok(())
    } else {
        Err(crate::json_err_string(
            "invalid_input",
            format!("{tool} does not accept: {}", present.join(", ")),
            "Follow the tool-specific schema.",
        ))
    }
}

pub fn handle_write(args: &Value) -> ToolResult {
    // 只认 `list`（纯文本全量覆写）：旧 items 形态、顶层便利字段、定位
    // 插入与 action 词表一律拒绝。
    let result = reject_fields(
        args,
        &[
            "items",
            "id",
            "status",
            "evidence",
            "title",
            "description",
            "ids",
            "updates",
            "after_id",
            "before_id",
            "action",
            "explanation",
        ],
        "todo_write",
    )
    .and_then(|_| exec_todo_write(args));
    tool_result(result)
}

pub fn handle_update(args: &Value) -> ToolResult {
    // 单一形态：{title, status, evidence?} 按 title 精确翻转一次一条；
    // 旧 id/ids/updates 形态拒绝（title 即身份）。
    let result = reject_fields(
        args,
        &[
            "id",
            "ids",
            "updates",
            "items",
            "after_id",
            "before_id",
            "action",
        ],
        "todo_update",
    )
    .and_then(|_| exec_todo_set(args));
    tool_result(result)
}

pub fn handle_list(args: &Value) -> ToolResult {
    let result = reject_fields(
        args,
        &[
            "title",
            "description",
            "items",
            "id",
            "evidence",
            "after_id",
            "before_id",
            "ids",
            "updates",
        ],
        "todo_list",
    )
    .and_then(|_| super::actions::exec_todo_list(args));
    tool_result(result)
}

// ═══════════════════════════════════════════════════════
// Schemas（单一职责：无 oneOf、无参数归属说明文字）
// ═══════════════════════════════════════════════════════

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
