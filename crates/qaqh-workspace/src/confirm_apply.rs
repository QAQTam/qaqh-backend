//! `confirm_apply`：dry-run 后的内存直提。
//!
//! 写工具（apply_patch / write）以 `dry_run=true` 通过验证时
//! 返回 `pending_id`（参数已暂存在 `pending` 注册表）。模型向用户确认后调
//! 本工具：**从注册表取出参数重放执行路径**——模型不需要重新输出 patch /
//! content（消除二次输出）。
//!
//! - `action=apply`：重放 → 落盘（各工具的 expected_hash 校验拦截 dry-run
//!   之后发生的外部改动；内容匹配工具天然防漂移）。
//! - `action=discard`：丢弃 pending，不落盘。
//! - pending 一次性（apply 或 discard 都消费）；过期（30 分钟）或不存在 →
//!   `PENDING_NOT_FOUND_OR_EXPIRED`。

use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{Value, json};

use crate::file_mutate::mutation_error;
use crate::tool_api::{
    ErasedTool, OutputBudget, ToolCallContext, ToolContentBlock, ToolDescriptor, ToolDisplay,
    ToolError, ToolErrorCode, ToolErrorKind, ToolExecutionError, ToolExposure, ToolHeader,
    ToolName, ToolProjection, ToolSource, ToolStatus, TypedTool, TypedToolAdapter,
};

#[derive(Debug, serde::Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfirmApplyArgs {
    pub pending_id: String,
    #[serde(default = "default_confirm_action")]
    pub action: String,
}

fn default_confirm_action() -> String {
    "apply".to_string()
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ConfirmApplyOutput {
    pub pending_id: String,
    pub action: String,
    pub status: String,
    pub applied: bool,
    pub discarded: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip)]
    #[schemars(skip)]
    wire: qaqh_types::ToolResult,
    #[serde(skip)]
    #[schemars(skip)]
    display: ToolDisplay,
}

impl ToolProjection for ConfirmApplyOutput {
    fn status(&self) -> ToolStatus {
        self.wire.status
    }

    fn error(&self) -> Option<ToolError> {
        self.wire.error.as_ref().map(|wire| {
            let mut error = ToolError::new(ToolErrorKind::Execution, wire.message.clone());
            error.code = ToolErrorCode::from_legacy(&wire.code);
            error.retryable = wire.retryable;
            error.hint = wire.hint.clone();
            error
        })
    }

    fn images(&self) -> Vec<qaqh_types::ToolImage> {
        self.wire.images.clone()
    }

    fn model_blocks(&self) -> Vec<ToolContentBlock> {
        vec![ToolContentBlock::Text {
            text: self.wire.model_text().to_string(),
        }]
    }

    fn summary(&self) -> Option<String> {
        let summary = self.wire.summary();
        (!summary.is_empty()).then(|| summary.to_string())
    }

    fn display(&self, _args: &Value) -> ToolDisplay {
        self.display.clone()
    }
}

pub struct ConfirmApplyTool;

impl TypedTool for ConfirmApplyTool {
    type Args = ConfirmApplyArgs;
    type Output = ConfirmApplyOutput;

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: ToolName::new("confirm_apply").expect("valid confirm_apply tool name"),
            display_name: None,
            description:
                "Apply or discard a dry_run pending_id (apply_patch/write). One-shot, 30min TTL."
                    .to_string(),
            input_schema: confirm_apply_schema(),
            output_schema: serde_json::to_value(schemars::schema_for!(ConfirmApplyOutput))
                .expect("confirm_apply output schema"),
            category: crate::permission::ToolCategory::Write,
            risk: crate::ToolRisk::Write,
            default_timeout: std::time::Duration::from_secs(60),
            exposure: ToolExposure::Direct,
            source: ToolSource::Builtin,
            output_budget: OutputBudget::default(),
            capabilities: crate::tool_capabilities::builtin_capabilities("confirm_apply")
                .unwrap_or_default(),
        }
    }

    #[allow(clippy::result_large_err)] // ToolExecutionError is the frozen typed boundary.
    fn run(
        &self,
        ctx: &ToolCallContext,
        args: Self::Args,
    ) -> Result<Self::Output, ToolExecutionError> {
        if args.pending_id.trim().is_empty() {
            return Err(mutation_error(
                "MISSING_PENDING_ID",
                "confirm_apply requires 'pending_id' (returned by a dry_run of apply_patch / write)",
                None,
                json!({
                    "timeis": crate::now_utc8(),
                    "status": "error",
                    "code": "MISSING_PENDING_ID",
                    "message": "confirm_apply requires 'pending_id' (returned by a dry_run of apply_patch / write)",
                }),
            ));
        }
        let pending_id = args.pending_id;
        let action = args.action;
        let Some(pending) = crate::pending::take(&pending_id) else {
            return Err(mutation_error(
                "PENDING_NOT_FOUND_OR_EXPIRED",
                format!("pending {pending_id} not found or expired (30 min window; one-shot)"),
                Some("Re-run the write tool with dry_run=true to get a fresh pending_id."),
                json!({
                    "timeis": crate::now_utc8(),
                    "status": "error",
                    "code": "PENDING_NOT_FOUND_OR_EXPIRED",
                    "pending_id": pending_id,
                }),
            ));
        };

        match action.as_str() {
            "apply" => {
                let tool = pending.tool_name.clone();
                let wire = match tool.as_str() {
                    "write" => {
                        let _ = serde_json::from_value::<crate::file_mutate::WriteArgs>(
                            pending.args.clone(),
                        )
                        .map_err(|error| {
                            mutation_error(
                                "INVALID_PENDING_ARGS",
                                format!("pending {pending_id} holds invalid write args: {error}"),
                                None,
                                json!({}),
                            )
                        })?;
                        TypedToolAdapter::new(crate::file_mutate::WriteTool)
                            .execute(ctx.clone(), pending.args.clone())
                            .map_err(ToolExecutionError::Fatal)?
                            .to_tool_result()
                    }
                    "apply_patch" => {
                        let _ = serde_json::from_value::<crate::apply_patch::ApplyPatchArgs>(
                            pending.args.clone(),
                        )
                        .map_err(|error| {
                            mutation_error(
                                "INVALID_PENDING_ARGS",
                                format!(
                                    "pending {pending_id} holds invalid apply_patch args: {error}"
                                ),
                                None,
                                json!({}),
                            )
                        })?;
                        TypedToolAdapter::new(crate::apply_patch::ApplyPatchTool)
                            .execute(ctx.clone(), pending.args.clone())
                            .map_err(ToolExecutionError::Fatal)?
                            .to_tool_result()
                    }
                    other => {
                        return Err(mutation_error(
                            "UNKNOWN_PENDING_TOOL",
                            format!("pending {pending_id} holds unknown tool '{other}'"),
                            None,
                            json!({
                                "timeis": crate::now_utc8(),
                                "status": "error",
                                "code": "UNKNOWN_PENDING_TOOL",
                                "pending_id": pending_id,
                            }),
                        ));
                    }
                };
                let display = wire
                    .display()
                    .map(crate::tool_api::output::from_wire_display)
                    .unwrap_or_else(|| {
                        ToolDisplay::new(
                            ToolHeader::Other {
                                label: "confirm_apply".to_string(),
                            },
                            crate::tool_api::ToolBody::None,
                        )
                    });
                let result = Some(wire.data.clone());
                let status = if wire.status.is_success() {
                    "ok".to_string()
                } else {
                    "error".to_string()
                };
                Ok(ConfirmApplyOutput {
                    pending_id,
                    action,
                    status,
                    applied: true,
                    discarded: false,
                    tool: Some(tool),
                    result,
                    wire,
                    display,
                })
            }
            "discard" => {
                let wire = crate::ToolResult::ok(format!(
                    "[OK] confirm_apply — pending {pending_id} discarded, no changes written\n"
                ));
                let display = ToolDisplay::new(
                    ToolHeader::Other {
                        label: "confirm_apply".to_string(),
                    },
                    crate::tool_api::ToolBody::None,
                )
                .with_summary(format!("discarded pending {pending_id}"));
                Ok(ConfirmApplyOutput {
                    pending_id,
                    action,
                    status: "ok".to_string(),
                    applied: false,
                    discarded: true,
                    tool: None,
                    result: None,
                    wire,
                    display,
                })
            }
            other => Err(mutation_error(
                "INVALID_ACTION",
                format!("invalid action {other:?} — use \"apply\" or \"discard\""),
                None,
                json!({
                    "timeis": crate::now_utc8(),
                    "status": "error",
                    "code": "INVALID_ACTION",
                }),
            )),
        }
    }
}

fn confirm_apply_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "pending_id": {"type": "string", "description": "Pending ID from dry_run"},
            "action": {"type": "string", "enum": ["apply", "discard"], "default": "apply", "description": "apply or discard"}
        },
        "required": ["pending_id"],
        "additionalProperties": false
    })
}

pub fn register(mgr: &mut crate::ToolManager) {
    mgr.register_typed(ConfirmApplyTool);
}

/// Compatibility entry retained for existing in-process tests.
#[cfg(test)]
fn exec_confirm_apply(args: &Value) -> crate::ToolResult {
    use crate::tool_api::ErasedTool;

    let ctx = crate::file_mutate::ambient_tool_context(
        "confirm-apply-compat",
        std::time::Duration::from_secs(60),
    );
    TypedToolAdapter::new(ConfirmApplyTool)
        .execute(ctx, args.clone())
        .unwrap_or_else(|fatal| panic!("confirm_apply tool fatal: {}", fatal.message))
        .to_tool_result()
}

// ─────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// 写 CURRENT_WORKSPACE 的测试必须串行（全局静态，并行测试会互相踩踏）。
    static WS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn run_confirm(id: &str, action: &str) -> serde_json::Value {
        let result = exec_confirm_apply(&serde_json::json!({
            "pending_id": id,
            "action": action,
        }));
        let data = result.data.clone();
        if data.as_object().is_none_or(|o| o.is_empty()) {
            // 无结构化 data → 尝试解析错误 JSON；失败则按状态兜底。
            let raw = result.model_text();
            let mut v = serde_json::from_str::<serde_json::Value>(raw).unwrap_or_default();
            if v.get("code").is_none() {
                v["status"] =
                    serde_json::json!(if matches!(result.status, crate::ToolStatus::Ok) {
                        "ok"
                    } else {
                        "error"
                    });
                v["raw"] = serde_json::json!(raw);
            }
            v
        } else {
            data
        }
    }

    #[test]
    fn typed_confirm_apply_registration_and_discard() {
        let mut manager = crate::ToolManager::new();
        register(&mut manager);
        assert!(
            manager.builtins["confirm_apply"].legacy.is_none(),
            "confirm_apply still has legacy executor"
        );
        let id = crate::pending::store("write", &serde_json::json!({}));
        let result = exec_confirm_apply(&serde_json::json!({
            "pending_id": id,
            "action": "discard",
        }));
        assert!(result.is_success());
        assert_eq!(result.data["status"], serde_json::json!("ok"));
        assert_eq!(result.data["discarded"], serde_json::json!(true));
        assert!(result.display().is_some());
    }

    #[test]
    fn write_dry_run_then_confirm() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("w.txt"), "old\n").unwrap();
        let ws = dir.path().to_string_lossy().to_string();
        let wpath = format!("{}/w.txt", ws.replace('\\', "/"));

        let dry = crate::file_mutate::exec_write_file(&serde_json::json!({
            "path": wpath,
            "content": "new content\n",
            "dry_run": true,
        }));
        let data = dry.data.clone();
        let pending_id = data["pending_id"].as_str().expect("pending_id").to_string();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("w.txt")).unwrap(),
            "old\n"
        );

        let out = run_confirm(&pending_id, "apply");
        assert_eq!(out["status"], "ok", "confirm write failed: {out}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("w.txt")).unwrap(),
            "new content\n"
        );
    }

    #[test]
    fn apply_patch_dry_run_then_confirm() {
        // 本测试写全局 CURRENT_WORKSPACE：仅靠模块内 WS_LOCK 不足以与
        // 其他模块的并行测试互斥，必须按家规叠加全仓串行锁。
        let _serial = crate::TEST_RUNTIME_SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _guard = WS_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.txt"), "line1\nline2\n").unwrap();
        let ws = dir.path().to_string_lossy().to_string();
        // 引擎的 workspace-bounded 检查要求绝对路径落在 cwd 内：
        // cwd = CURRENT_WORKSPACE（并行测试可能被踩踏）→ 用 cwd 无关的
        // 方式不可行，因此这里直接设置全局并立即 confirm（串行窗口内安全）。
        let apath = format!("{}/a.txt", ws.replace('\\', "/"));
        crate::CURRENT_WORKSPACE
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clone_from(&ws);
        let patch = format!(
            "*** Begin Patch\n*** Update File: {apath}\n@@\n-line2\n+LINE2\n*** End Patch\n"
        );
        let dry = crate::apply_patch::exec_apply_patch(&serde_json::json!({
            "patch": patch,
            "dry_run": true,
        }));
        let data = dry.data.clone();
        let pending_id = data["pending_id"].as_str().expect("pending_id").to_string();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "line1\nline2\n"
        );

        // 并行测试可能覆盖 CURRENT_WORKSPACE（Linux 调度差异放大竞争）：
        // confirm 重放前重新钉住本测试的 workspace。
        crate::CURRENT_WORKSPACE
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clone_from(&ws);
        let out = run_confirm(&pending_id, "apply");
        assert_eq!(out["status"], "ok", "confirm patch failed: {out}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "line1\nLINE2\n"
        );
    }

    #[test]
    fn missing_pending_id_is_error() {
        let out = run_confirm("", "apply");
        assert_eq!(out["code"], "MISSING_PENDING_ID");
    }
}
