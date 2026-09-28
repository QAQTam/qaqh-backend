//! todo::parse — ID 分配与参数解析（alloc_id/expand_ids/parse_write_items）。

use serde_json::Value;

use crate::json_err_string;

use super::model::{TodoStatus, TodoStore};

// ═══════════════════════════════════════════════════════
// ID generation
// ═══════════════════════════════════════════════════════

/// 高水位分配器：持久化的 `next_id` 是"下一个待分配号"。分配前取
/// max(持久值, 现存最大号+1)：旧文件（next_id=0）自动迁移到手改文件/
/// 高水位落后的场景也不会发出重复 ID。
pub(crate) fn alloc_id(store: &mut TodoStore) -> String {
    let max_existing = store
        .items
        .iter()
        .filter_map(|item| item.id.strip_prefix('T')?.parse::<u32>().ok())
        .max()
        .unwrap_or(0);
    if store.next_id < max_existing + 1 {
        store.next_id = max_existing + 1;
    }
    let id = format!("T{}", store.next_id);
    store.next_id += 1;
    id
}

pub(crate) fn parse_todo_id(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(id) => {
            let id = id.trim();
            match id.strip_prefix('T') {
                // "T<n>" 形式原样接受（'T' 为 ASCII，strip_prefix 等价 [1..]）。
                Some(rest) if rest.parse::<u32>().is_ok() => Some(id.to_string()),
                _ => id.parse::<u32>().ok().map(|number| format!("T{number}")),
            }
        }
        Value::Number(number) => number.as_u64().map(|number| format!("T{number}")),
        _ => None,
    }
}

/// 展开 ID 表达式为具体 ID 列表：支持逗号分隔 + 数字范围（`T1-T3` 含端点）。
/// 每段可接受 `T1` / `1` / `T1-T3` / `T1-3`；空段忽略；无有效 ID 报错。
pub(crate) fn expand_todo_ids(expr: &str) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for segment in expr.split(',') {
        let segment = segment.trim();
        if segment.is_empty() {
            continue;
        }
        if let Some((start, end)) = segment.split_once('-') {
            let start =
                parse_todo_id(Some(&Value::String(start.trim().to_string()))).ok_or_else(|| {
                    json_err_string(
                        "INVALID_INPUT",
                        format!("invalid id range '{segment}'"),
                        "Use T<n> or a numeric range like T1-T3.",
                    )
                })?;
            let end =
                parse_todo_id(Some(&Value::String(end.trim().to_string()))).ok_or_else(|| {
                    json_err_string(
                        "INVALID_INPUT",
                        format!("invalid id range '{segment}'"),
                        "Use T<n> or a numeric range like T1-T3.",
                    )
                })?;
            let (start_n, end_n) = (todo_id_number(&start), todo_id_number(&end));
            let (Some(start_n), Some(end_n)) = (start_n, end_n) else {
                return Err(json_err_string(
                    "INVALID_INPUT",
                    format!("invalid id range '{segment}'"),
                    "Range endpoints must be T<number>.",
                ));
            };
            if start_n > end_n {
                return Err(json_err_string(
                    "INVALID_INPUT",
                    format!("id range '{segment}' has start > end"),
                    "Use ascending ranges like T1-T3.",
                ));
            }
            // W1：展开封顶——创建侧有 MAX_CREATE_ITEMS=20，展开侧原本无界，
            // T1-T4000000000 会 OOM/卡死 agent loop。
            const MAX_RANGE_EXPAND: u32 = 1000;
            if end_n - start_n >= MAX_RANGE_EXPAND {
                return Err(json_err_string(
                    "INVALID_INPUT",
                    format!("id range '{segment}' expands to more than {MAX_RANGE_EXPAND} ids"),
                    "Narrow the range, e.g. T1-T20.",
                ));
            }
            for n in start_n..=end_n {
                out.push(format!("T{n}"));
            }
        } else {
            let id = parse_todo_id(Some(&Value::String(segment.to_string()))).ok_or_else(|| {
                json_err_string(
                    "INVALID_INPUT",
                    format!("invalid id '{segment}'"),
                    "Use T<n> or a numeric range like T1-T3.",
                )
            })?;
            out.push(id);
        }
    }
    if out.is_empty() {
        return Err(json_err_string(
            "INVALID_INPUT",
            "no valid ids in expression",
            "Provide at least one id, e.g. T1 or T1-T3.",
        ));
    }
    Ok(out)
}

/// 提取 `T<n>` 中的数字部分。
pub(crate) fn todo_id_number(id: &str) -> Option<u32> {
    id.strip_prefix('T').and_then(|digits| digits.parse().ok())
}

/// A single model-authored task description before its permanent ID is assigned.
/// 仅剩 HTTP/CLI service 面（`exec_todo_create`）与测试使用；模型工具
/// `todo_write` 走 [`ParsedWriteItem`]（全量覆写，status 必填）。
#[derive(Debug, Clone)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct NewTodo {
    pub(crate) title: String,
    pub(crate) description: String,
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn parse_new_todo(value: &Value, label: &str) -> Result<NewTodo, String> {
    let title = value
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if title.is_empty() || title.chars().count() > 100 {
        return Err(json_err_string(
            "INVALID_INPUT",
            format!("{label}.title must be 1-100 chars"),
            "Use a short imperative title, e.g. 'Add login API'.",
        ));
    }
    let description = value
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if description.chars().count() > 200 {
        return Err(json_err_string(
            "INVALID_INPUT",
            format!("{label}.description max 200 chars"),
            "",
        ));
    }
    Ok(NewTodo { title, description })
}

/// One fully-specified item of a full-replace `todo_write` call (v4).
/// `id: None` = assign a fresh `T<n>`; `Some(id)` = re-reference an existing ID.
#[derive(Debug, Clone)]
pub(crate) struct ParsedWriteItem {
    pub(crate) id: Option<String>,
    pub(crate) title: String,
    pub(crate) description: String,
    pub(crate) status: TodoStatus,
    pub(crate) evidence: Option<String>,
}

/// One mutation can carry at most this many items. Keeping a rewrite in one
/// transaction prevents parallel tool calls from racing the T{n} allocator.
pub(crate) const MAX_WRITE_ITEMS: usize = 20;

fn parse_status_field(value: Option<&Value>, label: &str) -> Result<TodoStatus, String> {
    let raw = value
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default();
    super::actions::parse_status(raw).ok_or_else(|| {
        json_err_string(
            "INVALID_INPUT",
            format!("{label}.status must be one of pending|in_progress|completed|cancelled"),
            "Status is required on every item (full-replace semantics).",
        )
    })
}

pub(crate) fn parse_write_items(args: &Value) -> Result<Vec<ParsedWriteItem>, String> {
    let items = args.get("items").and_then(Value::as_array).ok_or_else(|| {
        json_err_string(
            "INVALID_INPUT",
            "todo_write requires items",
            "Provide the full list: [{title, status, id?, description?, evidence?}].",
        )
    })?;
    if items.len() > MAX_WRITE_ITEMS {
        return Err(json_err_string(
            "INVALID_INPUT",
            format!("items max {MAX_WRITE_ITEMS} entries per call"),
            "Reduce the plan to its essential steps.",
        ));
    }
    items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let label = format!("items[{index}]");
            let title = item
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string();
            if title.is_empty() || title.chars().count() > 100 {
                return Err(json_err_string(
                    "INVALID_INPUT",
                    format!("{label}.title must be 1-100 chars"),
                    "Use a short imperative title, e.g. 'Add login API'.",
                ));
            }
            let description = item
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string();
            if description.chars().count() > 200 {
                return Err(json_err_string(
                    "INVALID_INPUT",
                    format!("{label}.description max 200 chars"),
                    "",
                ));
            }
            let status = parse_status_field(item.get("status"), &label)?;
            let evidence = item
                .get("evidence")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string);
            let id = parse_todo_id(item.get("id"));
            Ok(ParsedWriteItem {
                id,
                title,
                description,
                status,
                evidence,
            })
        })
        .collect()
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn insertion_index(store: &TodoStore, args: &Value) -> Result<usize, String> {
    let before_raw = args.get("before_id");
    let after_raw = args.get("after_id");
    let before = parse_todo_id(before_raw);
    let after = parse_todo_id(after_raw);
    if before_raw.is_some() && before.is_none() {
        return Err(json_err_string(
            "INVALID_INPUT",
            "invalid before_id",
            "Use an assigned ID such as T1.",
        ));
    }
    if after_raw.is_some() && after.is_none() {
        return Err(json_err_string(
            "INVALID_INPUT",
            "invalid after_id",
            "Use an assigned ID such as T1.",
        ));
    }
    if before.is_some() && after.is_some() {
        return Err(json_err_string(
            "INVALID_INPUT",
            "use only one of before_id or after_id",
            "",
        ));
    }
    if before.is_none() && after.is_none() {
        return Err(json_err_string(
            "INVALID_INPUT",
            "insert requires before_id or after_id",
            "Use action=create to append tasks at the end.",
        ));
    }
    if let Some(id) = before {
        return store
            .items
            .iter()
            .position(|item| item.id == id)
            .ok_or_else(|| {
                json_err_string(
                    "NOT_FOUND",
                    format!("todo {id} not found"),
                    "Use todo(action=\"list\") to inspect IDs.",
                )
            });
    }
    if let Some(id) = after {
        return store
            .items
            .iter()
            .position(|item| item.id == id)
            .map(|index| index + 1)
            .ok_or_else(|| {
                json_err_string(
                    "NOT_FOUND",
                    format!("todo {id} not found"),
                    "Use todo(action=\"list\") to inspect IDs.",
                )
            });
    }
    unreachable!("insert anchor presence was validated above")
}
