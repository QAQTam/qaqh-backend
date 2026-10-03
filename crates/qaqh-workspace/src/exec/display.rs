//! exec 展示投影（09-18 跨仓展示契约 §3.2 / §7 工具映射表）。
//!
//! 投影由 typed `ExecOutput` 直接派生；legacy 字符串路径只在确认是 exec
//! 自己的 canonical JSON 时复用同一投影函数，不做 client 侧 JSON 考古。

use super::direct::ExecOutput;
use crate::tool_api::display::clamp_display_body;
use crate::tool_api::{
    ToolBody, ToolContentBlock, ToolDisplay, ToolDisplayOutcome, ToolError, ToolErrorKind,
    ToolHeader, ToolProjection, ToolStatus, ToolTerminalState,
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
        // 失败槽的 detail 是给账本与 timeline 状态行的人读一句话，不是正文：
        // stdout/stderr 证据由 display body（与模型向 JSON）承载，这里再塞
        // `to_json()` 会让失败槽与正文逐字重复。timeout/cancelled 连退出码
        // 都没有，kind 一并给准，code 随 kind 推导。
        let (kind, detail) = if self.timed_out {
            (ToolErrorKind::Timeout, "timeout".to_owned())
        } else if self.cancelled {
            (ToolErrorKind::Cancelled, "cancelled".to_owned())
        } else {
            (
                ToolErrorKind::Execution,
                format!(
                    "exit {}",
                    self.exit_code.map_or("?".into(), |code| code.to_string())
                ),
            )
        };
        Some(ToolError::new(kind, detail))
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
    let state = if view.timed_out {
        ToolTerminalState::TimedOut
    } else if view.cancelled {
        ToolTerminalState::Cancelled
    } else if view.exit_code == Some(0) {
        ToolTerminalState::Succeeded
    } else if view.exit_code.is_none() && view.process_id.is_some() {
        ToolTerminalState::Backgrounded
    } else {
        ToolTerminalState::Failed
    };
    let (body, body_truncated) = if view.stdout.is_empty() && view.stderr.is_empty() {
        (
            ToolBody::Shell {
                // 终态正文在后端完成 `\r` 覆盖归一化（§5.2）；模型文本不变。
                output: normalize_carriage_returns(&view.output),
                exit_code: view.exit_code,
                truncated: view.truncated,
            },
            view.truncated,
        )
    } else {
        let (stdout, stdout_truncated) =
            clamp_display_body(&normalize_carriage_returns(&view.stdout));
        let (stderr, stderr_truncated) =
            clamp_display_body(&normalize_carriage_returns(&view.stderr));
        let truncated = view.truncated || stdout_truncated || stderr_truncated;
        (
            ToolBody::Streams {
                stdout,
                stderr,
                exit_code: view.exit_code,
                truncated,
                // 当前没有 pipe 层全局序号；两条流已分开，不伪装真实交织顺序。
                interleaved: false,
            },
            truncated,
        )
    };
    ToolDisplay::new(header, body)
        .with_summary(summary)
        .with_outcome(ToolDisplayOutcome {
            state,
            exit_code: view.exit_code,
            duration_ms: None,
            output_bytes: None,
            truncated: Some(body_truncated),
        })
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
        let outcome = display.outcome.as_ref().expect("structured outcome");
        assert_eq!(outcome.state, ToolTerminalState::Succeeded);
        assert_eq!(outcome.exit_code, Some(0));
        assert_eq!(outcome.truncated, Some(false));
    }

    #[test]
    fn exec_display_separates_stdout_and_stderr_for_v2() {
        let args = serde_json::json!({"command": "sh -c 'echo out; echo err >&2'"});
        let view = ExecOutput {
            status: "completed".into(),
            command: "sh -c 'echo out; echo err >&2'".into(),
            exit_code: Some(0),
            output: "err\nout\n".into(),
            stdout: "out\n".into(),
            stderr: "err\n".into(),
            truncated: false,
            timed_out: false,
            cancelled: false,
            process_id: None,
        };
        let display = view.display(&args);

        assert_eq!(
            display.body,
            ToolBody::Streams {
                stdout: "out\n".into(),
                stderr: "err\n".into(),
                exit_code: Some(0),
                truncated: false,
                interleaved: false,
            }
        );
        assert_eq!(
            display.outcome.as_ref().map(|outcome| outcome.state),
            Some(ToolTerminalState::Succeeded)
        );
    }

    #[test]
    fn exec_display_maps_backgrounded_and_timeout_without_fake_exit_code() {
        let args = serde_json::json!({"command": "cargo test"});
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
        assert_eq!(
            display.outcome.as_ref().map(|outcome| outcome.state),
            Some(ToolTerminalState::Backgrounded)
        );

        let timed_out = r#"{"status":"completed","exit_code":null,"output":"","truncated":true,"timed_out":true}"#;
        let display = project_display(&args, timed_out);
        assert_eq!(display.summary.as_deref(), Some("timeout · cargo test"));
        let outcome = display.outcome.as_ref().expect("timeout outcome");
        assert_eq!(outcome.state, ToolTerminalState::TimedOut);
        assert_eq!(outcome.exit_code, None);
        assert_eq!(outcome.truncated, Some(true));
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

    /// 失败槽 detail 是给人读的一句话（exit N / timeout / cancelled），不再把
    /// 整段模型向 JSON 塞进 `error.detail`——那曾让 timeline 失败槽与正文逐字
    /// 重复，TUI 状态行/正文双重显示。
    #[test]
    fn exec_error_slot_is_human_reason_not_a_json_copy() {
        let mut view = ExecOutput {
            status: "completed".into(),
            command: "cargo build".into(),
            exit_code: Some(101),
            output: String::new(),
            stdout: String::new(),
            stderr: "error: could not compile".into(),
            truncated: false,
            timed_out: false,
            cancelled: false,
            process_id: None,
        };
        let error = view.error().expect("failed exec carries error");
        assert_eq!(error.code.as_str(), "execution");
        assert_eq!(error.detail, "exit 101");
        assert!(!error.detail.contains('{'), "detail 不得是 JSON: {error:?}");

        view.timed_out = true;
        view.exit_code = None;
        let error = view.error().expect("timeout carries error");
        assert_eq!(error.code.as_str(), "timeout");
        assert_eq!(error.detail, "timeout");

        view.timed_out = false;
        view.cancelled = true;
        let error = view.error().expect("cancelled carries error");
        assert_eq!(error.code.as_str(), "cancelled");
        assert_eq!(error.detail, "cancelled");
    }

    /// LLM 传输面回归锁：错误槽改造不得动模型可见内容——model_blocks 仍是
    /// canonical JSON，summary 仍是 `exit N · command`。
    #[test]
    fn exec_model_face_is_unchanged_by_error_slot_rework() {
        let view = ExecOutput {
            status: "completed".into(),
            command: "cargo build".into(),
            exit_code: Some(101),
            output: "partial".into(),
            stdout: String::new(),
            stderr: "error: could not compile".into(),
            truncated: false,
            timed_out: false,
            cancelled: false,
            process_id: None,
        };
        let blocks = view.model_blocks();
        assert_eq!(blocks.len(), 1);
        let ToolContentBlock::Text { text } = &blocks[0] else {
            panic!("exec model block must be text");
        };
        assert_eq!(text, &view.to_json(), "模型面必须仍是 canonical JSON");
        // stdout/stderr 是 `#[serde(skip)]` 的 display-only 字段；模型 JSON 带
        // 的是合并 `output`。锁的是形状不变：合并正文仍在模型面。
        assert!(text.contains("partial"), "合并正文仍在模型面");
        assert!(!text.contains("stdout"), "display-only 流字段不进模型 JSON");
        // summary() 不带 args（命令后缀走 display(&args) 路径），此处只锁
        // 「错误槽改造未影响 summary 投影」。
        assert_eq!(view.summary(), Some("exit 101".to_string()));
    }
}
