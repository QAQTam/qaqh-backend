//! 剩余内置工具的展示投影（09-18 跨仓展示契约 §6.1）。
//!
//! 每个投影只解析该工具自己的 canonical 输出；解析失败时不猜结构，回退为
//! `ToolHeader::Other` + `ToolBody::Text`，保持 H1 的「summary 禁止 JSON」。
//! 框架会在运行后覆写 `metrics`（H4），因此这里只构造展示形态。

use crate::tool_api::{ToolBody, ToolDisplay, ToolHeader, ToolProjection};

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

#[cfg(test)]
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

pub(crate) fn project_todo_list(args: &serde_json::Value, output: &str) -> ToolDisplay {
    serde_json::from_str::<crate::todo::typed::TodoListOutput>(output)
        .map(|typed| typed.display(args))
        .unwrap_or_else(|_| todo_display("todo", output))
}

/// 从 typed runtime 写入 `ToolResult.data` 的 canonical payload 生成展示投影。
///
/// 仅识别已完成 typed 化的 todo 三件套；未命中时返回 `None`，调用方回退旧
/// `ToolDisplayFn`。这保证 display 不再从 model JSON 字符串考古。
pub(crate) fn project_typed_tool_display(
    name: &str,
    args: &serde_json::Value,
    data: &serde_json::Value,
) -> Option<ToolDisplay> {
    match name {
        "todo_write" => serde_json::from_value::<crate::todo::typed::TodoWriteOutput>(data.clone())
            .ok()
            .map(|output| output.display(args)),
        "todo_update" => {
            serde_json::from_value::<crate::todo::typed::TodoUpdateOutput>(data.clone())
                .ok()
                .map(|output| output.display(args))
        }
        "todo_list" => serde_json::from_value::<crate::todo::typed::TodoListOutput>(data.clone())
            .ok()
            .map(|output| output.display(args)),
        _ => None,
    }
}

/// MCP 动态工具没有工具作者专属投影时的 canonical fallback。
///
/// `label` 必须是完整注册名，多个 MCP 工具同屏时标题/身份才可区分。
/// 正文保持 `None`：模型/展示的 `output` 是同一份 canonical 内容，wire 不双写。
/// `summary` 携带紧凑参数摘要，让 client 可以删除通用 args 考古器。
pub(crate) fn project_mcp_fallback(
    name: &str,
    args: &serde_json::Value,
    _output: &str,
) -> ToolDisplay {
    let display = ToolDisplay::new(
        ToolHeader::Other {
            label: name.to_string(),
        },
        ToolBody::None,
    );
    let summary = compact_args_summary(args);
    if summary.is_empty() {
        display
    } else {
        display.with_summary(summary)
    }
}

/// 紧凑参数摘要：最多取 3 个 primitive 字段，避免把任意 MCP args 全量上屏。
///
/// MCP server 的参数面是任意的；这里不猜 schema，只把模型调用时用户关心的
/// 简单标量透出。空对象/复杂对象不产生伪造文本。
fn compact_args_summary(args: &serde_json::Value) -> String {
    const MAX_PARTS: usize = 3;
    const MAX_VALUE_CHARS: usize = 40;
    let Some(object) = args.as_object() else {
        return String::new();
    };
    let mut parts = Vec::new();
    for (key, value) in object {
        let rendered = match value {
            serde_json::Value::String(text) if !text.is_empty() => {
                let short: String = text.chars().take(MAX_VALUE_CHARS).collect();
                let ellipsis = if text.chars().count() > MAX_VALUE_CHARS {
                    "…"
                } else {
                    ""
                };
                format!("{key}={short}{ellipsis}")
            }
            serde_json::Value::Number(_) | serde_json::Value::Bool(_) => format!("{key}={value}"),
            _ => continue,
        };
        parts.push(rendered);
        if parts.len() >= MAX_PARTS {
            break;
        }
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("args [{}]", parts.join(", "))
    }
}

/// ask 的结果由 interaction 面板承载；display 只留一条调用语义。
pub(crate) fn project_ask(args: &serde_json::Value, _output: &str) -> ToolDisplay {
    let count = args
        .get("questions")
        .and_then(|value| value.as_array())
        .map(Vec::len)
        .unwrap_or(1);
    ToolDisplay::new(
        ToolHeader::Other {
            label: "ask".to_string(),
        },
        ToolBody::None,
    )
    .with_summary(format!(
        "asked {count} question{}",
        if count == 1 { "" } else { "s" }
    ))
}

/// skills 的 activate/list/resource/validate 输出各自有明确回执语义。
pub(crate) fn project_skills(args: &serde_json::Value, output: &str) -> ToolDisplay {
    let action = args
        .get("action")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("skills");
    let name = args
        .get("name")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let path = args
        .get("path")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let label = format!("skills {action}");

    let body = if action == "resource" {
        text_body(&human_body(output))
    } else {
        ToolBody::None
    };
    let display = ToolDisplay::new(ToolHeader::Other { label }, body);

    let view = json_view(output);
    let summary = match action {
        "activate" => json_string(view.as_ref(), &["content", "message"]),
        "resource" => Some(match (name, path) {
            (Some(name), Some(path)) => format!("resource · {name}/{path}"),
            (Some(name), None) => format!("resource · {name}"),
            _ => "resource".to_string(),
        }),
        "list" => {
            let skills = view
                .as_ref()
                .and_then(|view| view.get("skills"))
                .and_then(|value| value.as_array())
                .map(Vec::len)
                .unwrap_or(0);
            let diagnostics = view
                .as_ref()
                .and_then(|view| view.get("diagnostics"))
                .and_then(|value| value.as_array())
                .map(Vec::len)
                .unwrap_or(0);
            Some(format!(
                "listed {skills} skills · {diagnostics} diagnostics"
            ))
        }
        "validate" => {
            let valid = view
                .as_ref()
                .and_then(|view| view.get("valid"))
                .and_then(|value| value.as_bool());
            let errors = view
                .as_ref()
                .and_then(|view| view.get("errors"))
                .and_then(|value| value.as_array())
                .map(Vec::len);
            match (valid, errors) {
                (Some(true), _) => Some("validation passed".to_string()),
                (Some(false), Some(count)) => Some(format!("validation failed · {count} errors")),
                _ => json_summary(output),
            }
        }
        _ => json_summary(output),
    };
    with_line_summary(display, summary.or_else(|| json_summary(output)))
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
    use crate::tool_api::{PathOp, ToolMetrics};
    use serde_json::json;

    #[test]
    fn file_projectors_declare_path_and_non_json_summary() {
        let edit = crate::file_mutate::mutation_display(
            Some("a.rs"),
            "a.rs",
            PathOp::Edit,
            "edit",
            "[OK] edit a.rs\n",
            None,
        );
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
    fn apply_patch_header_uses_canonical_first_target() {
        let display = crate::file_mutate::mutation_display(
            Some("src/app.rs"),
            "",
            PathOp::Patch,
            "apply_patch",
            "[OK] apply_patch — applied: 1 file(s)",
            None,
        );
        assert_eq!(
            display.header,
            ToolHeader::Path {
                path: "src/app.rs".into(),
                op: PathOp::Patch
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
    fn todo_list_uses_typed_projection() {
        let output = crate::todo::typed::TodoListOutput {
            items: Vec::new(),
            current_id: None,
            counts: crate::todo::typed::TodoCounts {
                idle: 1,
                in_progress: 1,
                completed: 0,
                cancelled: 0,
                total: 2,
            },
        };
        let display = project_todo_list(&json!({}), &output.to_envelope_string().unwrap());
        assert_eq!(display.body, ToolBody::None);
        assert_eq!(
            display.summary.as_deref(),
            Some("2 task(s) · 1 in progress")
        );
        assert!(!display.summary.as_deref().unwrap().starts_with('{'));
    }

    #[test]
    fn mcp_fallback_uses_full_registration_name_and_compact_args_only() {
        let args = serde_json::json!({
            "alpha": "z".repeat(45),
            "beta": true,
            "delta": 1,
            "gamma": "dropped"
        });
        let display = project_mcp_fallback("mcp__demo__echo", &args, "canonical-output");
        assert_eq!(
            display.header,
            ToolHeader::Other {
                label: "mcp__demo__echo".into()
            }
        );
        assert_eq!(display.body, ToolBody::None);
        assert_eq!(
            display.summary.as_deref(),
            Some("args [alpha=zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz…, beta=true, delta=1]")
        );
        assert!(!display.summary.as_deref().unwrap().contains("gamma"));

        let empty = project_mcp_fallback("mcp__demo__noargs", &serde_json::json!({}), "");
        assert_eq!(empty.summary, None);
        assert_eq!(
            empty.header,
            ToolHeader::Other {
                label: "mcp__demo__noargs".into()
            }
        );
    }

    #[test]
    fn ask_and_skills_use_panel_semantics_without_json_dumping() {
        let ask = project_ask(
            &json!({"questions": [{"question": "A?"}, {"question": "B?"}]}),
            &crate::json_ok(json!({"mode":"batch"})),
        );
        assert_eq!(ask.summary.as_deref(), Some("asked 2 questions"));
        assert_eq!(ask.body, ToolBody::None);

        let activate = project_skills(
            &json!({"action":"activate","name":"foo"}),
            &crate::json_ok(json!({"skill":"foo","content":"[OK] skill 'foo' activated."})),
        );
        assert_eq!(
            activate.summary.as_deref(),
            Some("[OK] skill 'foo' activated.")
        );
        assert_eq!(activate.body, ToolBody::None);

        let list = project_skills(
            &json!({"action":"list"}),
            r#"{"skills":[{},{}],"diagnostics":[]}"#,
        );
        assert_eq!(
            list.summary.as_deref(),
            Some("listed 2 skills · 0 diagnostics")
        );

        let resource = project_skills(
            &json!({"action":"resource","name":"foo","path":"guide.md"}),
            "use foo like this",
        );
        assert_eq!(resource.summary.as_deref(), Some("resource · foo/guide.md"));
        assert_eq!(
            resource.body,
            ToolBody::Text {
                text: "use foo like this".into(),
                truncated: false
            }
        );
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
