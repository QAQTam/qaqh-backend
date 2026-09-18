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

use std::time::Duration;

use serde_json::Value;

use crate::permission::ToolCategory;
use crate::{ToolCallCtx, ToolHandler, ToolResult, ToolRisk};

use super::actions::{exec_todo_set, exec_todo_write};

// ═══════════════════════════════════════════════════════
// Handlers（字段校验表 = 各工具形态的白名单补集）
// ═══════════════════════════════════════════════════════

pub(crate) fn tool_result(result: Result<String, String>) -> ToolResult {
    match result {
        Ok(content) => ToolResult::ok(content),
        Err(content) => ToolResult::error(content),
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
            "INVALID_INPUT",
            format!("{tool} does not accept: {}", present.join(", ")),
            "Follow the tool-specific schema.",
        ))
    }
}

pub fn handle_write(ctx: ToolCallCtx) -> ToolResult {
    // items-only：顶层便利字段（单条形态）与定位插入残留一律拒绝。
    // 条目内的 id/status/evidence/description 是 v4 全量覆写的合法字段。
    let result = reject_fields(
        &ctx.args,
        &[
            "id",
            "status",
            "evidence",
            "after_id",
            "before_id",
            "ids",
            "updates",
            "title",
            "description",
        ],
        "todo_write",
    )
    .and_then(|_| exec_todo_write(&ctx.args));
    tool_result(result)
}

pub fn handle_update(ctx: ToolCallCtx) -> ToolResult {
    // 单一形态：{id, status, evidence?} 一次一条；批量/updates 已移除——
    // 多任务循环调用。底层 exec_todo_set 的 ids/updates 分支保留
    // （HTTP service 面 / CLI 直访不受工具形态约束）。
    let result = reject_fields(
        &ctx.args,
        &[
            "ids",
            "updates",
            "title",
            "description",
            "items",
            "after_id",
            "before_id",
        ],
        "todo_update",
    )
    .and_then(|_| exec_todo_set(&ctx.args));
    tool_result(result)
}

pub fn handle_list(ctx: ToolCallCtx) -> ToolResult {
    let result = reject_fields(
        &ctx.args,
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
    .and_then(|_| super::actions::exec_todo_list(&ctx.args));
    tool_result(result)
}

// ═══════════════════════════════════════════════════════
// Schemas（单一职责：无 oneOf、无参数归属说明文字）
// ═══════════════════════════════════════════════════════

fn todo_write_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "items": {
                "type": "array",
                "maxItems": 20,
                "description": "The FULL task list — replaces the previous list entirely. Each item needs title + status; include every prior item you want to keep.",
                "items": {
                    "type": "object",
                    "properties": {
                        "id": {"type": ["string", "integer"], "description": "Existing T<n> to keep/update this task; omit to assign a new one."},
                        "title": {"type": "string", "description": "Task title (1-100 chars)."},
                        "status": {"type": "string", "enum": ["idle", "in_progress", "completed", "cancelled"], "description": "Exactly one item should be in_progress while working."},
                        "description": {"type": "string", "description": "Optional context (<=200 chars)."},
                        "evidence": {"type": "string", "description": "Completion evidence (for completed items)."}
                    },
                    "required": ["title", "status"],
                    "additionalProperties": false
                }
            },
            "explanation": {"type": "string", "description": "Optional one-liner on why the plan changed."}
        },
        "required": ["items"],
        "additionalProperties": false
    })
}

fn todo_update_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "id": {"type": ["string", "integer"], "description": "Target ID (e.g. T1)."},
            "status": {"type": "string", "enum": ["idle", "in_progress", "completed", "cancelled"], "description": "Target status."},
            "evidence": {"type": "string", "description": "Completion summary (required when completed)."}
        },
        "required": ["id", "status"],
        "additionalProperties": false
    })
}

fn todo_list_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "status": {"type": "string", "enum": ["idle", "in_progress", "completed", "cancelled"], "description": "Optional filter."}
        },
        "additionalProperties": false
    })
}

// ═══════════════════════════════════════════════════════
// Registration
// ═══════════════════════════════════════════════════════

/// 拆分工具注册项（key/description/schema/handler/risk/category）。
type SplitTool = (
    &'static str,
    &'static str,
    Value,
    fn(ToolCallCtx) -> ToolResult,
    ToolRisk,
    ToolCategory,
);

pub fn register(mgr: &mut crate::ToolManager) {
    mgr.register_display("todo_write", crate::display::project_todo_write);
    mgr.register_display("todo_update", crate::display::project_todo_update);
    mgr.register_display("todo_list", crate::display::project_todo_list);
    let tools: [SplitTool; 3] = [
        (
            "todo_write",
            "Replace the whole task list (full-replace). Each item needs title + status; re-include every item you keep, with its id to preserve it. Exactly one in_progress while working. New items omit id.",
            todo_write_schema(),
            handle_write,
            ToolRisk::Write,
            ToolCategory::Write,
        ),
        (
            "todo_update",
            "Set one task's status: {id, status, evidence?}. One task per call — loop for batches.",
            todo_update_schema(),
            handle_update,
            ToolRisk::Write,
            ToolCategory::Write,
        ),
        (
            "todo_list",
            "List session tasks; optional status filter. Read-only (allowed in plan mode).",
            todo_list_schema(),
            handle_list,
            ToolRisk::ReadOnly,
            ToolCategory::Read,
        ),
    ];
    for (key, description, input_schema, handler, risk, category) in tools {
        mgr.register(ToolHandler {
            key: key.to_string(),
            description,
            input_schema,
            handler,
            risk,
            category,
            default_timeout: Duration::from_secs(15),
        });
    }
}
