//! exec 展示投影（09-18 跨仓展示契约 §3.2 / §7 工具映射表）。
//!
//! 投影由 typed `ExecOutput` 直接派生；legacy 字符串路径只在确认是 exec
//! 自己的 canonical JSON 时复用同一投影函数，不做 client 侧 JSON 考古。

use super::direct::ExecOutput;
use crate::tool_api::{
    ToolBody, ToolContentBlock, ToolDisplay, ToolError, ToolErrorCode, ToolErrorKind, ToolHeader,
    ToolProjection, ToolStatus,
};

#[cfg(test)]
pub(crate) fn project_display(args: &serde_json::Value, output: &str) -> ToolDisplay {
    let command = command_from_args(args);
    let Ok(view) = serde_json::from_str::<ExecOutput>(output) else {
        // 非 ExecOutput（如 manager 失败文本）：仍给出可渲染的 header/body，
        // 不猜测结构，原样透出文本。
        let header = command
            .clone()
            .map(|command| ToolHeader::Shell { command })
            .unwrap_or(ToolHeader::Other {
                label: "exec".to_string(),
            });
        return ToolDisplay::new(
            header,
            ToolBody::Text {
                text: output.to_string(),
                truncated: false,
            },
        );
    };
    display_from_output(&view, command)
}

impl ToolProjection for ExecOutput {
    fn status(&self) -> ToolStatus {
        let success = match self.exit_code {
            Some(0) => true,
            Some(_) => false,
            None => !self.timed_out && !self.cancelled,
        };
        if success {
            ToolStatus::Ok
        } else {
            ToolStatus::Error
        }
    }

    fn error(&self) -> Option<ToolError> {
        if self.status() == ToolStatus::Ok {
            return None;
        }
        let mut error = ToolError::new(ToolErrorKind::Execution, self.to_json());
        error.code = ToolErrorCode::from_legacy("TOOL_ERROR");
        Some(error)
    }

    fn model_blocks(&self) -> Vec<ToolContentBlock> {
        vec![ToolContentBlock::Text {
            text: self.to_json(),
        }]
    }

    fn summary(&self) -> Option<String> {
        Some(shell_summary(self, None))
    }

    fn display(&self, args: &serde_json::Value) -> ToolDisplay {
        display_from_output(self, command_from_args(args))
    }
}

fn display_from_output(view: &ExecOutput, command: Option<String>) -> ToolDisplay {
    let header = command
        .clone()
        .map(|command| ToolHeader::Shell { command })
        .unwrap_or(ToolHeader::Other {
            label: "exec".to_string(),
        });
    let summary = shell_summary(view, command.as_deref());
    ToolDisplay::new(
        header,
        ToolBody::Shell {
            // 终态正文在后端完成 `\r` 覆盖归一化（§5.2）；模型文本不变。
            output: normalize_carriage_returns(&view.output),
            exit_code: view.exit_code,
            truncated: view.truncated,
        },
    )
    .with_summary(summary)
}

/// 终态正文的 `\r` 覆盖归一化（仅展示平面；模型文本不变，契约 §5.2）。
///
/// 语义与 TUI `apply_bash_progress` 对齐：`\r\n` 视为换行；含 `\r` 的行取
/// **最后一个非空覆盖段**；纯 `\r` 行（无有效内容）丢弃。流式 progress 不做
/// 此归一化——已发送的字节无法撤回，覆盖语义由 client 在自己累积的缓冲上渲染。
fn normalize_carriage_returns(text: &str) -> String {
    let normalized = text.replace("\r\n", "\n");
    let mut lines: Vec<&str> = Vec::new();
    for line in normalized.split('\n') {
        if let Some(first_cr) = line.find('\r') {
            let _ = first_cr;
            if let Some(segment) = line.rsplit('\r').find(|segment| !segment.is_empty()) {
                lines.push(segment)
            } else {
                // 纯 `\r` 行：没有有效内容，整行丢弃。
            }
        } else {
            lines.push(line);
        }
    }
    lines.join("\n")
}

fn shell_summary(view: &ExecOutput, command: Option<&str>) -> String {
    let label = match (
        view.timed_out,
        view.process_id,
        view.exit_code,
        view.status.as_str(),
    ) {
        (true, _, _, _) => "timeout".to_string(),
        (_, _, _, "cancelled") => "cancelled".to_string(),
        (_, Some(pid), None, _) => format!("backgrounded · pid {pid}"),
        (_, _, Some(code), _) => format!("exit {code}"),
        (_, _, None, status) if !status.is_empty() => status.to_string(),
        _ => "completed".to_string(),
    };
    match command.map(short_command).filter(|cmd| !cmd.is_empty()) {
        Some(command) => format!("{label} · {command}"),
        None => label,
    }
}

/// 完整命令是 header 的真相字段；摘要里只放有界显示态（§4.4）。
fn short_command(command: &str) -> String {
    const MAX_CHARS: usize = 80;
    let one_line = command.replace(['\n', '\r'], " ");
    let trimmed = one_line.trim();
    if trimmed.chars().count() <= MAX_CHARS {
        return trimmed.to_string();
    }
    let mut short: String = trimmed.chars().take(MAX_CHARS).collect();
    short.push('…');
    short
}

fn command_from_args(args: &serde_json::Value) -> Option<String> {
    if let Some(argv) = args.get("argv").and_then(|value| value.as_array()) {
        let parts: Vec<&str> = argv.iter().filter_map(|value| value.as_str()).collect();
        if !parts.is_empty() {
            return Some(parts.join(" "));
        }
    }
    let command = args.get("command").and_then(|value| value.as_str())?;
    let mut rendered = String::new();
    if let Some(shell) = args
        .get("shell")
        .and_then(|value| value.as_str())
        .filter(|shell| !shell.is_empty())
    {
        rendered.push_str(shell);
        rendered.push(' ');
    }
    rendered.push_str(command);
    if let Some(extra) = args.get("args").and_then(|value| value.as_array()) {
        for arg in extra.iter().filter_map(|value| value.as_str()) {
            rendered.push(' ');
            rendered.push_str(arg);
        }
    }
    Some(rendered)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_display_carries_command_exit_code_and_shell_body() {
        let args = serde_json::json!({"shell": "bash", "command": "cargo check"});
        let output = r#"{"status":"completed","command":"bash cargo check","exit_code":0,"output":"ok\n","truncated":false,"timed_out":false,"cancelled":false}"#;
        let display = project_display(&args, output);

        assert_eq!(
            display.header,
            ToolHeader::Shell {
                command: "bash cargo check".into()
            }
        );
        assert_eq!(
            display.body,
            ToolBody::Shell {
                output: "ok\n".into(),
                exit_code: Some(0),
                truncated: false,
            }
        );
        assert_eq!(
            display.summary.as_deref(),
            Some("exit 0 · bash cargo check")
        );
        assert!(!display.summary.as_deref().unwrap_or("").contains('{'));
    }

    #[test]
    fn exec_display_maps_backgrounded_and_timeout_without_fake_exit_code() {
        let args = serde_json::json!({"argv": ["cargo", "test"]});
        let backgrounded = r#"{"status":"completed","exit_code":null,"output":"","truncated":false,"timed_out":false,"process_id":42}"#;
        let display = project_display(&args, backgrounded);
        assert_eq!(
            display.body,
            ToolBody::Shell {
                output: String::new(),
                exit_code: None,
                truncated: false,
            }
        );
        assert_eq!(
            display.summary.as_deref(),
            Some("backgrounded · pid 42 · cargo test")
        );

        let timed_out = r#"{"status":"completed","exit_code":null,"output":"","truncated":true,"timed_out":true}"#;
        let display = project_display(&args, timed_out);
        assert_eq!(display.summary.as_deref(), Some("timeout · cargo test"));
    }

    #[test]
    fn builtin_manager_registers_contracted_projectors() {
        let manager = crate::registration::build_tool_manager(&[]);
        let args = serde_json::json!({"command": "ls"});
        let output = r#"{"status":"completed","exit_code":0,"output":"ok","truncated":false,"timed_out":false}"#;
        assert!(
            manager.project_display("exec", &args, output).is_none(),
            "exec is typed and carries display in ToolResult"
        );
        let view: ExecOutput = serde_json::from_str(output).expect("exec output");
        let display = view.display(&args);
        assert!(display.summary.is_some());
        assert!(
            manager.project_display("read", &args, "L1: ok").is_none(),
            "read is typed and carries display in ToolResult"
        );
        assert!(
            manager
                .project_display("read_image", &serde_json::json!({"path": "a.png"}), "image")
                .is_none(),
            "未迁移工具必须保持 display=None，client 完整回退"
        );
    }

    #[test]
    fn exec_display_normalizes_carriage_return_overwrite_in_body_only() {
        let args = serde_json::json!({"command": "apt install"});
        let output = r#"{"status":"completed","exit_code":0,"output":"10%\r20%\r100%\n\rdone\n","truncated":false,"timed_out":false}"#;
        let display = project_display(&args, output);
        match display.body {
            ToolBody::Shell { output, .. } => assert_eq!(output, "100%\ndone\n"),
            other => panic!("expected shell body, got {other:?}"),
        }
    }

    #[test]
    fn exec_display_falls_back_to_text_for_non_json_output() {
        let args = serde_json::json!({"command": "ls"});
        let display = project_display(&args, "[ERROR] manager unavailable");
        assert!(matches!(display.body, ToolBody::Text { .. }));
        assert_eq!(
            display.header,
            ToolHeader::Shell {
                command: "ls".into()
            }
        );
    }
}
