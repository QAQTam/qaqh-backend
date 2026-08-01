//! 跨会话记忆工具：`memory`。
//!
//! 支持三种操作：
//! - `write` — 手动写入一条记忆
//! - `query` — 语义搜索已存储的记忆
//! - `delete` — 删除指定记忆
//!
//! 与自动归档（memory_hook）互补：工具提供手动精确控制，归档提供自动提取。

use std::sync::{Arc, Mutex, OnceLock};

use deepx_vector::VectorEngine;

use crate::{ToolCallCtx, ToolHandler, ToolResult, ToolRisk};

// ─── 共享引擎 ────────────────────────────────────────────────────────────────

static ENGINE: OnceLock<Arc<Mutex<VectorEngine>>> = OnceLock::new();

/// 由 AgentState 调用，注入共享引擎。
pub fn set_engine(engine: Arc<Mutex<VectorEngine>>) {
    let _ = ENGINE.set(engine);
}

macro_rules! try_or_err {
    ($expr:expr, $msg:expr) => {
        match $expr {
            Some(v) => v,
            None => return ToolResult::error($msg),
        }
    };
}

// ─── Handler ─────────────────────────────────────────────────────────────────

fn handle_memory(ctx: ToolCallCtx) -> ToolResult {
    let action = ctx.get_str("action").unwrap_or("query");
    let eng = try_or_err!(ENGINE.get(), "记忆引擎未初始化");
    let guard = try_or_err!(eng.lock().ok(), "引擎锁已毒化");

    match action {
        "write" => {
            let content = try_or_err!(ctx.get_str("content"), "缺少参数 content");
            let memory_type = ctx.get_str("type").unwrap_or("finding");
            match guard.write_memory(content, memory_type) {
                Ok(entry) => ToolResult::ok(format!(
                    "记忆 {} 已写入 [{}]",
                    entry.id,
                    entry.memory_type
                )),
                Err(e) => ToolResult::error(format!("写入记忆失败: {e}")),
            }
        }

        "query" => {
            let query = try_or_err!(ctx.get_str("query"), "缺少参数 query");
            let limit = ctx.get_u64("limit").unwrap_or(5) as usize;

            let mut entries = guard.recall_memory_keyword(query, limit);

            if entries.len() < limit {
                if let Ok(sem) = guard.recall_memory(query, limit - entries.len()) {
                    entries.extend(sem);
                }
            }

            if entries.is_empty() {
                return ToolResult::ok("未找到相关记忆。");
            }

            let out: String = entries
                .iter()
                .enumerate()
                .map(|(i, e)| {
                    format!(
                        "[{}] {} [{}] {}",
                        i + 1,
                        e.id,
                        e.memory_type,
                        e.content
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            ToolResult::ok(out)
        }

        "delete" => {
            let id = try_or_err!(ctx.get_str("id"), "缺少参数 id");
            match guard.delete_memory(id) {
                Ok(true) => ToolResult::ok(format!("记忆 {} 已删除", id)),
                Ok(false) => ToolResult::error(format!("未找到记忆 {}", id)),
                Err(e) => ToolResult::error(format!("删除记忆失败: {e}")),
            }
        }

        _ => ToolResult::error(format!(
            "未知操作: {action}。使用 write、query 或 delete。"
        )),
    }
}

// ─── Registration ────────────────────────────────────────────────────────────

pub fn register(mgr: &mut crate::ToolManager) {
    use std::time::Duration;

    mgr.register(ToolHandler {
        key: "memory".to_string(),
        description: "管理跨会话记忆：write（手动记录）、query（语义/关键词搜索）、delete（删除）。记忆在会话之间持久化，查询时优先返回最近/最重要的条目。",
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["write", "query", "delete"],
                    "description": "操作类型"
                },
                "content": {
                    "type": "string",
                    "description": "write: 要记忆的内容"
                },
                "type": {
                    "type": "string",
                    "enum": ["decision", "fix", "finding", "convention"],
                    "description": "write: 记忆类别"
                },
                "query": {
                    "type": "string",
                    "description": "query: 搜索查询"
                },
                "limit": {
                    "type": "integer",
                    "description": "query: 最大返回条数，默认 5"
                },
                "id": {
                    "type": "string",
                    "description": "delete: 要删除的记忆 ID（如 mem_xxx）"
                }
            },
            "required": ["action"],
            "additionalProperties": false
        }),
        handler: handle_memory,
        risk: ToolRisk::Write,
        default_timeout: Duration::from_secs(15),
    });
}
