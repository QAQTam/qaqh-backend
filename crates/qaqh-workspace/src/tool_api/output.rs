//! 工具输出模型（base spec §8）。
//!
//! 一次执行 → 四个投影：`output`（canonical，宿主/审计）/ `error`（模型 +
//! timeline failure）/ `model`（provider tool result）/ `display`（timeline、
//! TUI、client）/ `metrics`（telemetry、审计）。
//!
//! 展示结构（`ToolDisplay` 等）以 09-18 展示契约为唯一事实源，见
//! [`super::display`]。

use std::time::Duration;

use serde::Serialize;

use super::display::{
    PathOp, ToolBody, ToolDisplay, ToolDisplayOutcome, ToolHeader, ToolMetrics, ToolTerminalState,
};
use super::error::{ToolError, ToolErrorKind};
use qaqh_types::{
    ContentRef, ToolImage, ToolResultDisplay, ToolResultDisplayBody, ToolResultDisplayHeader,
    ToolResultDisplayOutcome, ToolResultDisplayOutcomeState, ToolResultDisplayPathOp,
};

pub use qaqh_types::ToolStatus;

/// 模型面内容块（base spec §8.1/§9.1）。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum ToolContentBlock {
    /// 文本块。
    Text {
        /// 文本内容。
        text: String,
    },
    /// 图片块（base64 载荷由 gate 在请求构建期降为 provider 原生格式）。
    Image(ToolImage),
    /// 结构化 JSON 块（序列化为 text 时由适配器决定边界）。
    Json {
        /// JSON 值。
        value: serde_json::Value,
    },
}

/// 工具作者声明的输出投影（base spec §8.1）。
///
/// 09-19 补充稿 Q4：原 trait 名 `ToolOutput` 与 09-18 输出侧契约的 typed
/// 容器 `ToolOutput<O>` 撞名，本 trait 更名为 `ToolProjection`（纯重命名；
/// 若评审否决可直接回退）。
#[doc(alias = "ToolOutput")]
pub trait ToolProjection: Serialize {
    /// 权威终态。默认成功；exec 等工具在输出内承载 exit_code / timeout /
    /// cancelled，需要覆盖此方法，避免把非零退出伪装成成功。
    fn status(&self) -> ToolStatus {
        ToolStatus::Ok
    }

    /// 失败终态对应的可恢复错误。默认无错误；覆盖 [`Self::status`] 返回
    /// 失败态时必须同时提供错误，满足 [`ToolOutcome::check_invariants`]。
    fn error(&self) -> Option<ToolError> {
        None
    }

    /// 图片附件。默认无图片；`read_image` 等工具覆盖此方法，避免把
    /// base64 载荷塞进模型文本 JSON。
    fn images(&self) -> Vec<qaqh_types::ToolImage> {
        Vec::new()
    }

    /// 模型投影。为空时由适配器按 base spec §8.1 默认规则生成
    /// （文本序列化 / summary + 有界 JSON）。
    fn model_blocks(&self) -> Vec<ToolContentBlock> {
        Vec::new()
    }

    /// 单行人类可读摘要（展示与模型提示共用）。不得是 JSON（H1）。
    fn summary(&self) -> Option<String> {
        None
    }

    /// 展示投影。`args` 是**已通过 typed 校验的原始参数值**，供 header 提取
    /// 真相字段（path / command / query）；实现不得读取线程局部，也不得重新
    /// 解析未经校验的输入（H13）。
    fn display(&self, _args: &serde_json::Value) -> ToolDisplay {
        ToolDisplay::default()
    }

    /// 宿主侧 typed effects。默认无副作用；需要注入 skill activation 等
    /// 可信状态迁移的工具在此显式返回，禁止让 runtime 解析工具文本猜测。
    fn effects(&self) -> Vec<crate::ToolEffect> {
        Vec::new()
    }
}

/// canonical 输出值（base spec §8.2）：宿主 / 审计 / 后续工具可消费，
/// 不保证直接发给模型。
#[derive(Debug, Clone, PartialEq)]
pub enum ToolOutputValue {
    /// 无输出。
    Empty,
    /// 文本输出。
    Text(String),
    /// 结构化输出。
    Json(serde_json::Value),
    /// 外置内容引用（大内容）。
    ContentRef(ContentRef),
}

/// 模型投影（base spec §8.2）。
///
/// 适配层把它映射到 `qaqh_types::ToolModelPayload`（含 token 统计等 wire 字段）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ToolModelProjection {
    /// 模型可见文本（已按 output_budget 截断）。
    pub text: String,
    /// 是否发生截断。
    pub truncated: bool,
}

/// 执行指标（base spec §8.3）：必须经 outcome 进入 runtime 适配层，
/// 不得生成后丢弃。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ToolExecutionMetrics {
    /// 执行耗时。
    pub elapsed: Duration,
    /// 本次调用产出的展示文本字节数（UTF-8，截断后）。
    pub output_bytes: u64,
    /// 重试次数。
    pub retry_count: u32,
    /// 别名/MCP 解析后的实际工具名；无别名时 = descriptor.name。
    pub effective_tool_name: Option<String>,
    /// 调用来源是否为用户直接发起（宿主侧来源，非工具自报）。
    pub user_initiated: bool,
}

/// 执行结果（base spec §8.2）。
#[derive(Debug)]
pub struct ToolOutcome {
    /// 权威状态（五态，与 wire `qaqh_types::ToolStatus` 同一词汇）。
    pub status: ToolStatus,
    /// canonical 输出。
    pub output: ToolOutputValue,
    /// 可恢复错误（与 status 的不变式见 [`ToolOutcome::check_invariants`]）。
    pub error: Option<ToolError>,
    /// 模型投影。
    pub model: ToolModelProjection,
    /// 展示投影。
    pub display: ToolDisplay,
    /// 图片附件（read_image 等）。
    ///
    /// 09-19 P2 补充：legacy `ToolResult.images` 的落点（wire 适配器据此重建
    /// tool message 的图片部件）；新工具经 `ToolProjection::model_blocks`
    /// 的 `Image` 块表达同一内容。
    pub images: Vec<ToolImage>,
    /// 执行指标。
    pub metrics: ToolExecutionMetrics,
    /// 宿主侧可信 effects（例如 skill activation）。
    pub effects: Vec<crate::ToolEffect>,
}

impl ToolOutcome {
    /// 检查 base spec §7.3 不变量：
    /// `Ok/Backgrounded ⇒ error.is_none()`；`Partial/Cancelled/Error ⇒ error.is_some()`。
    pub fn check_invariants(&self) -> Result<(), &'static str> {
        let success = matches!(self.status, ToolStatus::Ok | ToolStatus::Backgrounded);
        match (success, self.error.is_some()) {
            (true, true) => Err("成功状态不得携带 error（base spec §7.3）"),
            (false, false) => Err("失败状态必须携带 error（base spec §7.3）"),
            _ => Ok(()),
        }
    }

    /// 投影到迁移期 wire 容器 [`qaqh_types::ToolResult`]。
    ///
    /// 这是 typed runtime 与 v1 message/timeline 之间唯一的兼容出口：
    /// canonical typed payload 进入 `ToolResult.data`，model/display/error
    /// 分别沿用既有 wire 字段，旧 client 不需要理解 `ToolOutcome`。
    pub fn to_tool_result(&self) -> qaqh_types::ToolResult {
        let data = match &self.output {
            ToolOutputValue::Empty => serde_json::json!({}),
            ToolOutputValue::Text(text) => serde_json::json!({ "text": text }),
            ToolOutputValue::Json(value) => value.clone(),
            ToolOutputValue::ContentRef(reference) => {
                serde_json::to_value(reference).unwrap_or_else(|_| serde_json::json!({}))
            }
        };
        let mut result = qaqh_types::ToolResult::text(self.status, self.model.text.clone());
        if let Some(summary) = &self.display.summary {
            result = result.with_summary(summary.clone());
        }
        result.data = data;
        result.images = self.images.clone();
        result.diff = self.display.diff.clone();
        result.error = self.error.as_ref().map(|error| qaqh_types::ToolError {
            code: error.code.as_str().to_owned(),
            message: error.detail.clone(),
            retryable: error.retryable,
            hint: error.hint.clone(),
        });
        if let Some(details) = self.error.as_ref().and_then(|error| error.details.as_ref()) {
            // `details` 是对 canonical `data` 的**补充**，不是覆盖：同时带结构化输出
            // 与 details 的工具不能把结构化输出静默丢掉（否则 wire 形态还会随工具作者
            // 是否恰好填了 details 而分叉）。只有「没有任何结构化输出」时，details
            // 才整体充当 `data`。
            let empty_object =
                matches!(&result.data, serde_json::Value::Object(map) if map.is_empty());
            if empty_object {
                result.data = details.clone();
            } else if let serde_json::Value::Object(map) = &mut result.data {
                map.insert("details".to_owned(), details.clone());
            } else {
                let canonical = std::mem::replace(&mut result.data, serde_json::Value::Null);
                result.data = serde_json::json!({ "data": canonical, "details": details });
            }
        }
        result.metrics = qaqh_types::ToolResultMetrics {
            elapsed_ms: Some(self.metrics.elapsed.as_millis() as u64),
            output_bytes: self.metrics.output_bytes,
            retry_count: self.metrics.retry_count,
            effective_tool_name: self.metrics.effective_tool_name.clone(),
            user_initiated: self.metrics.user_initiated,
        };
        let display = self.display_with_framework_outcome();
        if display != ToolDisplay::default() {
            result = result.with_display(to_wire_display(&display));
        }
        result
    }

    /// Fill framework-owned metrics/outcome without mutating the tool's
    /// declared display projection.
    fn display_with_framework_outcome(&self) -> ToolDisplay {
        let mut display = self.display.clone();
        display.metrics = ToolMetrics {
            elapsed_ms: Some(self.metrics.elapsed.as_millis() as u64),
            output_bytes: self.metrics.output_bytes,
            retry_count: self.metrics.retry_count,
            effective_tool_name: self.metrics.effective_tool_name.clone(),
            user_initiated: self.metrics.user_initiated,
        };
        if let Some(outcome) = display.outcome.as_mut() {
            if outcome.duration_ms.is_none() || self.metrics.elapsed != Duration::ZERO {
                outcome.duration_ms = Some(self.metrics.elapsed.as_millis() as u64);
            }
            if outcome.output_bytes.is_none() || self.metrics.output_bytes != 0 {
                outcome.output_bytes = Some(self.metrics.output_bytes);
            }
            let (body_exit_code, body_truncated) = body_terminal_fields(&display.body);
            if outcome.exit_code.is_none() {
                outcome.exit_code = body_exit_code;
            }
            if outcome.truncated.is_none() {
                outcome.truncated = body_truncated.or(self.model.truncated.then_some(true));
            }
        } else {
            display.outcome = Some(self.default_display_outcome(&display));
        }
        display
    }

    fn default_display_outcome(&self, display: &ToolDisplay) -> ToolDisplayOutcome {
        let state = match self.status {
            ToolStatus::Ok => ToolTerminalState::Succeeded,
            ToolStatus::Backgrounded => ToolTerminalState::Backgrounded,
            ToolStatus::Cancelled => ToolTerminalState::Cancelled,
            ToolStatus::Error | ToolStatus::Partial => {
                if self
                    .error
                    .as_ref()
                    .is_some_and(|error| error.kind == ToolErrorKind::Timeout)
                {
                    ToolTerminalState::TimedOut
                } else {
                    ToolTerminalState::Failed
                }
            }
        };
        let (exit_code, mut truncated) = body_terminal_fields(&display.body);
        if truncated.is_none() {
            truncated = self.model.truncated.then_some(true);
        }
        ToolDisplayOutcome {
            state,
            exit_code,
            duration_ms: Some(self.metrics.elapsed.as_millis() as u64),
            output_bytes: Some(self.metrics.output_bytes),
            truncated,
        }
    }
}

fn body_terminal_fields(body: &ToolBody) -> (Option<i32>, Option<bool>) {
    match body {
        ToolBody::Shell {
            exit_code,
            truncated,
            ..
        }
        | ToolBody::Streams {
            exit_code,
            truncated,
            ..
        } => (*exit_code, Some(*truncated)),
        ToolBody::Text { truncated, .. } => (None, Some(*truncated)),
        _ => (None, None),
    }
}

fn to_wire_display(display: &ToolDisplay) -> ToolResultDisplay {
    ToolResultDisplay {
        summary: display.summary.clone(),
        diff: display.diff.clone(),
        lines_added: display.lines_added,
        lines_removed: display.lines_removed,
        header: match &display.header {
            ToolHeader::None => None,
            ToolHeader::Path { path, op } => Some(ToolResultDisplayHeader::Path {
                path: path.clone(),
                op: match op {
                    PathOp::Read => ToolResultDisplayPathOp::Read,
                    PathOp::Write => ToolResultDisplayPathOp::Write,
                    PathOp::Edit => ToolResultDisplayPathOp::Edit,
                    PathOp::List => ToolResultDisplayPathOp::List,
                    PathOp::Patch => ToolResultDisplayPathOp::Patch,
                    PathOp::Delete => ToolResultDisplayPathOp::Delete,
                },
            }),
            ToolHeader::Shell { command } => Some(ToolResultDisplayHeader::Shell {
                command: command.clone(),
            }),
            ToolHeader::Query { query, scope } => Some(ToolResultDisplayHeader::Query {
                query: query.clone(),
                scope: scope.clone(),
            }),
            ToolHeader::Other { label } => Some(ToolResultDisplayHeader::Other {
                label: label.clone(),
            }),
        },
        body: match &display.body {
            ToolBody::None => None,
            ToolBody::Text { text, truncated } => Some(ToolResultDisplayBody::Text {
                text: text.clone(),
                truncated: *truncated,
            }),
            ToolBody::Diff { unified, files } => Some(ToolResultDisplayBody::Diff {
                unified: unified.clone(),
                files: files.clone(),
            }),
            ToolBody::Shell {
                output,
                exit_code,
                truncated,
            } => Some(ToolResultDisplayBody::Shell {
                output: output.clone(),
                exit_code: *exit_code,
                truncated: *truncated,
            }),
            ToolBody::Streams {
                stdout,
                stderr,
                exit_code,
                truncated,
                interleaved,
            } => Some(ToolResultDisplayBody::Streams {
                stdout: stdout.clone(),
                stderr: stderr.clone(),
                exit_code: *exit_code,
                truncated: *truncated,
                interleaved: *interleaved,
            }),
            ToolBody::Subagent { name, session_id } => Some(ToolResultDisplayBody::Subagent {
                name: name.clone(),
                session_id: session_id.clone(),
            }),
        },
        outcome: display.outcome.as_ref().map(to_wire_outcome),
    }
}

fn to_wire_outcome(outcome: &ToolDisplayOutcome) -> ToolResultDisplayOutcome {
    ToolResultDisplayOutcome {
        state: match outcome.state {
            ToolTerminalState::Succeeded => ToolResultDisplayOutcomeState::Succeeded,
            ToolTerminalState::Failed => ToolResultDisplayOutcomeState::Failed,
            ToolTerminalState::Cancelled => ToolResultDisplayOutcomeState::Cancelled,
            ToolTerminalState::TimedOut => ToolResultDisplayOutcomeState::TimedOut,
            ToolTerminalState::Backgrounded => ToolResultDisplayOutcomeState::Backgrounded,
            ToolTerminalState::Unknown => ToolResultDisplayOutcomeState::Unknown,
        },
        exit_code: outcome.exit_code,
        duration_ms: outcome.duration_ms,
        output_bytes: outcome.output_bytes,
        truncated: outcome.truncated,
    }
}

pub(crate) fn from_wire_display(display: &ToolResultDisplay) -> ToolDisplay {
    ToolDisplay {
        summary: display.summary.clone(),
        diff: display.diff.clone(),
        lines_added: display.lines_added,
        lines_removed: display.lines_removed,
        header: match &display.header {
            None => ToolHeader::None,
            Some(ToolResultDisplayHeader::Path { path, op }) => ToolHeader::Path {
                path: path.clone(),
                op: match op {
                    ToolResultDisplayPathOp::Read => PathOp::Read,
                    ToolResultDisplayPathOp::Write => PathOp::Write,
                    ToolResultDisplayPathOp::Edit => PathOp::Edit,
                    ToolResultDisplayPathOp::List => PathOp::List,
                    ToolResultDisplayPathOp::Patch => PathOp::Patch,
                    ToolResultDisplayPathOp::Delete => PathOp::Delete,
                },
            },
            Some(ToolResultDisplayHeader::Shell { command }) => ToolHeader::Shell {
                command: command.clone(),
            },
            Some(ToolResultDisplayHeader::Query { query, scope }) => ToolHeader::Query {
                query: query.clone(),
                scope: scope.clone(),
            },
            Some(ToolResultDisplayHeader::Other { label }) => ToolHeader::Other {
                label: label.clone(),
            },
        },
        body: match &display.body {
            None => ToolBody::None,
            Some(ToolResultDisplayBody::None) => ToolBody::None,
            Some(ToolResultDisplayBody::Text { text, truncated }) => ToolBody::Text {
                text: text.clone(),
                truncated: *truncated,
            },
            Some(ToolResultDisplayBody::Diff { unified, files }) => ToolBody::Diff {
                unified: unified.clone(),
                files: files.clone(),
            },
            Some(ToolResultDisplayBody::Shell {
                output,
                exit_code,
                truncated,
            }) => ToolBody::Shell {
                output: output.clone(),
                exit_code: *exit_code,
                truncated: *truncated,
            },
            Some(ToolResultDisplayBody::Streams {
                stdout,
                stderr,
                exit_code,
                truncated,
                interleaved,
            }) => ToolBody::Streams {
                stdout: stdout.clone(),
                stderr: stderr.clone(),
                exit_code: *exit_code,
                truncated: *truncated,
                interleaved: *interleaved,
            },
            Some(ToolResultDisplayBody::Subagent { name, session_id }) => ToolBody::Subagent {
                name: name.clone(),
                session_id: session_id.clone(),
            },
            Some(ToolResultDisplayBody::Unknown) => ToolBody::None,
        },
        metrics: ToolMetrics::default(),
        outcome: display.outcome.as_ref().map(from_wire_outcome),
    }
}

fn from_wire_outcome(outcome: &ToolResultDisplayOutcome) -> ToolDisplayOutcome {
    ToolDisplayOutcome {
        state: match outcome.state {
            ToolResultDisplayOutcomeState::Succeeded => ToolTerminalState::Succeeded,
            ToolResultDisplayOutcomeState::Failed => ToolTerminalState::Failed,
            ToolResultDisplayOutcomeState::Cancelled => ToolTerminalState::Cancelled,
            ToolResultDisplayOutcomeState::TimedOut => ToolTerminalState::TimedOut,
            ToolResultDisplayOutcomeState::Backgrounded => ToolTerminalState::Backgrounded,
            ToolResultDisplayOutcomeState::Unknown => ToolTerminalState::Unknown,
        },
        exit_code: outcome.exit_code,
        duration_ms: outcome.duration_ms,
        output_bytes: outcome.output_bytes,
        truncated: outcome.truncated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(status: ToolStatus, error: Option<ToolError>) -> ToolOutcome {
        ToolOutcome {
            status,
            output: ToolOutputValue::Empty,
            error,
            model: ToolModelProjection::default(),
            display: ToolDisplay::default(),
            images: Vec::new(),
            metrics: ToolExecutionMetrics::default(),
            effects: Vec::new(),
        }
    }

    #[test]
    fn error_details_complement_structured_output_instead_of_replacing_it() {
        use crate::tool_api::error::ToolErrorKind;

        fn details_error() -> ToolError {
            let mut error = ToolError::new(ToolErrorKind::Execution, "boom");
            error.details = Some(serde_json::json!({ "path": "a.txt" }));
            error
        }

        // 没有结构化输出（read 工具的错误路径）：details 整体充当 data。
        let result = outcome(ToolStatus::Error, Some(details_error())).to_tool_result();
        assert_eq!(result.data, serde_json::json!({ "path": "a.txt" }));

        // 同时有结构化输出：details 是补充，不能把 canonical data 覆盖掉。
        let mut with_output = outcome(ToolStatus::Error, Some(details_error()));
        with_output.output = ToolOutputValue::Json(serde_json::json!({ "hits": 3 }));
        let result = with_output.to_tool_result();
        assert_eq!(result.data["hits"], serde_json::json!(3));
        assert_eq!(result.data["details"]["path"], serde_json::json!("a.txt"));

        // 非对象的结构化输出也不能被丢弃。
        let mut scalar = outcome(ToolStatus::Error, Some(details_error()));
        scalar.output = ToolOutputValue::Json(serde_json::json!([1, 2, 3]));
        let result = scalar.to_tool_result();
        assert_eq!(result.data["data"], serde_json::json!([1, 2, 3]));
        assert_eq!(result.data["details"]["path"], serde_json::json!("a.txt"));
    }

    #[test]
    fn to_tool_result_fills_structured_terminal_outcome() {
        let mut display = ToolDisplay::new(
            ToolHeader::Shell {
                command: "sleep 1".into(),
            },
            ToolBody::Shell {
                output: "".into(),
                exit_code: Some(2),
                truncated: true,
            },
        );
        display.outcome = Some(ToolDisplayOutcome {
            state: ToolTerminalState::TimedOut,
            exit_code: Some(2),
            duration_ms: None,
            output_bytes: None,
            truncated: Some(true),
        });
        let mut result = outcome(
            ToolStatus::Error,
            Some(ToolError::new(ToolErrorKind::Timeout, "timed out")),
        );
        result.display = display;
        result.metrics.elapsed = Duration::from_millis(1500);
        result.metrics.output_bytes = 4096;

        let wire = result.to_tool_result();
        let display = wire.display().expect("display projection");
        let outcome = display.outcome.as_ref().expect("structured outcome");
        assert_eq!(
            outcome.state,
            ToolResultDisplayOutcomeState::TimedOut,
            "timeout must not be inferred from summary text"
        );
        assert_eq!(outcome.exit_code, Some(2));
        assert_eq!(outcome.duration_ms, Some(1500));
        assert_eq!(outcome.output_bytes, Some(4096));
        assert_eq!(outcome.truncated, Some(true));
    }

    #[test]
    fn wire_display_roundtrips_terminal_line_delta() {
        let display = ToolDisplay {
            lines_added: 4,
            lines_removed: 2,
            ..Default::default()
        };

        let wire = to_wire_display(&display);
        assert_eq!((wire.lines_added, wire.lines_removed), (4, 2));

        let back = from_wire_display(&wire);
        assert_eq!((back.lines_added, back.lines_removed), (4, 2));
    }

    #[test]
    fn wire_display_outcome_roundtrips_through_internal_projection() {
        let wire = ToolResultDisplay {
            summary: Some("exit 0".into()),
            diff: None,
            lines_added: 0,
            lines_removed: 0,
            header: None,
            body: None,
            outcome: Some(ToolResultDisplayOutcome {
                state: ToolResultDisplayOutcomeState::Succeeded,
                exit_code: Some(0),
                duration_ms: Some(12),
                output_bytes: Some(2),
                truncated: Some(false),
            }),
        };
        let internal = from_wire_display(&wire);
        let outcome = internal.outcome.expect("internal outcome");
        assert_eq!(outcome.state, ToolTerminalState::Succeeded);
        assert_eq!(outcome.exit_code, Some(0));
        assert_eq!(outcome.duration_ms, Some(12));
        assert_eq!(outcome.output_bytes, Some(2));
        assert_eq!(outcome.truncated, Some(false));
    }

    #[test]
    fn legacy_result_display_outcome_roundtrips_through_tool_outcome() {
        let result = qaqh_types::ToolResult::ok("ok").with_display(ToolResultDisplay {
            summary: Some("exit 0".into()),
            diff: None,
            lines_added: 0,
            lines_removed: 0,
            header: None,
            body: Some(ToolResultDisplayBody::Shell {
                output: "ok".into(),
                exit_code: Some(0),
                truncated: false,
            }),
            outcome: Some(ToolResultDisplayOutcome {
                state: ToolResultDisplayOutcomeState::Succeeded,
                exit_code: Some(0),
                duration_ms: Some(5),
                output_bytes: Some(2),
                truncated: Some(false),
            }),
        });
        let restored = crate::tool_api::map_tool_result(result)
            .to_tool_result()
            .display()
            .and_then(|display| display.outcome.clone())
            .expect("outcome survives legacy mapping");
        assert_eq!(restored.state, ToolResultDisplayOutcomeState::Succeeded);
        assert_eq!(restored.exit_code, Some(0));
        assert_eq!(restored.duration_ms, Some(5));
    }

    #[test]
    fn streams_body_roundtrips_through_wire() {
        let display = ToolDisplay::new(
            ToolHeader::Shell {
                command: "sh -c 'echo out; echo err >&2'".into(),
            },
            ToolBody::Streams {
                stdout: "out\n".into(),
                stderr: "err\n".into(),
                exit_code: Some(1),
                truncated: false,
                interleaved: false,
            },
        );
        let wire = to_wire_display(&display);
        assert!(matches!(
            wire.body,
            Some(ToolResultDisplayBody::Streams {
                ref stdout,
                ref stderr,
                exit_code: Some(1),
                interleaved: false,
                ..
            }) if stdout == "out\n" && stderr == "err\n"
        ));
        assert_eq!(from_wire_display(&wire).body, display.body);
    }

    #[test]
    fn invariants_hold_for_consistent_outcomes() {
        assert!(outcome(ToolStatus::Ok, None).check_invariants().is_ok());
        assert!(
            outcome(ToolStatus::Backgrounded, None)
                .check_invariants()
                .is_ok()
        );
        assert!(
            outcome(ToolStatus::Error, Some(ToolError::not_found("x")))
                .check_invariants()
                .is_ok()
        );
    }

    #[test]
    fn invariants_reject_inconsistent_outcomes() {
        assert!(
            outcome(ToolStatus::Ok, Some(ToolError::cancelled("x")))
                .check_invariants()
                .is_err(),
            "成功状态带 error 违反不变量"
        );
        assert!(
            outcome(ToolStatus::Partial, None)
                .check_invariants()
                .is_err(),
            "失败状态缺 error 违反不变量"
        );
    }

    #[test]
    fn default_projection_is_empty() {
        struct Empty;
        impl Serialize for Empty {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_none()
            }
        }
        impl ToolProjection for Empty {}

        assert!(Empty.model_blocks().is_empty());
        assert_eq!(Empty.summary(), None);
        assert_eq!(
            Empty.display(&serde_json::json!({})),
            ToolDisplay::default()
        );
    }
}
