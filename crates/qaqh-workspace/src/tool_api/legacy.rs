//! LegacyToolAdapter：把 v1 `ToolHandler` 包成 [`ErasedTool`]。
//!
//! 本模块同时是 **`ToolResult` → [`ToolOutcome`] 映射的唯一权威**
//! （[`map_tool_result`]）：v1 工具与 MCP/LSP 的 v1 内部管线都经此映射收口
//! 到 typed 结果面。
//!
//! 使用面：仅 `ToolManager::register`（test-harness 门控）。生产内置工具走
//! `register_typed`，动态工具走 [`super::dynamic::DynamicToolAdapter`]。
//!
//! ## 映射语义（P2 决议）
//!
//! | legacy `ToolResult` | [`ToolOutcome`] |
//! |---|---|
//! | `status`（五态） | `status`（同一词汇） |
//! | `output_ref` / `data` | `output`：`ContentRef` 优先，否则 `Json(data)` |
//! | `error{code,message,retryable,hint}` | `error`（legacy code 为合法 snake_case 时**原样保留**，否则回退 kind 内置码；kind 由 code 推导） |
//! | `model.text/truncated` | `model` |
//! | `summary` / `diff` | `display.summary` / `display.diff`（header/body 由展示投影器填充） |
//! | `images` | `images`（09-19 P2 补充字段；wire 适配器据此重建图片附件） |
//! | `metrics` | `metrics`（`elapsed_ms → Duration`） |
//!
//! ## 已知边界
//!
//! - **workspace 注入**：legacy handler 经线程局部读 workspace；适配器在
//!   `execute_legacy` 里把显式 [`ToolCallContext`] 装成该兼容视图
//!   （`install_tool_call_context`），因此 `workspace_root` 会被 legacy 工具
//!   消费，显式上下文始终是唯一事实源。
//! - **action 后缀**：新契约无 `action`（legacy 的 `{name}_{action}` 解析属于
//!   admit 侧），适配器填 `""`。
//! - **宿主副作用**：skill effects 经 [`LegacyCallOutcome::effects`] 显式返回；
//!   [`ErasedTool::execute`] 路径会丢弃并告警（生产路径尚未接线，见 Q8）。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::capabilities::ToolCapabilities;
use super::context::ToolCallContext;
use super::descriptor::{
    DescriptorError, OutputBudget, ToolDescriptor, ToolExposure, ToolName, ToolSource,
};
use super::erased::ErasedTool;
use super::error::{FatalToolError, ToolError, ToolErrorCode, ToolErrorKind};
use super::output::{ToolExecutionMetrics, ToolModelProjection, ToolOutcome, ToolOutputValue};
use super::progress::{ProgressSink, ProgressStream, ToolProgress};
use crate::{ExecProgressSender, ToolCallCtx, ToolEffect, ToolHandler, ToolResult};

/// legacy 工具包装器（实现 [`ErasedTool`]）。
///
/// 仅服务 `ToolManager::register`（test-harness 门控）：动态工具（MCP/LSP）
/// 走 [`super::dynamic::DynamicToolAdapter`]，不再经本适配器。
pub struct LegacyToolAdapter {
    handler: ToolHandler,
    name: ToolName,
    source: ToolSource,
    exposure: ToolExposure,
    capabilities: ToolCapabilities,
    output_budget: OutputBudget,
    output_schema: serde_json::Value,
}

/// 一次 legacy 调用的完整结果。
#[derive(Debug)]
pub struct LegacyCallOutcome {
    /// 映射后的执行结果。
    pub outcome: ToolOutcome,
    /// 宿主副作用（v1: skill activations）。
    ///
    /// 09-19 P2 发现的契约缺口（Q8）：SDK 核心 `ToolOutcome` 不含宿主类型，
    /// effects 经本字段显式返回；接线时由 runtime 消费。
    pub effects: Vec<ToolEffect>,
}

impl LegacyToolAdapter {
    /// 包装一个 legacy handler。
    ///
    /// 构造期校验：名称合法、描述非空、input_schema 为 object、非交互工具
    /// 超时非零（即 [`ToolDescriptor::validate`] 全量通过）；非法直接返回错误。
    pub fn new(handler: ToolHandler) -> Result<Self, DescriptorError> {
        Self::from_handler(handler, ToolCapabilities::default())
    }

    /// 包装一个 legacy handler，并注入显式 capability。
    ///
    /// `ask` 等交互工具用 `interactive = true` 表达“无默认超时”，避免把
    /// `Duration::ZERO` 误当成普通工具超时。
    pub fn new_with_capabilities(
        handler: ToolHandler,
        capabilities: ToolCapabilities,
    ) -> Result<Self, DescriptorError> {
        Self::from_handler(handler, capabilities)
    }

    fn from_handler(
        handler: ToolHandler,
        capabilities: ToolCapabilities,
    ) -> Result<Self, DescriptorError> {
        let name = ToolName::new(&handler.key)?;
        let adapter = Self {
            handler,
            name,
            source: ToolSource::Builtin,
            exposure: ToolExposure::Direct,
            capabilities,
            output_budget: OutputBudget::default(),
            output_schema: default_output_schema(),
        };
        adapter.descriptor().validate()?;
        Ok(adapter)
    }

    /// 覆盖来源（默认 [`ToolSource::Builtin`]）。
    pub fn with_source(mut self, source: ToolSource) -> Self {
        self.source = source;
        self
    }

    /// 覆盖暴露面（默认 [`ToolExposure::Direct`]）。
    pub fn with_exposure(mut self, exposure: ToolExposure) -> Self {
        self.exposure = exposure;
        self
    }

    /// 覆盖编排能力声明（默认保守档）。
    pub fn with_capabilities(mut self, capabilities: ToolCapabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// 覆盖输出预算。
    pub fn with_output_budget(mut self, output_budget: OutputBudget) -> Self {
        self.output_budget = output_budget;
        self
    }

    /// 覆盖 output schema（legacy 无此字段，默认 `{"type":"object"}`）。
    pub fn with_output_schema(mut self, output_schema: serde_json::Value) -> Self {
        self.output_schema = output_schema;
        self
    }

    /// 工具描述符（由 legacy 字段构造）。
    pub fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: self.name.clone(),
            display_name: None,
            description: self.handler.description.to_owned(),
            input_schema: self.handler.input_schema.clone(),
            output_schema: self.output_schema.clone(),
            category: self.handler.category,
            risk: self.handler.risk.clone(),
            default_timeout: self.handler.default_timeout,
            exposure: self.exposure,
            source: self.source,
            output_budget: self.output_budget.clone(),
            capabilities: self.capabilities.clone(),
        }
    }

    /// 执行一次 legacy 调用，返回映射结果与宿主副作用。
    ///
    /// 进度桥接：`ctx.progress` 非空时建立有界转发线程
    /// （`ExecProgressEvent` → [`ToolProgress::Text`]，可丢弃、不阻塞工具）。
    pub fn execute_legacy(
        &self,
        ctx: &ToolCallContext,
        args: serde_json::Value,
    ) -> Result<LegacyCallOutcome, FatalToolError> {
        let progress = ctx.progress.as_ref().map(bridge_progress);
        let legacy_ctx = build_legacy_ctx(&self.name, ctx, args, progress);
        // Legacy handlers still read workspace/session/mode/sandbox/cancel via
        // thread-local accessors. Install the explicit context as that
        // compatibility view; the context remains the source of truth.
        let _scope = crate::runtime::install_tool_call_context(ctx);
        // ToolCallCtx 克隆共享 skill_effects 单元；handler 消费一个克隆，
        // 本函数从原值取回副作用。
        let result = (self.handler.handler)(legacy_ctx.clone());
        let effects = legacy_ctx.take_skill_effects();
        Ok(LegacyCallOutcome {
            outcome: map_tool_result(result),
            effects,
        })
    }
}

fn build_legacy_ctx(
    name: &ToolName,
    ctx: &ToolCallContext,
    args: serde_json::Value,
    progress: Option<ExecProgressSender>,
) -> ToolCallCtx {
    ToolCallCtx {
        id: ctx.call_id.clone(),
        name: name.as_str().to_owned(),
        args,
        tx_progress: progress,
        timeout_secs: Some(ctx.timeout.as_secs()),
        // 与 ToolCallContext.cancellation 共享同一信号（exec/process 轮询它）。
        cancel: ctx.cancellation.shared_flag(),
        skill_effects: Arc::new(Mutex::new(Vec::new())),
    }
}

impl ErasedTool for LegacyToolAdapter {
    fn descriptor(&self) -> ToolDescriptor {
        Self::descriptor(self)
    }

    fn execute(
        &self,
        ctx: ToolCallContext,
        args: serde_json::Value,
    ) -> Result<ToolOutcome, FatalToolError> {
        let call = self.execute_legacy(&ctx, args)?;
        if !call.effects.is_empty() {
            log::warn!(
                "LegacyToolAdapter: 工具 '{}' 产生 {} 个宿主副作用，经 execute() 路径被丢弃；接线前请使用 execute_legacy()（P2.5）",
                self.name,
                call.effects.len()
            );
        }
        Ok(call.outcome)
    }
}

/// legacy output schema 占位（legacy 工具未声明输出契约）。
fn default_output_schema() -> serde_json::Value {
    serde_json::json!({"type": "object"})
}

/// `ToolResult` → [`ToolOutcome`] 映射（唯一权威，供适配器与 wire 适配器复用）。
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
        // 不变量兜底：legacy 约定 failure ⇒ error，但 validate() 不在生产路径
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

/// legacy 错误码 → SDK 错误分类（未识别码归 `Custom`；code 为合法 snake_case 时原样保留，否则回退 kind 内置码）。
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

/// legacy 错误 → SDK 错误（code 为 conforming snake_case 时保留，kind 由 code 推导）。
fn map_error(error: &qaqh_types::ToolError) -> ToolError {
    let kind = map_error_kind(&error.code);
    let mut mapped = ToolError::new(kind, error.message.clone()).with_retryable(error.retryable);
    mapped.code = ToolErrorCode::parse_or_builtin(&error.code, kind);
    if let Some(hint) = &error.hint {
        mapped = mapped.with_hint(hint.clone());
    }
    mapped
}

/// 建立 legacy 进度 → [`ProgressSink`] 的转发桥。
///
/// 返回给 legacy handler 的 sender 被丢弃后，转发线程自行退出；
/// 线程分离运行（不 join）——后台进程可能在工具返回后继续产生进度帧。
pub(crate) fn bridge_progress(sink: &ProgressSink) -> ExecProgressSender {
    let (tx, rx) = crate::bounded_exec_progress_channel();
    let sink = sink.clone();
    let spawned = std::thread::Builder::new()
        .name("legacy-progress-bridge".to_owned())
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
        log::warn!("legacy 进度桥转发线程拉起失败，进度将不被转发");
    }
    tx
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::*;
    use crate::ToolRisk;
    use crate::permission::{PermissionLevel, ToolCategory};
    use crate::tool_api::context::{AgentMode, CancellationToken, SandboxMode};

    fn legacy_handler(key: &str, handler: fn(ToolCallCtx) -> ToolResult) -> ToolHandler {
        ToolHandler {
            key: key.to_owned(),
            description: "测试工具",
            input_schema: serde_json::json!({"type": "object"}),
            handler,
            risk: ToolRisk::ReadOnly,
            category: ToolCategory::Read,
            default_timeout: Duration::from_secs(30),
        }
    }

    fn test_ctx(progress: Option<ProgressSink>) -> ToolCallContext {
        ToolCallContext {
            call_id: "call_1".to_owned(),
            session_id: "seed_1".to_owned(),
            workspace_root: std::path::PathBuf::from("/tmp/ws"),
            mode: AgentMode::Code,
            permission_level: PermissionLevel::ReadOnly,
            sandbox: SandboxMode::Main,
            sandbox_spec: crate::tool_api::SandboxSpec::workspace_write(std::path::PathBuf::from(
                "/tmp/ws",
            )),
            exec_default_shell: None,
            timeout: Duration::from_secs(30),
            cancellation: CancellationToken::new(),
            progress,
            source: super::super::context::ToolCallSource::Model,
        }
    }

    fn echo_handler(ctx: ToolCallCtx) -> ToolResult {
        let text = ctx
            .args
            .get("text")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_owned();
        ToolResult::ok(text)
    }

    fn context_handler(_ctx: ToolCallCtx) -> ToolResult {
        ToolResult::ok(format!(
            "{}|{}|{}|{}",
            crate::current_workspace(),
            crate::current_session().unwrap_or_default(),
            crate::runtime::current_mode(),
            crate::authorization::is_subagent_sandbox(),
        ))
    }

    fn diff_handler(_ctx: ToolCallCtx) -> ToolResult {
        ToolResult::ok("wrote file").with_diff("--- a\n+++ b\n")
    }

    fn error_handler(_ctx: ToolCallCtx) -> ToolResult {
        ToolResult::error_with(
            "not_found",
            "file missing",
            false,
            Some("check the path".to_owned()),
        )
    }

    fn cancelled_handler(_ctx: ToolCallCtx) -> ToolResult {
        ToolResult::cancelled("stopped by user")
    }

    fn cancel_probe_handler(ctx: ToolCallCtx) -> ToolResult {
        ToolResult::ok(format!("cancelled={}", ctx.cancel.load(Ordering::SeqCst)))
    }

    fn progress_handler(ctx: ToolCallCtx) -> ToolResult {
        if let Some(tx) = ctx.tx_progress.as_ref() {
            tx.try_send(crate::ExecProgressEvent {
                tool_call_id: ctx.id.clone(),
                stream: crate::ExecOutputStream::Stdout,
                seq: 1,
                chunk: "chunk-1".to_owned(),
                bytes_total: 0,
            });
        }
        ToolResult::ok("done")
    }

    fn skill_handler(ctx: ToolCallCtx) -> ToolResult {
        ctx.push_skill_effect(qaqh_skills::SkillEffect::Activate(
            qaqh_skills::SkillActivation {
                metadata: qaqh_skills::SkillMetadata {
                    name: "demo".to_owned(),
                    description: "demo skill".to_owned(),
                    license: None,
                    compatibility: None,
                    metadata: Default::default(),
                    allowed_tools: Vec::new(),
                    path: std::path::PathBuf::from("/tmp/skill"),
                    scope: qaqh_skills::SkillScope::Project,
                },
                body: "body".to_owned(),
                resources: Vec::new(),
            },
        ));
        ToolResult::ok("activated")
    }

    fn metrics_handler(_ctx: ToolCallCtx) -> ToolResult {
        let mut result = ToolResult::ok("with metrics");
        result.metrics.elapsed_ms = Some(42);
        result.metrics.output_bytes = 7;
        result.metrics.retry_count = 2;
        result.metrics.effective_tool_name = Some("read_alias".to_owned());
        result.metrics.user_initiated = true;
        result
    }

    #[test]
    fn partial_and_backgrounded_statuses_keep_invariants() {
        fn partial_handler(_ctx: ToolCallCtx) -> ToolResult {
            ToolResult::partial("stopped early")
        }
        fn backgrounded_handler(_ctx: ToolCallCtx) -> ToolResult {
            ToolResult::backgrounded("running as bg-1")
        }

        let adapter =
            LegacyToolAdapter::new(legacy_handler("exec", partial_handler)).expect("adapter");
        let call = adapter
            .execute_legacy(&test_ctx(None), serde_json::json!({}))
            .expect("execute");
        assert_eq!(call.outcome.status, qaqh_types::ToolStatus::Partial);
        assert_eq!(call.outcome.check_invariants(), Ok(()));
        let error = call.outcome.error.expect("partial 必须带 error");
        assert_eq!(error.code.as_str(), "partial");

        let adapter =
            LegacyToolAdapter::new(legacy_handler("exec", backgrounded_handler)).expect("adapter");
        let call = adapter
            .execute_legacy(&test_ctx(None), serde_json::json!({}))
            .expect("execute");
        assert_eq!(call.outcome.status, qaqh_types::ToolStatus::Backgrounded);
        assert!(call.outcome.error.is_none());
        assert_eq!(call.outcome.check_invariants(), Ok(()));
    }

    #[test]
    fn images_and_data_map_into_outcome() {
        fn image_handler(_ctx: ToolCallCtx) -> ToolResult {
            ToolResult::ok_data(serde_json::json!({"path": "img.png"}), "captured")
                .with_image("image/png", "aGVsbG8=")
        }
        let adapter =
            LegacyToolAdapter::new(legacy_handler("read_image", image_handler)).expect("adapter");
        let call = adapter
            .execute_legacy(&test_ctx(None), serde_json::json!({}))
            .expect("execute");
        assert_eq!(call.outcome.images.len(), 1, "图片附件不得丢失");
        assert_eq!(call.outcome.images[0].mime_type, "image/png");
        match &call.outcome.output {
            ToolOutputValue::Json(data) => assert_eq!(data["path"], "img.png"),
            other => panic!("期望 Json(data)，得到 {other:?}"),
        }
    }

    #[test]
    fn rejects_invalid_legacy_key() {
        let handler = legacy_handler("Bad-Name", echo_handler);
        assert!(matches!(
            LegacyToolAdapter::new(handler),
            Err(DescriptorError::InvalidName { .. })
        ));
    }

    #[test]
    fn zero_timeout_tool_is_rejected_known_issue_q9() {
        // 迁移项 Q9：legacy `ask` 用 `Duration::ZERO` 表示「无超时」，SDK 校验拒绝零超时。
        // 本测试固定当前行为；Q9 决策落地后随实现更新。
        let mut handler = legacy_handler("ask", echo_handler);
        handler.default_timeout = Duration::ZERO;
        assert!(matches!(
            LegacyToolAdapter::new(handler),
            Err(DescriptorError::ZeroTimeout)
        ));
    }

    #[test]
    fn interactive_capability_allows_zero_timeout() {
        let mut handler = legacy_handler("ask", echo_handler);
        handler.default_timeout = Duration::ZERO;
        let capabilities = ToolCapabilities {
            interactive: true,
            ..ToolCapabilities::default()
        };
        let adapter = LegacyToolAdapter::new_with_capabilities(handler, capabilities)
            .expect("interactive tool may use zero default timeout");
        assert_eq!(adapter.descriptor().default_timeout, Duration::ZERO);
    }

    #[test]
    fn descriptor_reflects_legacy_fields_and_validates() {
        let adapter =
            LegacyToolAdapter::new(legacy_handler("read", echo_handler)).expect("adapter");
        let descriptor = adapter.descriptor();
        assert_eq!(descriptor.name.as_str(), "read");
        assert_eq!(descriptor.description, "测试工具");
        assert_eq!(descriptor.category, ToolCategory::Read);
        assert_eq!(descriptor.risk, ToolRisk::ReadOnly);
        assert_eq!(descriptor.default_timeout, Duration::from_secs(30));
        assert_eq!(descriptor.source, ToolSource::Builtin);
        assert_eq!(descriptor.exposure, ToolExposure::Direct);
        assert_eq!(descriptor.validate(), Ok(()));
    }

    #[test]
    fn execute_maps_success_display_and_args() {
        let adapter =
            LegacyToolAdapter::new(legacy_handler("echo", echo_handler)).expect("adapter");
        let call = adapter
            .execute_legacy(&test_ctx(None), serde_json::json!({"text": "hello"}))
            .expect("execute");
        let outcome = call.outcome;
        assert_eq!(outcome.status, qaqh_types::ToolStatus::Ok);
        assert_eq!(outcome.model.text, "hello");
        assert!(!outcome.model.truncated);
        assert_eq!(outcome.display.summary.as_deref(), Some("hello"));
        assert!(outcome.error.is_none());
        assert_eq!(outcome.check_invariants(), Ok(()));
        assert!(call.effects.is_empty());
    }

    #[test]
    fn execute_legacy_installs_explicit_context_for_handler() {
        let adapter =
            LegacyToolAdapter::new(legacy_handler("context", context_handler)).expect("adapter");
        let mut ctx = test_ctx(None);
        ctx.workspace_root = std::path::PathBuf::from("/tmp/qaqh-adapter-context");
        ctx.sandbox_spec =
            crate::tool_api::SandboxSpec::workspace_write(ctx.workspace_root.clone());
        ctx.session_id = "adapter-seed".to_string();
        ctx.mode = AgentMode::Plan;
        ctx.sandbox = SandboxMode::Subagent;
        crate::set_actor_context("/tmp/qaqh-before-adapter", "before-seed");
        crate::runtime::set_mode(2);
        crate::authorization::set_subagent_sandbox(false);

        let outcome = adapter
            .execute_legacy(&ctx, serde_json::json!({}))
            .expect("context probe");
        assert_eq!(
            outcome.outcome.model.text,
            "/tmp/qaqh-adapter-context|adapter-seed|1|true"
        );
        assert_eq!(crate::current_session().as_deref(), Some("before-seed"));
        assert_eq!(crate::current_workspace(), "/tmp/qaqh-before-adapter");
        assert_eq!(crate::runtime::current_mode(), 2);
        assert!(!crate::authorization::is_subagent_sandbox());
        crate::clear_actor_context();
        crate::runtime::set_mode(0);
    }

    #[test]
    fn execute_maps_diff_into_display_plane() {
        let adapter =
            LegacyToolAdapter::new(legacy_handler("write", diff_handler)).expect("adapter");
        let call = adapter
            .execute_legacy(&test_ctx(None), serde_json::json!({}))
            .expect("execute");
        assert_eq!(
            call.outcome.display.diff.as_deref(),
            Some("--- a\n+++ b\n"),
            "diff 只进展示面"
        );
    }

    #[test]
    fn execute_preserves_legacy_error_code_and_kind() {
        let adapter =
            LegacyToolAdapter::new(legacy_handler("read", error_handler)).expect("adapter");
        let call = adapter
            .execute_legacy(&test_ctx(None), serde_json::json!({}))
            .expect("execute");
        assert_eq!(call.outcome.status, qaqh_types::ToolStatus::Error);
        assert_eq!(call.outcome.check_invariants(), Ok(()));
        let error = call.outcome.error.expect("error 必须存在");
        assert_eq!(error.kind, ToolErrorKind::NotFound);
        assert_eq!(error.code.as_str(), "not_found", "legacy code 原样保留");
        assert_eq!(error.detail, "file missing");
        assert_eq!(error.hint.as_deref(), Some("check the path"));
        assert!(!error.retryable);
    }

    #[test]
    fn execute_maps_cancelled_status() {
        let adapter =
            LegacyToolAdapter::new(legacy_handler("read", cancelled_handler)).expect("adapter");
        let call = adapter
            .execute_legacy(&test_ctx(None), serde_json::json!({}))
            .expect("execute");
        assert_eq!(call.outcome.status, qaqh_types::ToolStatus::Cancelled);
        assert_eq!(call.outcome.check_invariants(), Ok(()));
        let error = call.outcome.error.expect("error 必须存在");
        assert_eq!(error.kind, ToolErrorKind::Cancelled);
        assert_eq!(error.code.as_str(), "cancelled");
    }

    #[test]
    fn cancellation_token_is_shared_with_legacy_ctx() {
        let adapter =
            LegacyToolAdapter::new(legacy_handler("read", cancel_probe_handler)).expect("adapter");
        let ctx = test_ctx(None);
        ctx.cancellation.cancel();
        let call = adapter
            .execute_legacy(&ctx, serde_json::json!({}))
            .expect("execute");
        assert_eq!(
            call.outcome.model.text, "cancelled=true",
            "legacy ctx.cancel 必须与 ToolCallContext.cancellation 共享同一信号"
        );
    }

    #[test]
    fn metrics_are_mapped_from_legacy() {
        let adapter =
            LegacyToolAdapter::new(legacy_handler("read", metrics_handler)).expect("adapter");
        let call = adapter
            .execute_legacy(&test_ctx(None), serde_json::json!({}))
            .expect("execute");
        let metrics = call.outcome.metrics;
        assert_eq!(metrics.elapsed, Duration::from_millis(42));
        assert_eq!(metrics.output_bytes, 7);
        assert_eq!(metrics.retry_count, 2);
        assert_eq!(metrics.effective_tool_name.as_deref(), Some("read_alias"));
        assert!(metrics.user_initiated);
    }

    #[test]
    fn execute_legacy_collects_skill_effects() {
        let adapter =
            LegacyToolAdapter::new(legacy_handler("skills", skill_handler)).expect("adapter");
        let call = adapter
            .execute_legacy(&test_ctx(None), serde_json::json!({}))
            .expect("execute");
        assert_eq!(call.effects.len(), 1, "skill effects 经显式字段返回");
        assert!(matches!(call.effects[0], ToolEffect::Skill(_)));
    }

    #[test]
    fn erased_tool_path_returns_outcome_and_drops_effects() {
        let adapter =
            LegacyToolAdapter::new(legacy_handler("skills", skill_handler)).expect("adapter");
        let outcome = adapter
            .execute(test_ctx(None), serde_json::json!({}))
            .expect("execute");
        assert_eq!(outcome.status, qaqh_types::ToolStatus::Ok);
        assert_eq!(outcome.check_invariants(), Ok(()));
    }

    #[test]
    fn progress_frames_are_forwarded_to_sink() {
        let adapter =
            LegacyToolAdapter::new(legacy_handler("exec", progress_handler)).expect("adapter");
        let (sink, rx) = ProgressSink::channel(8);
        let ctx = test_ctx(Some(sink));
        adapter
            .execute_legacy(&ctx, serde_json::json!({}))
            .expect("execute");
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
    fn failure_without_error_is_synthesized_to_keep_invariants() {
        fn bad_handler(_ctx: ToolCallCtx) -> ToolResult {
            let mut result = ToolResult::ok("half done");
            result.status = qaqh_types::ToolStatus::Error; // 违反 legacy 约定：failure 无 error
            result
        }
        let adapter = LegacyToolAdapter::new(legacy_handler("read", bad_handler)).expect("adapter");
        let call = adapter
            .execute_legacy(&test_ctx(None), serde_json::json!({}))
            .expect("execute");
        assert_eq!(call.outcome.check_invariants(), Ok(()));
        let error = call.outcome.error.expect("应合成 error 以维持不变量");
        assert_eq!(error.kind, ToolErrorKind::Execution);
    }

    #[test]
    fn output_ref_takes_priority_over_data() {
        fn externalized_handler(_ctx: ToolCallCtx) -> ToolResult {
            let mut result =
                ToolResult::ok_data(serde_json::json!({"path": "big.log"}), "retained head");
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
            result
        }
        let adapter =
            LegacyToolAdapter::new(legacy_handler("exec", externalized_handler)).expect("adapter");
        let call = adapter
            .execute_legacy(&test_ctx(None), serde_json::json!({}))
            .expect("execute");
        match call.outcome.output {
            ToolOutputValue::ContentRef(reference) => {
                assert_eq!(reference.content_id, "c_1");
            }
            other => panic!("期望 ContentRef，得到 {other:?}"),
        }
    }

    #[test]
    fn flag_shared_after_adapter_execute_is_readable() {
        // 反向共享验证：handler 侧置位后，宿主侧 token 可观察到。
        fn cancel_setter(ctx: ToolCallCtx) -> ToolResult {
            ctx.cancel.store(true, Ordering::SeqCst);
            ToolResult::ok("set")
        }
        let adapter =
            LegacyToolAdapter::new(legacy_handler("exec", cancel_setter)).expect("adapter");
        let ctx = test_ctx(None);
        adapter
            .execute_legacy(&ctx, serde_json::json!({}))
            .expect("execute");
        assert!(
            ctx.cancellation.is_cancelled(),
            "handler 侧置位应能被宿主 token 观察到"
        );
    }
}
