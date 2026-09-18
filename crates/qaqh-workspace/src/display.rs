//! 剩余内置工具的展示投影（09-18 跨仓展示契约 §6.1）。
//!
//! 每个投影只解析该工具自己的 canonical 输出；解析失败时不猜结构，回退为
//! `ToolHeader::Other` + `ToolBody::Text`，保持 H1 的「summary 禁止 JSON」。
//! 框架会在运行后覆写 `metrics`（H4），因此这里只构造展示形态。

use crate::tool_api::{PathOp, ToolBody, ToolDisplay, ToolHeader};

/// 解析工具输出的 JSON 信封；工具输出不是 JSON 时返回 `None`。
fn json_view(output: &str) -> Option<serde_json::Value> {
    serde_json::from_str::<serde_json::Value>(output).ok()
}

/// 从 JSON 输出中取第一个非空字符串字段。
fn json_string(value: Option<&serde_json::Value>, keys: &[&str]) -> Option<String> {
    let object = value?.as_object()?;
    keys.iter().find_map(|key| {
        object
            .get(*key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    })
}

/// 取文本输出的首行作为兜底摘要，并避免把 JSON 信封当作人类摘要。
fn first_human_line(output: &str) -> Option<String> {
    output
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .filter(|line| !line.starts_with('{'))
        .map(str::to_string)
}

/// 从本工具 JSON 信封提取人类可读正文；非 JSON 输出原样透传。
fn human_body(output: &str) -> String {
    let view = json_view(output);
    json_string(view.as_ref(), &["content", "message"]).unwrap_or_else(|| output.to_string())
}

/// 从本工具 JSON 信封提取人类可读 content/message。
fn json_summary(output: &str) -> Option<String> {
    let view = json_view(output);
    json_string(view.as_ref(), &["content", "message"]).or_else(|| first_human_line(output))
}

fn text_body(output: &str) -> ToolBody {
    ToolBody::Text {
        text: output.to_string(),
        truncated: false,
    }
}

fn fallback_display(label: &str, output: &str) -> ToolDisplay {
    ToolDisplay::new(
        ToolHeader::Other {
            label: label.to_string(),
        },
        text_body(output),
    )
}

/// 用工具名 + 兜底摘要构造单行 summary；没有可用文本时保持 `None`。
fn with_line_summary(mut display: ToolDisplay, summary: Option<String>) -> ToolDisplay {
    if let Some(summary) = summary.filter(|summary| !summary.is_empty()) {
        display.summary = Some(first_line(&summary));
    }
    display
}

fn first_line(value: &str) -> String {
    let line = value.lines().find(|line| !line.trim().is_empty());
    line.map(str::trim)
        .unwrap_or_default()
        .chars()
        .take(160)
        .collect()
}

fn read_paths(args: &serde_json::Value) -> Option<String> {
    if let Some(requests) = args.get("requests").and_then(|value| value.as_array()) {
        let paths: Vec<String> = requests
            .iter()
            .filter_map(|request| request.get("path").and_then(|value| value.as_str()))
            .filter(|path| !path.trim().is_empty())
            .map(str::to_string)
            .collect();
        return match paths.as_slice() {
            [] => None,
            [path] => Some(path.clone()),
            _ => Some(paths.join(", ")),
        };
    }
    args.get("path")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(str::to_string)
}

pub(crate) fn project_read(args: &serde_json::Value, output: &str) -> ToolDisplay {
    match read_paths(args) {
        Some(path) => {
            let display = ToolDisplay::new(
                ToolHeader::Path {
                    path,
                    op: PathOp::Read,
                },
                text_body(output),
            );
            with_line_summary(display, first_human_line(output))
        }
        None => fallback_display("read", output),
    }
}

pub(crate) fn project_write(args: &serde_json::Value, output: &str) -> ToolDisplay {
    path_result(
        args.get("path").and_then(|value| value.as_str()),
        PathOp::Write,
        "write",
        output,
    )
}

pub(crate) fn project_delete(args: &serde_json::Value, output: &str) -> ToolDisplay {
    path_result(
        args.get("path").and_then(|value| value.as_str()),
        PathOp::Delete,
        "delete",
        output,
    )
}

pub(crate) fn project_edit(args: &serde_json::Value, output: &str) -> ToolDisplay {
    path_result(
        args.get("path").and_then(|value| value.as_str()),
        PathOp::Edit,
        "edit",
        output,
    )
}

pub(crate) fn project_copy_range(args: &serde_json::Value, output: &str) -> ToolDisplay {
    path_result(
        args.get("target_path").and_then(|value| value.as_str()),
        PathOp::Write,
        "copy_range",
        output,
    )
}

fn path_result(path: Option<&str>, op: PathOp, tool: &'static str, output: &str) -> ToolDisplay {
    match path.map(str::trim).filter(|path| !path.is_empty()) {
        Some(path) => {
            let display = ToolDisplay::new(
                ToolHeader::Path {
                    path: path.to_string(),
                    op,
                },
                text_body(output),
            );
            with_line_summary(display, first_human_line(output))
        }
        None => fallback_display(tool, output),
    }
}

pub(crate) fn project_apply_patch(args: &serde_json::Value, output: &str) -> ToolDisplay {
    let path = args
        .get("patch")
        .and_then(|value| value.as_str())
        .and_then(|patch| {
            patch.lines().find_map(|line| {
                let (marker, path) = line
                    .split_once(" File: ")
                    .map(|(marker, path)| (marker.trim_start_matches('*'), path))
                    .unwrap_or_default();
                let marker = marker.trim();
                (matches!(marker, "Update" | "Add" | "Delete") && !path.trim().is_empty())
                    .then(|| path.trim().to_string())
            })
        });
    let display = match path {
        Some(path) => ToolDisplay::new(
            ToolHeader::Path {
                path,
                op: PathOp::Patch,
            },
            text_body(&human_body(output)),
        ),
        None => fallback_display("apply_patch", output),
    };
    with_line_summary(display, first_human_line(output))
}

pub(crate) fn project_glob(args: &serde_json::Value, output: &str) -> ToolDisplay {
    let pattern = args
        .get("pattern")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let root = args
        .get("path")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let path = match (root, pattern) {
        (Some(root), Some(pattern)) => format!("{root}:{pattern}"),
        (Some(root), None) => root.to_string(),
        (None, Some(pattern)) => pattern.to_string(),
        (None, None) => {
            return fallback_display("glob", output);
        }
    };
    let display = ToolDisplay::new(
        ToolHeader::Path {
            path,
            op: PathOp::List,
        },
        text_body(output),
    );
    with_line_summary(display, first_human_line(output))
}

fn query_display(
    tool: &'static str,
    query: Option<String>,
    scope: Option<String>,
    output: &str,
) -> ToolDisplay {
    let display = match query.filter(|query| !query.trim().is_empty()) {
        Some(query) => ToolDisplay::new(
            ToolHeader::Query {
                query,
                scope: scope.filter(|scope| !scope.trim().is_empty()),
            },
            text_body(&human_body(output)),
        ),
        None => fallback_display(tool, output),
    };
    with_line_summary(
        display,
        json_summary(output).or_else(|| first_human_line(output)),
    )
}

pub(crate) fn project_grep(args: &serde_json::Value, output: &str) -> ToolDisplay {
    query_display(
        "grep",
        args.get("pattern")
            .and_then(|value| value.as_str())
            .map(str::to_string),
        args.get("path")
            .and_then(|value| value.as_str())
            .map(str::to_string),
        output,
    )
}

pub(crate) fn project_web_fetch(args: &serde_json::Value, output: &str) -> ToolDisplay {
    query_display(
        "web_fetch",
        args.get("url")
            .and_then(|value| value.as_str())
            .map(str::to_string),
        args.get("output")
            .and_then(|value| value.as_str())
            .map(str::to_string),
        output,
    )
}

/// todo 系列的面板数据由 dashboard 维护；display 只保留 canonical 摘要行。
fn todo_display(label: &'static str, output: &str) -> ToolDisplay {
    ToolDisplay::new(
        ToolHeader::Other {
            label: label.to_string(),
        },
        ToolBody::None,
    )
    .with_summary(json_summary(output).unwrap_or_else(|| label.to_string()))
}

pub(crate) fn project_todo_write(_args: &serde_json::Value, output: &str) -> ToolDisplay {
    todo_display("todo", output)
}

pub(crate) fn project_todo_update(_args: &serde_json::Value, output: &str) -> ToolDisplay {
    todo_display("todo", output)
}

pub(crate) fn project_todo_list(_args: &serde_json::Value, output: &str) -> ToolDisplay {
    todo_display("todo", output)
}

pub(crate) fn project_process(args: &serde_json::Value, output: &str) -> ToolDisplay {
    let action = args
        .get("action")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("process");
    let view = json_view(output);
    let summary = json_string(view.as_ref(), &["content", "message", "status"]);
    let display = ToolDisplay::new(
        ToolHeader::Other {
            label: format!("process {action}"),
        },
        ToolBody::Text {
            text: json_string(view.as_ref(), &["content"]).unwrap_or_else(|| output.to_string()),
            truncated: false,
        },
    );
    with_line_summary(display, summary)
}

pub(crate) fn project_journal(args: &serde_json::Value, output: &str) -> ToolDisplay {
    let action = args
        .get("action")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("query");
    let display = ToolDisplay::new(
        ToolHeader::Other {
            label: format!("journal {action}"),
        },
        text_body(output),
    );
    with_line_summary(display, first_human_line(output))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_api::ToolMetrics;
    use serde_json::json;

    #[test]
    fn file_projectors_declare_path_and_non_json_summary() {
        let read = project_read(&json!({"path": "a.rs"}), "L1: fn a()\n");
        assert_eq!(
            read.header,
            ToolHeader::Path {
                path: "a.rs".into(),
                op: PathOp::Read
            }
        );
        assert_eq!(read.summary.as_deref(), Some("L1: fn a()"));

        let edit = project_edit(&json!({"path": "a.rs"}), "[OK] edit a.rs\n");
        assert_eq!(
            edit.header,
            ToolHeader::Path {
                path: "a.rs".into(),
                op: PathOp::Edit
            }
        );
        assert_eq!(edit.summary.as_deref(), Some("[OK] edit a.rs"));
    }

    #[test]
    fn apply_patch_header_uses_first_target_from_own_patch() {
        let args = json!({"patch": "*** Begin Patch\n*** Update File: src/app.rs\n@@\n-old\n+new\n*** End Patch\n"});
        let display = project_apply_patch(&args, "[OK] apply_patch — applied: 1 file(s)");
        assert_eq!(
            display.header,
            ToolHeader::Path {
                path: "src/app.rs".into(),
                op: PathOp::Patch
            }
        );
    }

    #[test]
    fn query_projectors_declare_pattern_and_scope() {
        let grep = project_grep(
            &json!({"pattern": "TODO", "path": "src"}),
            "src/a.rs:1:TODO\n",
        );
        assert_eq!(
            grep.header,
            ToolHeader::Query {
                query: "TODO".into(),
                scope: Some("src".into())
            }
        );
        let web = project_web_fetch(
            &json!({"url": "https://example.test", "output": "page.md"}),
            "page",
        );
        assert_eq!(
            web.header,
            ToolHeader::Query {
                query: "https://example.test".into(),
                scope: Some("page.md".into())
            }
        );
    }

    #[test]
    fn todo_uses_human_message_and_no_body() {
        let output = crate::json_ok(json!({"message": "Updated 3 todo(s)", "count": 3}));
        let display = project_todo_update(&json!({}), &output);
        assert_eq!(display.body, ToolBody::None);
        assert_eq!(display.summary.as_deref(), Some("Updated 3 todo(s)"));
        assert!(!display.summary.as_deref().unwrap().starts_with('{'));
    }

    #[test]
    fn process_and_subagent_fallbacks_never_emit_json_summary() {
        let output =
            crate::json_ok(json!({"content": "process 12: completed", "status": "completed"}));
        let process = project_process(&json!({"action": "check", "id": 12}), &output);
        assert_eq!(process.summary.as_deref(), Some("process 12: completed"));

        let fallback = fallback_display("exec-ish", "{\"message\":\"json\"}");
        assert_eq!(fallback.summary, None);
        assert_eq!(fallback.metrics, ToolMetrics::default());
    }
}
