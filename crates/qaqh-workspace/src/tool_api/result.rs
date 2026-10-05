//! v1 `ToolResult` → [`ToolOutcome`] 映射与进度桥（唯一权威）。
//!
//! 本模块只剩两个"结果面"翻译函数：
//! - [`map_tool_result`]：v1 `ToolResult` 信封 → typed [`ToolOutcome`]。MCP/LSP
//!   内部管线仍是 v1 信封（模型面 §7 逐字节契约），由此收口；
//! - [`bridge_progress`]：`ExecProgressSender`（有界、可丢弃）→ typed
//!   [`ProgressSink`]。
//!
//! v1 载体（`ToolHandler` / `LegacyToolAdapter` / `ToolCallCtx`）已随 typed
//! 执行面统一而退役。
//!
//! ## 映射语义
//!
//! | v1 `ToolResult` | [`ToolOutcome`] |
//! |---|---|
//! | `status`（五态） | `status`（同一词汇） |
//! | `output_ref` / `data` | `output`：`ContentRef` 优先，否则 `Json(data)` |
//! | `error{code,message,retryable,hint}` | `error`（code 为合法 snake_case 时**原样保留**，否则回退 kind 内置码；kind 由 code 推导） |
//! | `model.text/truncated` | `model` |
//! | `summary` / `diff` | `display.summary` / `display.diff`（header/body 由展示投影器填充） |
//! | `images` | `images` |
//! | `metrics` | `metrics`（`elapsed_ms → Duration`） |

use std::time::Duration;

use super::error::{ToolError, ToolErrorCode, ToolErrorKind};
use super::output::{ToolExecutionMetrics, ToolModelProjection, ToolOutcome, ToolOutputValue};
use super::progress::{ProgressSink, ProgressStream, ToolProgress};
use crate::{ExecProgressSender, ToolResult};

/// `ToolResult` → [`ToolOutcome`] 映射（唯一权威；MCP/LSP 内部 v1 管线经此收口）。
pub fn map_tool_result(result: ToolResult) -> ToolOutcome {
    let status = result.status;
    let output = match result.output_ref() {
        // 大输出外置：canonical 全量内容在引用处。
        Some(reference) => ToolOutputValue::ContentRef(reference.clone()),
        None => ToolOutputValue::Json(result.data.clone()),
    };

    let model = ToolModelProjection {
        text: result.model_text().to_owned(),
        truncated: result.model_truncated(),
    };

    let mut display = result
        .display()
        .map(super::output::from_wire_display)
        .unwrap_or_default();
    let summary = result.summary();
    if !summary.is_empty() {
        display.summary = Some(summary.to_owned());
    }
    display.diff = result.diff.clone();

    let metrics = ToolExecutionMetrics {
        elapsed: Duration::from_millis(result.metrics.elapsed_ms.unwrap_or(0)),
        output_bytes: result.metrics.output_bytes,
        retry_count: result.metrics.retry_count,
        effective_tool_name: result.metrics.effective_tool_name.clone(),
        user_initiated: result.metrics.user_initiated,
    };

    let error = match result.error.as_ref() {
        Some(error) => Some(map_error(error)),
        // 不变量兜底：v1 约定 failure ⇒ error，但 validate() 不在生产路径
        // 强制执行；缺 error 时按 summary 合成，保证 SDK 不变量成立。
        None if status.is_failure() => Some(ToolError::new(
            if status == qaqh_types::ToolStatus::Cancelled {
                ToolErrorKind::Cancelled
            } else {
                ToolErrorKind::Execution
            },
            summary.to_owned(),
        )),
        None => None,
    };

    ToolOutcome {
        status,
        output,
        error,
        model,
        display,
        images: result.images.clone(),
        metrics,
        effects: Vec::new(),
    }
}

/// v1 错误码 → SDK 错误分类（未识别码归 `Custom`；code 为合法 snake_case 时原样保留，否则回退 kind 内置码）。
fn map_error_kind(code: &str) -> ToolErrorKind {
    match code {
        "invalid_arguments" | "invalid_args" => ToolErrorKind::InvalidArguments,
        "unknown_tool" | "not_found" => ToolErrorKind::NotFound,
        "permission_denied"
        | "permission_required"
        | "blocked_by_mode"
        | "session_mismatch"
        | "resource_mismatch"
        | "workspace_mismatch" => ToolErrorKind::PermissionDenied,
        "cancelled" => ToolErrorKind::Cancelled,
        "timeout" => ToolErrorKind::Timeout,
        "manager_unavailable" | "runtime_not_initialized" => ToolErrorKind::Unavailable,
        "partial" | "io_error" | "internal_error" | "tool_error" | "prepare_rejected" => {
            ToolErrorKind::Execution
        }
        _ => ToolErrorKind::Custom,
    }
}

/// v1 错误 → SDK 错误（code 为 conforming snake_case 时保留，kind 由 code 推导）。
fn map_error(error: &qaqh_types::ToolError) -> ToolError {
    let kind = map_error_kind(&error.code);
    let mut mapped = ToolError::new(kind, error.message.clone()).with_retryable(error.retryable);
    mapped.code = ToolErrorCode::parse_or_builtin(&error.code, kind);
    if let Some(hint) = &error.hint {
        mapped = mapped.with_hint(hint.clone());
    }
    mapped
}

/// 建立 v1 进度 → [`ProgressSink`] 的转发桥。
///
/// 返回给 handle 侧的 sender 被丢弃后，转发线程自行退出；线程分离运行
/// （不 join）——后台进程可能在工具返回后继续产生进度帧。
pub(crate) fn bridge_progress(sink: &ProgressSink) -> ExecProgressSender {
    let (tx, rx) = crate::bounded_exec_progress_channel();
    let sink = sink.clone();
    let spawned = std::thread::Builder::new()
        .name("tool-progress-bridge".to_owned())
        .spawn(move || {
            while let Ok(event) = rx.recv() {
                let stream = match event.stream {
                    crate::ExecOutputStream::Stdout => ProgressStream::Stdout,
                    crate::ExecOutputStream::Stderr => ProgressStream::Stderr,
                };
                sink.emit(ToolProgress::Text {
                    stream,
                    text: event.chunk,
                });
            }
        });
    if spawned.is_err() {
        // 转发线程拉起失败：进度静默降级（进度可丢弃，不影响执行）。
        log::warn!("进度桥转发线程拉起失败，进度将不被转发");
    }
    tx
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::*;
    use crate::tool_api::ToolProgress;
    use crate::tool_api::context::{AgentMode, CancellationToken, SandboxMode};

    #[test]
    fn success_maps_status_model_and_display_summary() {
        let outcome = map_tool_result(ToolResult::ok("hello"));
        assert_eq!(outcome.status, qaqh_types::ToolStatus::Ok);
        assert_eq!(outcome.model.text, "hello");
        assert!(!outcome.model.truncated);
        assert_eq!(outcome.display.summary.as_deref(), Some("hello"));
        assert!(outcome.error.is_none());
        assert_eq!(outcome.check_invariants(), Ok(()));
    }

    #[test]
    fn error_code_and_kind_are_preserved() {
        let outcome = map_tool_result(ToolResult::error_with(
            "not_found",
            "file missing",
            false,
            Some("check the path".to_owned()),
        ));
        assert_eq!(outcome.status, qaqh_types::ToolStatus::Error);
        assert_eq!(outcome.check_invariants(), Ok(()));
        let error = outcome.error.expect("error 必须存在");
        assert_eq!(error.kind, ToolErrorKind::NotFound);
        assert_eq!(error.code.as_str(), "not_found", "v1 code 原样保留");
        assert_eq!(error.detail, "file missing");
        assert_eq!(error.hint.as_deref(), Some("check the path"));
        assert!(!error.retryable);
    }

    #[test]
    fn cancelled_status_maps_to_cancelled_error() {
        let outcome = map_tool_result(ToolResult::cancelled("stopped by user"));
        assert_eq!(outcome.status, qaqh_types::ToolStatus::Cancelled);
        assert_eq!(outcome.check_invariants(), Ok(()));
        assert_eq!(outcome.error.expect("error").kind, ToolErrorKind::Cancelled);
    }

    #[test]
    fn partial_and_backgrounded_keep_invariants() {
        let partial = map_tool_result(ToolResult::partial("stopped early"));
        assert_eq!(partial.status, qaqh_types::ToolStatus::Partial);
        assert_eq!(partial.check_invariants(), Ok(()));
        assert_eq!(
            partial.error.expect("partial 必须带 error").code.as_str(),
            "partial"
        );

        let backgrounded = map_tool_result(ToolResult::backgrounded("running as bg-1"));
        assert_eq!(backgrounded.status, qaqh_types::ToolStatus::Backgrounded);
        assert!(backgrounded.error.is_none());
        assert_eq!(backgrounded.check_invariants(), Ok(()));
    }

    #[test]
    fn failure_without_error_is_synthesized_to_keep_invariants() {
        let mut result = ToolResult::ok("half done");
        result.status = qaqh_types::ToolStatus::Error; // 违反 v1 约定：failure 无 error
        let outcome = map_tool_result(result);
        assert_eq!(outcome.check_invariants(), Ok(()));
        assert_eq!(
            outcome.error.expect("应合成 error 以维持不变量").kind,
            ToolErrorKind::Execution
        );
    }

    #[test]
    fn output_ref_takes_priority_over_data() {
        let mut result = ToolResult::ok_data(serde_json::json!({"path": "big.log"}), "head");
        result.externalize_output(
            "retained head".to_owned(),
            "big.log head".to_owned(),
            qaqh_types::ContentRef {
                content_id: "c_1".to_owned(),
                media_type: "text/plain".to_owned(),
                sha256: "0".repeat(64),
                truncated: true,
            },
        );
        match map_tool_result(result).output {
            ToolOutputValue::ContentRef(reference) => assert_eq!(reference.content_id, "c_1"),
            other => panic!("期望 ContentRef，得到 {other:?}"),
        }
    }

    #[test]
    fn images_and_data_map_into_outcome() {
        let result = ToolResult::ok_data(serde_json::json!({"path": "img.png"}), "captured")
            .with_image("image/png", "aGVsbG8=");
        let outcome = map_tool_result(result);
        assert_eq!(outcome.images.len(), 1, "图片附件不得丢失");
        assert_eq!(outcome.images[0].mime_type, "image/png");
        match &outcome.output {
            ToolOutputValue::Json(data) => assert_eq!(data["path"], "img.png"),
            other => panic!("期望 Json(data)，得到 {other:?}"),
        }
    }

    #[test]
    fn metrics_and_diff_map_into_outcome() {
        let mut result = ToolResult::ok("wrote file").with_diff("--- a\n+++ b\n");
        result.metrics.elapsed_ms = Some(42);
        result.metrics.output_bytes = 7;
        result.metrics.retry_count = 2;
        result.metrics.effective_tool_name = Some("read_alias".to_owned());
        result.metrics.user_initiated = true;
        let outcome = map_tool_result(result);
        assert_eq!(outcome.metrics.elapsed, Duration::from_millis(42));
        assert_eq!(outcome.metrics.output_bytes, 7);
        assert_eq!(outcome.metrics.retry_count, 2);
        assert_eq!(
            outcome.metrics.effective_tool_name.as_deref(),
            Some("read_alias")
        );
        assert!(outcome.metrics.user_initiated);
        assert_eq!(
            outcome.display.diff.as_deref(),
            Some("--- a\n+++ b\n"),
            "diff 只进展示面"
        );
    }

    #[test]
    fn bridge_progress_forwards_frames_to_sink() {
        let (sink, rx) = ProgressSink::channel(8);
        let tx = bridge_progress(&sink);
        tx.try_send(crate::ExecProgressEvent {
            tool_call_id: "c1".to_owned(),
            stream: crate::ExecOutputStream::Stdout,
            seq: 1,
            chunk: "chunk-1".to_owned(),
            bytes_total: 0,
        });
        let frame = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("进度帧应在有界时间内转发");
        match frame {
            ToolProgress::Text { stream, text } => {
                assert_eq!(stream, ProgressStream::Stdout);
                assert_eq!(text, "chunk-1");
            }
            other => panic!("期望 Text 帧，得到 {other:?}"),
        }
    }

    #[test]
    fn cancellation_token_shared_flag_is_observable() {
        let token = CancellationToken::new();
        assert!(!token.is_cancelled());
        token.cancel();
        assert!(token.shared_flag().load(Ordering::SeqCst));
        let _ = AgentMode::Code;
        let _ = SandboxMode::Main;
    }
}
