//! todo::parse — 纯文本清单解析（apply_patch 范式：title 即身份，无 ID 分配）。
//!
//! todo_write 接收一段**完整清单文本**，逐行解析为 TodoItem。行格式：
//!
//! ```text
//! - [ ] 标题 | 描述
//! - [~] 标题 | 描述
//! - [x] 标题 | 描述 | 证据
//! [-] 已取消的标题
//! ```
//!
//! 标记：`[ ]` pending / `[~]` in_progress / `[x]` completed / `[-]` cancelled。
//! `| 描述` 与 `| 证据` 可省略（证据要占位：`[x] 标题 | | 已验证`）。
//! **title 即身份**：重复标题拒绝；模型改名 = 删除旧条目 + 新建。
//! `TodoItem.id` 恒等于 title（GoalEngine / 前端 dashboard 的 id 匹配零改动）。

use crate::json_err_string;

use super::model::{TodoItem, TodoStatus, TodoStore};

/// 单次覆写的条目上限（与旧 items 形态一致）。
pub(crate) const MAX_LIST_ITEMS: usize = 20;

fn status_of_marker(marker: &str) -> Option<TodoStatus> {
    match marker {
        " " => Some(TodoStatus::Pending),
        "~" => Some(TodoStatus::InProgress),
        "x" | "X" => Some(TodoStatus::Completed),
        "-" => Some(TodoStatus::Cancelled),
        _ => None,
    }
}

fn status_name(status: &TodoStatus) -> &'static str {
    match status {
        TodoStatus::Pending => "[ ]",
        TodoStatus::InProgress => "[~]",
        TodoStatus::Completed => "[x]",
        TodoStatus::Cancelled => "[-]",
    }
}

/// 把一行清单文本解析为 TodoItem。`id` 恒等于 title（身份即标题）。
fn parse_line(line: &str, line_no: usize) -> Result<TodoItem, String> {
    let invalid = |reason: String| {
        Err(json_err_string(
            "invalid_input",
            format!("todo list line {line_no}: {reason}"),
            "Expected `- [ ]|~|x|- title [| description [| evidence]]`.",
        ))
    };
    // 弹头 `- ` 仅在**后随空白**时剥离——`[-]` 取消标记自身以 `-` 开头，
    // 不能当作弹头吃掉。
    let body = line.trim_start();
    let body = match body.strip_prefix("- ") {
        Some(rest) => rest.trim_start(),
        None => body,
    };
    // `[ ]` 的空格标记会被 trim 吃掉——按 `[`/`]` 原样截取，不做 trim。
    let inner = match body.strip_prefix('[') {
        Some(inner) => inner,
        None => return invalid("missing status marker `[ ]`".into()),
    };
    let (marker, rest) = match inner.split_once(']') {
        Some((marker, rest)) => (marker, rest),
        None => return invalid("missing status marker `]`".into()),
    };
    let Some(status) = status_of_marker(marker) else {
        return invalid(format!(
            "unknown status marker `[{marker}]` (use [ ] [~] [x] [-])"
        ));
    };
    let rest = rest.trim();
    if rest.is_empty() {
        return invalid("title is required".into());
    }
    let mut fields = rest.split('|');
    let title = fields.next().unwrap_or_default().trim().to_string();
    if title.is_empty() {
        return invalid("title is required".into());
    }
    if title.chars().count() > 100 {
        return invalid("title max 100 chars".into());
    }
    let description = fields
        .next()
        .map(str::trim)
        .unwrap_or_default()
        .to_string();
    if description.chars().count() > 200 {
        return invalid("description max 200 chars".into());
    }
    let evidence = fields
        .next()
        .map(str::trim)
        .filter(|evidence| !evidence.is_empty())
        .map(str::to_string);
    if fields.next().is_some() {
        return invalid("too many `|` fields (title | description | evidence)".into());
    }
    Ok(TodoItem {
        id: title.clone(),
        title,
        description,
        status,
        evidence,
    })
}

/// 解析完整清单文本。空文本（或全空白）= 空清单（合法：清空重建）。
/// 重复标题拒绝（title 即身份，重名会让 update/snapshot 的匹配歧义）。
pub(crate) fn parse_todo_text(text: &str) -> Result<Vec<TodoItem>, String> {
    let mut items = Vec::new();
    for (offset, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let item = parse_line(line, offset + 1)?;
        if items.iter().any(|existing: &TodoItem| existing.title == item.title) {
            return Err(json_err_string(
                "invalid_input",
                format!(
                    "todo list line {}: duplicate title `{}`",
                    offset + 1,
                    item.title
                ),
                "Titles are identities — each title may appear at most once.",
            ));
        }
        items.push(item);
    }
    if items.len() > MAX_LIST_ITEMS {
        return Err(json_err_string(
            "invalid_input",
            format!("todo list max {MAX_LIST_ITEMS} entries per call"),
            "Reduce the plan to its essential steps.",
        ));
    }
    Ok(items)
}

/// 把 store 渲染回清单文本（receipt / 快照复用同一格式——写读对称）。
pub(crate) fn render_todo_text(store: &TodoStore) -> String {
    store
        .items
        .iter()
        .map(|item| {
            let mut line = format!(
                "- {} {}",
                status_name(&item.status),
                item.title
            );
            if !item.description.is_empty() || item.evidence.is_some() {
                line.push_str(&format!(" | {}", item.description));
            }
            if let Some(evidence) = &item.evidence {
                line.push_str(&format!(" | {evidence}"));
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_four_markers_with_optional_fields() {
        let items = parse_todo_text(
            "- [ ] a | desc a\n- [~] b\n- [x] c | | verified\n[-] d | gone",
        )
        .unwrap();
        assert_eq!(items.len(), 4);
        assert_eq!(items[0].status, TodoStatus::Pending);
        assert_eq!(items[0].description, "desc a");
        assert_eq!(items[1].status, TodoStatus::InProgress);
        assert_eq!(items[2].status, TodoStatus::Completed);
        assert_eq!(items[2].evidence.as_deref(), Some("verified"));
        assert_eq!(items[2].description, "");
        assert_eq!(items[3].status, TodoStatus::Cancelled);
        // id 恒等于 title（GoalEngine / dashboard 的身份契约）。
        assert_eq!(items[0].id, "a");
    }

    #[test]
    fn duplicate_titles_are_rejected() {
        let error = parse_todo_text("- [ ] same\n- [x] same").unwrap_err();
        assert!(error.contains("duplicate title"), "{error}");
    }

    #[test]
    fn empty_text_is_an_empty_list() {
        assert!(parse_todo_text("").unwrap().is_empty());
        assert!(parse_todo_text("  \n \n").unwrap().is_empty());
    }

    #[test]
    fn render_roundtrips_through_parse() {
        let mut store = TodoStore::default();
        store.items = parse_todo_text("- [~] working | note\n- [x] done | | ok").unwrap();
        let rendered = render_todo_text(&store);
        let reparsed = parse_todo_text(&rendered).unwrap();
        assert_eq!(reparsed.len(), 2);
        assert_eq!(reparsed[0].title, "working");
        assert_eq!(reparsed[0].status, TodoStatus::InProgress);
        assert_eq!(reparsed[0].description, "note");
        assert_eq!(reparsed[1].evidence.as_deref(), Some("ok"));
    }
}
