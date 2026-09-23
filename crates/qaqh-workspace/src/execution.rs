//! Authorized tool execution and context-aware admission.

use crate::authorization::{Admission, AuthorizedToolCall, ToolInvocation, admit};
use std::collections::HashSet;
#[cfg(test)]
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crate::permission::PermissionLevel;
use crate::tool_api::{AgentMode, CancellationToken, SandboxMode, ToolCallContext, ToolCallSource};

/// Return type for tool execution with interrupt support.
pub struct ToolExecResult {
    pub content: String,
    pub success: bool,
    pub result: crate::ToolResult,
    pub meta: crate::ToolExecMeta,
    pub code_delta: Option<qaqh_domain::CodeDeltaRecord>,
    pub skill_effects: Vec<crate::ToolEffect>,
}

/// Consume an authorization proof and dispatch the bound handler.
///
/// Legacy adapter: reconstruct an explicit context from the current ambient
/// state, preserving the pre-P2-4d entry point for CLI/tests/older callers.
#[allow(clippy::result_large_err)] // 错误装箱属结构塑形，另立项（闭包返回大 Err）
pub fn execute_authorized(
    call: AuthorizedToolCall,
    progress_tx: Option<crate::ExecProgressSender>,
) -> ToolExecResult {
    let cancellation = CancellationToken::new();
    if crate::is_cancel() {
        cancellation.cancel();
    }
    let ambient = crate::runtime::context();
    let context = ToolCallContext {
        call_id: call.call_id().to_string(),
        session_id: ambient
            .as_ref()
            .map(|ctx| ctx.active_session.clone())
            .unwrap_or_else(|| call.session_id().to_string()),
        workspace_root: {
            let workspace = crate::current_workspace();
            if workspace.is_empty() || workspace == "." {
                call.workspace_root().to_path_buf()
            } else {
                std::path::PathBuf::from(workspace)
            }
        },
        mode: match crate::runtime::current_mode() {
            1 => AgentMode::Plan,
            _ => AgentMode::Code,
        },
        permission_level: PermissionLevel::from_u8(
            ambient.map(|ctx| ctx.permission_level).unwrap_or(0),
        ),
        sandbox: if crate::authorization::is_subagent_sandbox() {
            SandboxMode::Subagent
        } else {
            SandboxMode::Main
        },
        timeout: Duration::ZERO,
        cancellation,
        progress: None,
        source: ToolCallSource::Model,
    };
    execute_authorized_with_context(call, context, progress_tx)
}

/// Consume an authorization proof and dispatch under an explicit runtime
/// context. All session/workspace/mode/cancel/sandbox decisions below use the
/// supplied context; thread-local state is installed only as a compatibility
/// view for legacy handlers.
#[allow(clippy::result_large_err)] // 错误装箱属结构塑形，另立项（闭包返回大 Err）
pub fn execute_authorized_with_context(
    call: AuthorizedToolCall,
    context: ToolCallContext,
    progress_tx: Option<crate::ExecProgressSender>,
) -> ToolExecResult {
    let started = Instant::now();
    let (invocation, authorized_resources, authorized_workspace, grant) = call.into_parts();
    let pre_bind_level = Some(context.permission_level as u8).filter(|level| *level > 0);
    let _scope = crate::runtime::install_tool_call_context(&context);

    // 拒绝路径同样落审计（kind=tool_rejected）：商业审计要求"未执行的
    // 调用"也可追溯，不能只在成功路径记账。
    if invocation.session_id != context.session_id {
        audit_rejected(
            &invocation,
            "rejected",
            "SESSION_MISMATCH",
            None,
            None,
            started,
        );
        return failure(&invocation.tool_name, crate::ToolError::SessionMismatch);
    }

    let context_workspace = crate::permission::resolve_target_path(context.workspace_root.clone());
    if context_workspace != authorized_workspace {
        audit_rejected(
            &invocation,
            "rejected",
            "WORKSPACE_MISMATCH",
            None,
            pre_bind_level,
            started,
        );
        return failure(
            &invocation.tool_name,
            crate::ToolError::ToolSpecific {
                tool: invocation.tool_name.clone(),
                code: "WORKSPACE_MISMATCH".into(),
                message: "workspace mismatch — active workspace changed after authorization".into(),
            },
        );
    }

    let mut current_resources =
        crate::permission::extract_target_paths(&invocation.tool_name, &invocation.args);
    current_resources.sort();
    current_resources.dedup();
    let mut authorized_resources = authorized_resources;
    authorized_resources.sort();
    authorized_resources.dedup();
    if current_resources != authorized_resources {
        audit_rejected(
            &invocation,
            "rejected",
            "RESOURCE_MISMATCH",
            None,
            pre_bind_level,
            started,
        );
        return failure(&invocation.tool_name, crate::ToolError::ResourceMismatch);
    }

    if context.cancellation.is_cancelled() {
        audit_rejected(
            &invocation,
            "rejected",
            "CANCELLED",
            None,
            pre_bind_level,
            started,
        );
        return failure(&invocation.tool_name, crate::ToolError::Cancelled);
    }

    if context.mode == AgentMode::Plan
        && crate::PLAN_BLOCKED.contains(&invocation.tool_name.as_str())
    {
        audit_rejected(
            &invocation,
            "rejected",
            "BLOCKED_BY_MODE",
            Some("PLAN mode"),
            pre_bind_level,
            started,
        );
        return failure(
            &invocation.tool_name,
            crate::ToolError::BlockedByMode {
                mode: "PLAN".into(),
                tool: invocation.tool_name.clone(),
            },
        );
    }

    let ToolInvocation {
        session_id,
        call_id,
        tool_name: name,
        action,
        args,
        category,
    } = invocation;

    let timeout_secs = (!context.timeout.is_zero()).then_some(context.timeout.as_secs());
    let cancel_flag = context.cancellation.shared_flag();
    // Phase 1: prepare while holding the manager lock.
    let prepared = crate::runtime::with_manager(|manager| {
        manager.prepare_req_with_cancel(
            call_id.clone(),
            &name,
            &action,
            args.clone(),
            timeout_secs,
            progress_tx,
            cancel_flag,
        )
    });
    let prepared = match prepared {
        Some(Ok(prepared)) => prepared,
        Some(Err(report)) => {
            // prepare 阶段拒绝（未知工具/白名单外/参数预检失败等）：不进入
            // 执行，但仍必须落审计。
            let audit_invocation = ToolInvocation {
                session_id: session_id.clone(),
                call_id: call_id.clone(),
                tool_name: name.clone(),
                action: action.clone(),
                args: args.clone(),
                category,
            };
            audit_rejected(
                &audit_invocation,
                "rejected",
                "PREPARE_REJECTED",
                Some(&report.content),
                pre_bind_level,
                started,
            );
            let canonical = crate::ToolResult::error(report.content.clone());
            return ToolExecResult {
                content: report.content,
                success: report.success,
                result: canonical,
                meta: report.meta,
                code_delta: None,
                skill_effects: Vec::new(),
            };
        }
        None => {
            let audit_invocation = ToolInvocation {
                session_id: session_id.clone(),
                call_id: call_id.clone(),
                tool_name: name.clone(),
                action: action.clone(),
                args: args.clone(),
                category,
            };
            audit_rejected(
                &audit_invocation,
                "rejected",
                "MANAGER_UNAVAILABLE",
                None,
                pre_bind_level,
                started,
            );
            return failure(&name, crate::ToolError::ManagerUnavailable);
        }
    };

    // Phase 2: execute without holding the manager lock. All tools now run in
    // the daemon actor process; WSL deployment moves the whole daemon instead
    // of routing individual tool calls across an environment boundary.
    let _ = (authorized_workspace, authorized_resources);
    // 审计对象 before 指纹：派发前按与 finalize 同源的 args 口径快照
    // file_state 账本（键一致，命中即 before，未命中为 None）。
    let audit_paths = crate::manager::extract_files_affected(&name, &args);
    let before_hashes: Vec<(String, Option<String>)> = audit_paths
        .iter()
        .map(|path| (path.clone(), crate::file_state::last_hash(path)))
        .collect();

    // P4 A1: write/exec/net 必须先写 durable audit intent；失败不得进入 handler。
    let high_risk = matches!(
        category,
        crate::permission::ToolCategory::Write
            | crate::permission::ToolCategory::Exec
            | crate::permission::ToolCategory::Net
    );
    if high_risk && crate::audit::is_quarantined() {
        return failure(
            &name,
            crate::ToolError::AuditQuarantined {
                message: "audit store is quarantined; high-risk tool dispatch is blocked".into(),
            },
        );
    }
    if high_risk {
        let objects = audit_paths
            .iter()
            .map(|path| crate::audit::v2::AuditObject {
                kind: "file".to_string(),
                path: path.clone(),
                before_sha: before_hashes
                    .iter()
                    .find(|(candidate, _)| candidate == path)
                    .and_then(|(_, hash)| hash.clone()),
                after_sha: None,
            })
            .collect();
        let intent = crate::audit::AuditEntry {
            ts: chrono::Utc::now().to_rfc3339(),
            user: "agent".into(),
            tool: name.clone(),
            action: action.clone(),
            args_hash: crate::audit::hash_args(&args),
            args_bytes: crate::audit::args_size(&args),
            status: "pending".to_string(),
            elapsed_ms: started.elapsed().as_millis() as u64,
            kind: crate::audit::v2::AuditKind::ToolIntent,
            session: session_id.clone(),
            call_id: call_id.clone(),
            category: category.as_str().to_string(),
            permission_level: pre_bind_level,
            error_code: None,
            output_bytes: 0,
            retry_count: 0,
            effective_name: prepared.effective_tool_name.clone(),
            decision: Some(grant.as_str().to_string()),
            decision_reason: None,
            objects,
        };
        if let Err(error) = crate::audit::append_audit_intent(&intent) {
            log::error!("audit: intent barrier failed for {name}: {error}");
            return failure(
                &name,
                crate::ToolError::AuditUnavailable {
                    message: format!("audit intent barrier failed: {error}"),
                },
            );
        }
    }

    let (mut tool_result, skill_effects) = match prepared.executor.clone() {
        crate::manager::PreparedExecutor::Legacy(legacy) => {
            let result = legacy(prepared.ctx.clone());
            let skill_effects = if name == "skills" && result.is_success() {
                prepared.ctx.take_skill_effects()
            } else {
                Vec::new()
            };
            (result, skill_effects)
        }
        crate::manager::PreparedExecutor::Typed(erased) => match erased
            .execute(context.clone(), args.clone())
        {
            Ok(outcome) => {
                let effects = outcome.effects.clone();
                (outcome.to_tool_result(), effects)
            }
            Err(fatal) => {
                log::error!(
                    "typed tool '{}' returned fatal error {}: {}",
                    name,
                    fatal.code,
                    fatal.message
                );
                (
                    crate::ToolResult::error_with("TOOL_FATAL", "internal tool error", false, None),
                    Vec::new(),
                )
            }
        },
    };
    // 工具侧折叠：结果在工具执行层定型（取代 message 侧折叠），
    // 模型看到的、存储的就是最终形态——不再有位置相关的二次改写。
    crate::tool_side_fold::apply(&name, &mut tool_result);
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let success = tool_result.is_success();
    let mut canonical = tool_result.clone();

    // PreparedCall 要整体交给 finalize 清理 inflight；effective name 先取出。
    let effective_tool_name = prepared.effective_tool_name.clone();

    // Phase 3: finalize while holding the manager lock again.
    let report = crate::runtime::with_manager(|manager| {
        manager.finalize_req(prepared, tool_result, elapsed_ms)
    });
    let code_delta = success
        .then(|| crate::code_delta::compute(&name, &args))
        .flatten();

    match report {
        Some(report) => {
            // 运行元数据由执行层统一落盘（H4）：elapsed/output size 只有这里
            // 同时可知；`user_initiated` 由用户直调路径在返回后翻牌。
            canonical.metrics = qaqh_types::ToolResultMetrics {
                elapsed_ms: Some(report.meta.elapsed_ms),
                output_bytes: report.meta.output_size as u64,
                retry_count: 0,
                effective_tool_name,
                user_initiated: false,
            };
            let error_code = canonical.error.as_ref().map(|error| error.code.clone());
            // 对象可追溯：路径 + before/after 内容指纹（file_state LF 规范
            // 视图 hash；未建立基线/新文件为 None）。
            let objects: Vec<crate::audit::v2::AuditObject> = report
                .files_affected
                .iter()
                .map(|path| crate::audit::v2::AuditObject {
                    kind: "file".to_string(),
                    path: path.clone(),
                    before_sha: before_hashes
                        .iter()
                        .find(|(candidate, _)| candidate == path)
                        .and_then(|(_, hash)| hash.clone()),
                    after_sha: crate::file_state::last_hash(path),
                })
                .collect();
            let audit_entry = crate::audit::AuditEntry {
                ts: chrono::Utc::now().to_rfc3339(),
                user: "agent".into(),
                tool: name.clone(),
                action: action.clone(),
                args_hash: crate::audit::hash_args(&args),
                args_bytes: crate::audit::args_size(&args),
                status: crate::audit::status_str(canonical.status).to_string(),
                elapsed_ms: report.meta.elapsed_ms,
                kind: crate::audit::v2::AuditKind::ToolCall,
                session: session_id,
                call_id,
                category: category.as_str().to_string(),
                permission_level: pre_bind_level,
                error_code,
                output_bytes: report.meta.output_size as u64,
                retry_count: canonical.metrics.retry_count,
                effective_name: canonical.metrics.effective_tool_name.clone(),
                decision: Some(grant.as_str().to_string()),
                decision_reason: None,
                objects,
            };
            let result = ToolExecResult {
                content: report.content,
                success: report.success,
                result: canonical,
                meta: report.meta,
                code_delta,
                skill_effects,
            };
            if let Err(e) = crate::audit::append_audit(&audit_entry) {
                log::error!("audit: append failed for {name}: {e}");
                if high_risk {
                    let reason =
                        format!("result audit barrier failed after handler execution: {e}");
                    if let Err(quarantine_error) = crate::audit::quarantine(&audit_entry, &reason) {
                        log::error!(
                            "audit: quarantine sink failed for {name} after result barrier failure: {quarantine_error}"
                        );
                    }
                    return failure(
                        &name,
                        crate::ToolError::AuditQuarantined { message: reason },
                    );
                }
            }
            result
        }
        None => {
            let audit_invocation = ToolInvocation {
                session_id,
                call_id,
                tool_name: name.clone(),
                action: action.clone(),
                args: args.clone(),
                category,
            };
            audit_rejected(
                &audit_invocation,
                "rejected",
                "MANAGER_UNAVAILABLE",
                None,
                pre_bind_level,
                started,
            );
            failure(&name, crate::ToolError::ManagerUnavailable)
        }
    }
}

/// Parse, admit, and execute a call under an explicit [`ToolCtx`](crate::runtime::ToolCtx)
/// (PR-3-2): serve / CLI / tests assemble the context per call instead of
/// pre-mutating ambient state.
pub fn execute_with_context(
    name: &str,
    action: &str,
    args: &str,
    tool_call_id: &str,
    progress_tx: Option<crate::ExecProgressSender>,
    ctx: &crate::runtime::ToolCtx,
) -> ToolExecResult {
    let started = Instant::now();
    // Fail closed：显式上下文缺会话等价于旧“runtime 未初始化”。
    // （无会话 = 无主体身份，拒绝事件无从归属，不落审计。）
    if ctx.session_id.is_empty() {
        return failure(
            &resolve_name(name, action),
            crate::ToolError::RuntimeNotInitialized,
        );
    }
    let call_id = if tool_call_id.is_empty() {
        format!(
            "agent_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        )
    } else {
        tool_call_id.to_string()
    };
    let resolved_name = resolve_name(name, action);
    let args: serde_json::Value = match serde_json::from_str(args) {
        Ok(args) => args,
        Err(error) => {
            let error = crate::ToolError::InvalidArgs {
                message: error.to_string(),
            };
            let audit_invocation = ToolInvocation {
                session_id: ctx.session_id.clone(),
                call_id,
                tool_name: resolved_name.clone(),
                action: action.to_string(),
                args: serde_json::Value::Null,
                category: crate::runtime::lookup_category(&resolved_name)
                    .unwrap_or(crate::permission::ToolCategory::Write),
            };
            audit_rejected(
                &audit_invocation,
                "rejected",
                error.code(),
                Some(&error.to_string()),
                Some(ctx.permission_level),
                started,
            );
            return failure(&resolved_name, error);
        }
    };
    let _ctx_guard = crate::runtime::install_tool_ctx(ctx);

    let resolved_action = if action.is_empty() {
        args.get("action")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(name)
            .to_string()
    } else {
        action.to_string()
    };
    let workspace_root = crate::runtime::active_workspace_root();
    // 能力类别来自 handler 声明（单一事实源）；查不到时保守回退 Write。
    let category = crate::runtime::lookup_category(&resolved_name)
        .unwrap_or(crate::permission::ToolCategory::Write);
    let invocation = ToolInvocation {
        session_id: ctx.session_id.clone(),
        call_id,
        tool_name: resolved_name.clone(),
        action: resolved_action,
        args,
        category,
    };
    // admit 消费 invocation；拒绝路径需要完整授权快照来落审计。
    let audit_invocation = invocation.clone();

    match admit(
        invocation,
        ctx.permission_level,
        &workspace_root,
        &HashSet::new(),
    ) {
        Admission::Authorized(authorized) => execute_authorized(authorized, progress_tx),
        Admission::ApprovalRequired(challenge) => {
            let error = crate::ToolError::PermissionDenied {
                reason: challenge.reason().to_string(),
            };
            audit_rejected(
                &audit_invocation,
                "challenge_required",
                error.code(),
                Some(challenge.reason()),
                Some(ctx.permission_level),
                started,
            );
            failure(&resolved_name, error)
        }
        Admission::Denied(reason) => {
            let error = crate::ToolError::PermissionDenied {
                reason: reason.clone(),
            };
            audit_rejected(
                &audit_invocation,
                "rejected",
                error.code(),
                Some(&reason),
                Some(ctx.permission_level),
                started,
            );
            failure(&resolved_name, error)
        }
    }
}

/// 记录一条"未进入执行"的拒绝事件（v1 CSV + v2 账本双写）。
///
/// 拒绝路径没有 `ToolResult`：错误码/原因由调用方给出，主体/工具面从
/// 授权快照提取。写失败只记日志——审计写入失败不改变工具调用结果，
/// fail-closed 策略属配置层，另行接线。
fn audit_rejected(
    invocation: &ToolInvocation,
    outcome: &str,
    error_code: &str,
    reason: Option<&str>,
    permission_level: Option<u8>,
    started: Instant,
) {
    let entry = crate::audit::AuditEntry {
        ts: chrono::Utc::now().to_rfc3339(),
        user: "agent".into(),
        tool: invocation.tool_name.clone(),
        action: invocation.action.clone(),
        args_hash: crate::audit::hash_args(&invocation.args),
        args_bytes: crate::audit::args_size(&invocation.args),
        status: "error".to_string(),
        elapsed_ms: started.elapsed().as_millis() as u64,
        kind: crate::audit::v2::AuditKind::ToolRejected,
        session: invocation.session_id.clone(),
        call_id: invocation.call_id.clone(),
        category: invocation.category.as_str().to_string(),
        permission_level,
        error_code: Some(error_code.to_string()),
        output_bytes: 0,
        retry_count: 0,
        effective_name: None,
        decision: Some(outcome.to_string()),
        // 原因上界 512 字符：账本可读性与磁盘占用保护（截断标记不必要——
        // 结构上仍是前缀，verify 只校验链）。
        decision_reason: reason.map(|text| text.chars().take(512).collect::<String>()),
        objects: Vec::new(),
    };
    if let Err(e) = crate::audit::append_audit(&entry) {
        log::error!(
            "audit: append rejected event for {} failed: {e}",
            invocation.tool_name
        );
    }
}

fn resolve_name(name: &str, action: &str) -> String {
    if action.is_empty() {
        name.to_string()
    } else {
        format!("{name}_{action}")
    }
}

fn failure(name: &str, error: crate::ToolError) -> ToolExecResult {
    ToolExecResult {
        content: error.to_string(),
        success: false,
        result: error.into_result(),
        meta: crate::ToolExecMeta {
            name: name.to_string(),
            elapsed_ms: 0,
            output_size: 0,
            success: false,
            args_summary: String::new(),
        },
        code_delta: None,
        skill_effects: Vec::new(),
    }
}

// ═══════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authorization::{
        Admission, ApprovalError, AuthorizedToolCall, ToolInvocation, admit,
    };
    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::sync::{MutexGuard, atomic::AtomicU32};
    use std::time::Duration;

    static TEST_HANDLER_COUNT: AtomicU32 = AtomicU32::new(0);
    fn test_counter_handler(_ctx: crate::ToolCallCtx) -> crate::ToolResult {
        TEST_HANDLER_COUNT.fetch_add(1, Ordering::SeqCst);
        crate::ToolResult::ok("counter incremented")
    }

    struct WorkspaceReset;

    impl Drop for WorkspaceReset {
        fn drop(&mut self) {
            crate::set_workspace(".");
        }
    }

    struct ActorContextReset;

    impl Drop for ActorContextReset {
        fn drop(&mut self) {
            crate::clear_actor_context();
            crate::runtime::clear_context();
            crate::runtime::set_mode(0);
            crate::authorization::set_subagent_sandbox(false);
        }
    }

    fn context_probe_handler(_ctx: crate::ToolCallCtx) -> crate::ToolResult {
        crate::ToolResult::ok(
            serde_json::json!({
                "session": crate::current_session(),
                "workspace": crate::current_workspace(),
                "mode": crate::runtime::current_mode(),
                "cancelled": crate::is_cancel(),
            })
            .to_string(),
        )
    }

    fn setup_test_manager() -> MutexGuard<'static, ()> {
        let test_guard = crate::TEST_RUNTIME_SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        crate::set_workspace(".");
        let allowed: Vec<String> = vec![];
        crate::runtime::init_tools("test", &[], allowed);
        crate::runtime::register_test_handler(crate::ToolHandler {
            key: "test_counter".to_string(),
            description: "test handler",
            input_schema: serde_json::json!({}),
            handler: test_counter_handler,
            risk: crate::ToolRisk::ReadOnly,
            category: crate::permission::ToolCategory::Read,
            default_timeout: std::time::Duration::from_secs(5),
        });
        crate::runtime::register_test_handler(crate::ToolHandler {
            key: "test_write".to_string(),
            description: "test write handler",
            input_schema: serde_json::json!({}),
            handler: test_counter_handler,
            risk: crate::ToolRisk::Destructive,
            category: crate::permission::ToolCategory::Write,
            default_timeout: std::time::Duration::from_secs(5),
        });
        crate::runtime::register_test_handler(crate::ToolHandler {
            key: "context_probe".to_string(),
            description: "explicit context probe",
            input_schema: serde_json::json!({}),
            handler: context_probe_handler,
            risk: crate::ToolRisk::ReadOnly,
            category: crate::permission::ToolCategory::Read,
            default_timeout: std::time::Duration::from_secs(5),
        });
        TEST_HANDLER_COUNT.store(0, Ordering::SeqCst);
        test_guard
    }

    #[test]
    fn mcp_dynamic_execution_keeps_upstream_tool_name_in_metrics() {
        let _test_guard = setup_test_manager();
        let (full_name, dynamic_tool) = crate::build_dynamic_tool(
            "demo",
            "echo",
            "test dynamic tool",
            serde_json::json!({"type":"object"}),
            test_counter_handler,
            crate::permission::ToolCategory::Exec,
            Duration::from_secs(30),
        );
        let rejected =
            crate::runtime::replace_dynamic_tools(vec![(full_name.clone(), dynamic_tool)]);
        assert_eq!(rejected, 0, "MCP test projection must register");

        let result = execute_with_context(
            &full_name,
            "",
            r#"{}"#,
            "mcp-metrics-call",
            None,
            &crate::runtime::ToolCtx::admitted("test_session"),
        );

        assert!(result.success, "{}", result.content);
        assert_eq!(result.meta.name, full_name);
        assert_eq!(
            result.result.metrics.effective_tool_name.as_deref(),
            Some("echo"),
            "display metrics must carry the upstream MCP tool name"
        );
    }

    #[test]
    fn skill_execution_returns_typed_activation() {
        let _test_guard = setup_test_manager();
        let definitions = crate::runtime::all_tools();
        let skill_definitions = definitions
            .iter()
            .filter(|definition| definition.function.name == "skills")
            .collect::<Vec<_>>();
        assert_eq!(skill_definitions.len(), 1);
        assert!(
            skill_definitions[0].function.parameters["oneOf"]
                .as_array()
                .is_some_and(|variants| variants.len() == 4)
        );
        assert!(!definitions.iter().any(|definition| matches!(
            definition.function.name.as_str(),
            "skill" | "skill_resource" | "skills_list" | "skill_validate"
        )));
        let temp = tempfile::tempdir().unwrap();
        let skill_dir = temp.path().join(".agents/skills/typed-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: typed-skill\ndescription: Use for typed activation tests.\n---\n\n# Typed instructions",
        ).unwrap();
        std::fs::create_dir_all(skill_dir.join("references")).unwrap();
        std::fs::write(skill_dir.join("references/info.md"), "complete reference").unwrap();
        crate::set_workspace(&temp.path().to_string_lossy());
        crate::runtime::set_context("test_session", 4);

        let result = execute_with_context(
            "skills",
            "",
            r#"{"action":"activate","name":"typed-skill"}"#,
            "skill-call-1",
            None,
            &crate::runtime::ToolCtx::admitted("test_session"),
        );

        assert!(result.success);
        let activation = match result
            .skill_effects
            .into_iter()
            .next()
            .expect("typed activation")
        {
            // ToolEffect / SkillEffect 目前均为单变体 enum，此臂已穷尽；
            // 若未来新增变体，单臂 match 编译失败即强制此处显式处理。
            crate::ToolEffect::Skill(qaqh_skills::SkillEffect::Activate(activation)) => activation,
        };
        assert_eq!(activation.metadata.name, "typed-skill");
        assert!(activation.body.contains("Typed instructions"));

        let resource = execute_with_context(
            "skills",
            "",
            r#"{"action":"resource","name":"typed-skill","path":"references/info.md"}"#,
            "resource-call-1",
            None,
            &crate::runtime::ToolCtx::admitted("test_session"),
        );
        assert!(resource.success);
        assert_eq!(resource.content, "complete reference");
        assert!(resource.skill_effects.is_empty());

        let generic_read = execute_with_context(
            "read",
            "",
            &serde_json::json!({"path": skill_dir.join("SKILL.md")}).to_string(),
            "generic-skill-read",
            None,
            &crate::runtime::ToolCtx::admitted("test_session"),
        );
        assert!(!generic_read.success);
        assert_eq!(
            generic_read
                .result
                .error
                .as_ref()
                .map(|error| error.code.as_str()),
            Some("USE_SKILLS_TOOL")
        );

        let traversal = execute_with_context(
            "skills",
            "",
            r#"{"action":"resource","name":"typed-skill","path":"../outside.md"}"#,
            "resource-call-2",
            None,
            &crate::runtime::ToolCtx::admitted("test_session"),
        );
        assert!(!traversal.success);
        assert_eq!(
            traversal
                .result
                .error
                .as_ref()
                .map(|error| error.code.as_str()),
            Some("SKILL_RESOURCE_UNAVAILABLE")
        );

        let list = execute_with_context(
            "skills",
            "",
            r#"{"action":"list"}"#,
            "skills-list-1",
            None,
            &crate::runtime::ToolCtx::admitted("test_session"),
        );
        assert!(list.success);
        assert!(list.content.contains("typed-skill"));

        let invalid = execute_with_context(
            "skills",
            "",
            r#"{"action":"list","name":"typed-skill"}"#,
            "skills-invalid-1",
            None,
            &crate::runtime::ToolCtx::admitted("test_session"),
        );
        assert!(!invalid.success);
        assert_eq!(
            invalid
                .result
                .error
                .as_ref()
                .map(|error| error.code.as_str()),
            Some("INVALID_ARGUMENTS")
        );
        crate::set_workspace(".");
    }

    fn make_invocation(tool_name: &str, call_id: &str) -> ToolInvocation {
        // 与生产路径（execute_with_context）一致：从 manager lookup handler
        // 声明的 category（单一事实源）；测试 manager 已注册 test_* handler。
        let category = crate::runtime::lookup_category(tool_name)
            .unwrap_or(crate::permission::ToolCategory::Read);
        ToolInvocation {
            session_id: "test_session".to_string(),
            call_id: call_id.to_string(),
            tool_name: tool_name.to_string(),
            action: String::new(),
            args: serde_json::json!({}),
            category,
        }
    }

    // ── Test 1: Auto-approved calls execute normally (Level 4) ──

    #[test]
    fn auto_approved_call_executes_normally() {
        let _test_guard = setup_test_manager();
        crate::runtime::set_context("test_session", 4);
        let inv = make_invocation("test_counter", "call-1");
        let ws = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let trusted = HashSet::new();
        let admission = admit(inv, 4, &ws, &trusted);
        match admission {
            Admission::Authorized(auth) => {
                let result = execute_authorized(auth, None);
                assert!(result.success, "auto-approved call should succeed");
            }
            other => panic!(
                "expected Authorized, got {:?}",
                std::any::type_name_of_val(&other)
            ),
        }
        assert_eq!(TEST_HANDLER_COUNT.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn explicit_context_drives_admission_execution_and_restores_ambient() {
        let _test_guard = setup_test_manager();
        let _actor_reset = ActorContextReset;
        crate::set_actor_context("/tmp/qaqh-legacy-ambient", "legacy-seed");
        crate::runtime::set_mode(1);
        crate::authorization::set_subagent_sandbox(false);

        let workspace = tempfile::tempdir().unwrap();
        let context = crate::tool_api::ToolCallContext {
            call_id: "explicit-context-1".to_string(),
            session_id: "explicit-seed".to_string(),
            workspace_root: workspace.path().to_path_buf(),
            mode: crate::tool_api::AgentMode::Code,
            permission_level: crate::permission::PermissionLevel::Unrestricted,
            sandbox: crate::tool_api::SandboxMode::Main,
            timeout: Duration::ZERO,
            cancellation: crate::tool_api::CancellationToken::new(),
            progress: None,
            source: crate::tool_api::ToolCallSource::Model,
        };
        let authorized = match crate::authorization::authorize_call_with_context(
            "context_probe",
            &serde_json::json!({}),
            &context,
        ) {
            Admission::Authorized(call) => call,
            _ => panic!("explicit context probe must be authorized"),
        };
        let result = execute_authorized_with_context(authorized, context, None);
        assert!(result.success, "{}", result.content);

        let observed: serde_json::Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(observed["session"], "explicit-seed");
        assert_eq!(
            observed["workspace"],
            workspace.path().to_string_lossy().as_ref()
        );
        assert_eq!(observed["mode"], 0);
        assert_eq!(observed["cancelled"], false);
        assert_eq!(
            crate::current_session().as_deref(),
            Some("legacy-seed"),
            "execution must restore the previous ambient context"
        );
    }

    #[test]
    fn explicit_sandbox_controls_admission_without_tls() {
        let _test_guard = setup_test_manager();
        let _actor_reset = ActorContextReset;
        crate::authorization::set_subagent_sandbox(false);
        let workspace = tempfile::tempdir().unwrap();
        let args = serde_json::json!({
            "path": workspace.path().join("inside.txt").to_string_lossy(),
        });

        let main = crate::tool_api::ToolCallContext {
            call_id: "sandbox-main".to_string(),
            session_id: "sandbox-seed".to_string(),
            workspace_root: workspace.path().to_path_buf(),
            mode: crate::tool_api::AgentMode::Code,
            permission_level: crate::permission::PermissionLevel::MaxLockdown,
            sandbox: crate::tool_api::SandboxMode::Main,
            timeout: Duration::ZERO,
            cancellation: crate::tool_api::CancellationToken::new(),
            progress: None,
            source: crate::tool_api::ToolCallSource::Model,
        };
        assert!(matches!(
            crate::authorization::authorize_call_with_context("test_write", &args, &main),
            Admission::ApprovalRequired(_)
        ));

        let subagent = crate::tool_api::ToolCallContext {
            call_id: "sandbox-subagent".to_string(),
            sandbox: crate::tool_api::SandboxMode::Subagent,
            ..main
        };
        match crate::authorization::authorize_call_with_context("test_write", &args, &subagent) {
            Admission::Authorized(call) => {
                assert_eq!(call.grant(), crate::authorization::GrantKind::SandboxAuto)
            }
            other => panic!(
                "subagent sandbox must auto-authorize workspace writes, got {}",
                std::any::type_name_of_val(&other)
            ),
        }
    }

    #[test]
    fn explicit_cancellation_is_checked_before_dispatch() {
        let _test_guard = setup_test_manager();
        TEST_HANDLER_COUNT.store(0, Ordering::SeqCst);
        let cancellation = crate::tool_api::CancellationToken::new();
        cancellation.cancel();
        let context = crate::tool_api::ToolCallContext {
            call_id: "explicit-cancel".to_string(),
            session_id: "test_session".to_string(),
            workspace_root: crate::runtime::active_workspace_root(),
            mode: crate::tool_api::AgentMode::Code,
            permission_level: crate::permission::PermissionLevel::Unrestricted,
            sandbox: crate::tool_api::SandboxMode::Main,
            timeout: Duration::ZERO,
            cancellation,
            progress: None,
            source: crate::tool_api::ToolCallSource::Model,
        };
        let authorized = match crate::authorization::authorize_call_with_context(
            "test_counter",
            &serde_json::json!({}),
            &context,
        ) {
            Admission::Authorized(call) => call,
            _ => panic!("test_counter must be authorized before cancellation check"),
        };
        let result = execute_authorized_with_context(authorized, context, None);
        assert!(!result.success);
        assert_eq!(
            result
                .result
                .error
                .as_ref()
                .map(|error| error.code.as_str()),
            Some("CANCELLED")
        );
        assert_eq!(TEST_HANDLER_COUNT.load(Ordering::SeqCst), 0);
    }

    // ── Test 2: Level 1 (MaxLockdown) requires approval ──

    #[test]
    fn max_lockdown_requires_approval() {
        let _test_guard = setup_test_manager();
        let inv = make_invocation("test_counter", "call-2");
        let ws = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let trusted = HashSet::new();
        let admission = admit(inv, 1, &ws, &trusted);
        assert!(
            matches!(admission, Admission::ApprovalRequired(_)),
            "Level 1 should require approval for all tools"
        );
        assert_eq!(
            TEST_HANDLER_COUNT.load(Ordering::SeqCst),
            0,
            "handler must not execute before approval"
        );
    }

    // ── Test 3: Approval creates a single-use grant ──

    #[test]
    fn approved_call_executes_exactly_once() {
        let _test_guard = setup_test_manager();
        crate::runtime::set_context("test_session", 4);
        let inv = make_invocation("test_counter", "call-3-once");
        let ws = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let trusted = HashSet::new();
        let admission = admit(inv, 1, &ws, &trusted);
        let challenge = match admission {
            Admission::ApprovalRequired(c) => c,
            other => panic!(
                "expected ApprovalRequired, got {:?}",
                std::any::type_name_of_val(&other)
            ),
        };

        let authorized = challenge.approve(true).expect("approval should succeed");
        let result = execute_authorized(authorized, None);
        assert!(result.success, "approved call should execute");
        assert_eq!(
            TEST_HANDLER_COUNT.load(Ordering::SeqCst),
            1,
            "handler should execute exactly once"
        );
    }

    // ── Test 4: Rejected approval does not execute ──

    #[test]
    fn rejected_approval_does_not_execute() {
        let _test_guard = setup_test_manager();
        let inv = make_invocation("test_counter", "call-4");
        let ws = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let trusted = HashSet::new();
        let admission = admit(inv, 1, &ws, &trusted);
        let challenge = match admission {
            Admission::ApprovalRequired(c) => c,
            other => panic!(
                "expected ApprovalRequired, got {:?}",
                std::any::type_name_of_val(&other)
            ),
        };

        let result = challenge.approve(false);
        assert!(matches!(result, Err(ApprovalError::Rejected)));
        assert_eq!(
            TEST_HANDLER_COUNT.load(Ordering::SeqCst),
            0,
            "handler must not execute on rejection"
        );
    }

    // ── Test 5: Expired approval fails (is_expired check) ──

    #[test]
    fn expired_approval_fails() {
        let _test_guard = setup_test_manager();
        let inv = make_invocation("test_counter", "call-5-exp");
        let ws = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let trusted = HashSet::new();

        let admission = admit(inv, 1, &ws, &trusted);
        let challenge = match admission {
            Admission::ApprovalRequired(c) => c,
            _ => panic!("expected ApprovalRequired"),
        };
        assert!(matches!(
            challenge.approve_with_ttl(true, Duration::ZERO),
            Err(ApprovalError::Expired)
        ));
    }

    // ── Test 6: Challenge approve consumes the challenge (no replay) ──

    #[test]
    fn challenge_cannot_be_replayed() {
        let _test_guard = setup_test_manager();
        crate::runtime::set_context("test_session", 4);
        let inv = make_invocation("test_counter", "call-6");
        let ws = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let trusted = HashSet::new();
        let admission = admit(inv, 1, &ws, &trusted);
        let challenge = match admission {
            Admission::ApprovalRequired(c) => c,
            _ => panic!("expected ApprovalRequired"),
        };

        // First approval succeeds and consumes the challenge
        let auth = challenge
            .approve(true)
            .expect("first approval should succeed");
        let _result = execute_authorized(auth, None);
        assert_eq!(TEST_HANDLER_COUNT.load(Ordering::SeqCst), 1);

        // Cannot consume the same challenge twice — it was moved
        // (Rust move semantics guarantee this at compile time)
    }

    // ── Test 7: Different call_id approval fails (mismatch protection) ──

    #[test]
    fn mismatched_call_id_detected_at_loop_level() {
        let _test_guard = setup_test_manager();
        // Create challenge for call-7a
        let inv = make_invocation("test_counter", "call-7a");
        let ws = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let trusted = HashSet::new();
        let admission = admit(inv, 1, &ws, &trusted);
        let challenge = match admission {
            Admission::ApprovalRequired(c) => {
                assert_eq!(c.call_id(), "call-7a");
                c
            }
            _ => panic!("expected ApprovalRequired"),
        };
        // The challenge call_id matches the invocation — the Loop layer
        // enforces that the PermissionResponse call_id matches the pending
        // challenge's call_id via HashMap lookup.
        drop(challenge);
    }

    // ── Test 8: Authorization proof is bound to the call identity ──

    #[test]
    fn authorization_bound_to_call_identity() {
        let _test_guard = setup_test_manager();
        crate::runtime::set_context("test_session", 4);
        let inv1 = make_invocation("test_counter", "bound-1");
        let inv2 = make_invocation("test_counter", "bound-2");
        let ws = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let trusted = HashSet::new();

        let a1 = match admit(inv1, 4, &ws, &trusted) {
            Admission::Authorized(a) => a,
            _other => panic!("expected Authorized"),
        };
        let a2 = match admit(inv2, 4, &ws, &trusted) {
            Admission::Authorized(a) => a,
            _other => panic!("expected Authorized"),
        };

        assert_eq!(a1.call_id(), "bound-1");
        assert_eq!(a2.call_id(), "bound-2");
        assert_ne!(a1.call_id(), a2.call_id());
    }

    // ── Test 9: Compatibility wrapper with context delegates to secured path ──

    #[test]
    fn compat_wrapper_delegates_to_secured_path() {
        let _test_guard = setup_test_manager();
        crate::runtime::set_context("test_session", 4);
        TEST_HANDLER_COUNT.store(0, Ordering::SeqCst);
        // With Level 4 permission context, auto-approve should work
        let result = execute_with_context(
            "test_counter",
            "",
            "{}",
            "compat-2",
            None,
            &crate::runtime::ToolCtx::admitted("test_session"),
        );
        assert!(
            result.success,
            "compat wrapper should succeed with permission context: {}",
            result.content
        );
        assert_eq!(TEST_HANDLER_COUNT.load(Ordering::SeqCst), 1);
    }

    // ── Test 12: Structured success propagates ──

    #[test]
    fn structured_success_propagates_correctly() {
        let _test_guard = setup_test_manager();
        crate::runtime::set_context("test_session", 4);
        TEST_HANDLER_COUNT.store(0, Ordering::SeqCst);

        // auto-approve
        let inv = make_invocation("test_counter", "struc-1");
        let ws = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let trusted = HashSet::new();
        match admit(inv, 4, &ws, &trusted) {
            Admission::Authorized(auth) => {
                let result = execute_authorized(auth, None);
                assert!(result.success, "structured success should be true");
                assert!(
                    !result.content.contains("[ERROR]"),
                    "should not contain error prefix"
                );
            }
            _other => panic!("expected Authorized"),
        }
    }

    // ── Test 13: PLAN mode blocks destructive tools but not reads ──

    #[test]
    fn plan_mode_blocks_destructive_but_not_reads() {
        let _test_guard = setup_test_manager();
        crate::runtime::set_context("test_session", 4);
        let previous_mode = 0;
        crate::runtime::set_mode(1);

        let ws = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let trusted = HashSet::new();

        // `test_write` 不在 PLAN_BLOCKED 名单（`["edit", "exec", "process", "todo"]`，
        // 见 `crate::PLAN_BLOCKED`）中：PLAN 模式下仍放行，阻断只发生在名单内工具。
        //
        // 注意：`test_write` 是 Destructive + Write 的文件型工具，P0-2 之后
        // 「缺 path」会被 `SafetyPolicy` fail-closed 拦下（与 PLAN 模式无关）。
        // 本用例检验的是 PLAN 名单语义，故显式给出工区内 path 让安全闸门放行。
        let mut inv = make_invocation("test_write", "plan-write");
        inv.args = serde_json::json!({ "path": "." });
        if let Admission::Authorized(auth) = admit(inv, 4, &ws, &trusted) {
            let result = execute_authorized(auth, None);
            assert!(
                result.success,
                "test_write not in PLAN_BLOCKED, should succeed even in plan mode: {}",
                result.content
            );
        }

        crate::runtime::set_mode(previous_mode);
    }

    // ── Test 14: Same permission decision for equivalent invocations ──

    #[test]
    fn ui_and_llm_same_permission_decision() {
        let _test_guard = setup_test_manager();
        crate::runtime::set_context("test_session", 2); // ReadFree: reads auto, writes need approval
        let ws = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let trusted = HashSet::new();

        // Both "test_counter" and "test_write" have default ToolCategory::Write.
        // At Level 2 (ReadFree), both require approval.
        for id in &["inv-a", "inv-b"] {
            let inv = make_invocation("test_write", id);
            match admit(inv, 2, &ws, &trusted) {
                Admission::ApprovalRequired(_) => {} // expected for Write at Level 2
                other => panic!(
                    "write tools should require approval at level 2, {:?}",
                    std::any::type_name_of_val(&other)
                ),
            }
        }

        // Same at Level 4 — explicit bypass auto-approves write tools
        for id in &["inv-c", "inv-d"] {
            let inv = make_invocation("test_write", id);
            match admit(inv, 4, &ws, &trusted) {
                Admission::Authorized(_) => {} // expected for bypass mode
                other => panic!(
                    "level 4 bypass should auto-approve write tools, {:?}",
                    std::any::type_name_of_val(&other)
                ),
            }
        }
    }

    // ── Test: Missing context fails closed ──

    #[test]
    fn missing_context_fails_closed() {
        let _test_guard = setup_test_manager();
        TEST_HANDLER_COUNT.store(0, Ordering::SeqCst);
        // PR-3-2：fail-closed 契约迁移到显式 ToolCtx——空 session_id 等价于
        // 旧“runtime context 未初始化”。
        let ctx = crate::runtime::ToolCtx::admitted("");
        let result = execute_with_context("test_counter", "", "{}", "miss-ctx-1", None, &ctx);
        assert!(
            !result.success,
            "should fail closed without runtime context"
        );
        assert_eq!(
            TEST_HANDLER_COUNT.load(Ordering::SeqCst),
            0,
            "handler must never be reached"
        );
    }

    // ── Test: Invalid JSON does not execute ──

    #[test]
    fn invalid_json_does_not_execute() {
        let _test_guard = setup_test_manager();
        crate::runtime::set_context("test", 4);
        TEST_HANDLER_COUNT.store(0, Ordering::SeqCst);
        let result = execute_with_context(
            "test_counter",
            "",
            "not-json{{{",
            "inv-json-1",
            None,
            &crate::runtime::ToolCtx::admitted("test"),
        );
        assert!(!result.success, "invalid JSON should fail");
        assert!(
            result.content.contains("[ERROR]"),
            "should contain error prefix"
        );
        assert_eq!(
            TEST_HANDLER_COUNT.load(Ordering::SeqCst),
            0,
            "handler must never be reached"
        );
    }

    // ── Test: Resources bound in authorization ──

    #[test]
    fn resources_bound_in_authorization() {
        let _test_guard = setup_test_manager();
        let inv = make_invocation("test_counter", "res-bound-1");
        let ws = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let trusted = HashSet::new();
        let admission = admit(inv, 1, &ws, &trusted);
        let challenge = match admission {
            Admission::ApprovalRequired(c) => c,
            _ => panic!("expected ApprovalRequired"),
        };
        let expected_resources = challenge.resources().to_vec();
        let expected_workspace = challenge.workspace_root().to_path_buf();
        let authorized = challenge.approve(true).expect("approval should succeed");
        assert_eq!(
            authorized.resources(),
            expected_resources.as_slice(),
            "resources must be carried through approve()"
        );
        assert_eq!(
            authorized.workspace_root(),
            expected_workspace,
            "workspace root must be carried through approve()"
        );
    }

    #[test]
    fn workspace_change_after_authorization_is_rejected() {
        let _test_guard = setup_test_manager();
        let _workspace_reset = WorkspaceReset;
        crate::runtime::set_context("test_session", 4);
        TEST_HANDLER_COUNT.store(0, Ordering::SeqCst);
        let authorized_workspace = tempfile::tempdir().unwrap();
        let changed_workspace = tempfile::tempdir().unwrap();
        crate::set_workspace(&authorized_workspace.path().to_string_lossy());

        let authorized = match admit(
            make_invocation("test_workspace", "workspace-bound-1"),
            4,
            authorized_workspace.path(),
            &HashSet::new(),
        ) {
            Admission::Authorized(call) => call,
            _ => panic!("expected workspace call to be authorized"),
        };

        crate::set_workspace(&changed_workspace.path().to_string_lossy());
        let result = execute_authorized(authorized, None);
        assert!(!result.success, "changed workspace must invalidate proof");
        assert!(
            result.content.contains("workspace mismatch"),
            "should report workspace mismatch: {}",
            result.content
        );
        assert_eq!(
            TEST_HANDLER_COUNT.load(Ordering::SeqCst),
            0,
            "handler must never be reached"
        );
    }

    // ── Test: Session mismatch rejected ──

    #[test]
    fn session_mismatch_rejected() {
        let _test_guard = setup_test_manager();
        crate::runtime::set_context("session-A", 4);
        TEST_HANDLER_COUNT.store(0, Ordering::SeqCst);
        let inv = ToolInvocation {
            session_id: "session-B".to_string(),
            call_id: "sess-mis-1".to_string(),
            tool_name: "test_counter".to_string(),
            action: String::new(),
            args: serde_json::json!({}),
            category: crate::permission::ToolCategory::Read,
        };
        let auth = AuthorizedToolCall::new(
            inv,
            vec![],
            crate::runtime::active_workspace_root(),
            crate::authorization::GrantKind::Auto,
        );
        let result = execute_authorized(auth, None);
        assert!(!result.success, "session mismatch should be rejected");
        assert!(
            result.content.contains("session mismatch"),
            "should report session mismatch: {}",
            result.content
        );
        assert_eq!(
            TEST_HANDLER_COUNT.load(Ordering::SeqCst),
            0,
            "handler must never be reached"
        );
        // Cleanup runtime context for subsequent tests
        crate::runtime::clear_context();
    }

    // ── Test: Resource mismatch rejected ──

    #[test]
    fn resource_mismatch_rejected() {
        let _test_guard = setup_test_manager();
        crate::runtime::set_context("test", 4);
        TEST_HANDLER_COUNT.store(0, Ordering::SeqCst);

        let ws = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let trusted = HashSet::new();

        // Create invocation with path="a.txt", admit it to get authorized resources
        let inv1 = ToolInvocation {
            session_id: "test".to_string(),
            call_id: "res-mis-1".to_string(),
            tool_name: "test_counter".to_string(),
            action: String::new(),
            args: serde_json::json!({"path": "a.txt"}),
            category: crate::permission::ToolCategory::Read,
        };
        let admission = admit(inv1, 4, &ws, &trusted);
        let auth = match admission {
            Admission::Authorized(a) => a,
            other => panic!(
                "expected Authorized, got {:?}",
                std::any::type_name_of_val(&other)
            ),
        };

        // Forge a call with different path but same authorized resources
        let inv2 = ToolInvocation {
            session_id: "test".to_string(),
            call_id: "res-mis-1".to_string(),
            tool_name: "test_counter".to_string(),
            action: String::new(),
            args: serde_json::json!({"path": "b.txt"}),
            category: crate::permission::ToolCategory::Read,
        };
        let forged_auth = AuthorizedToolCall::new(
            inv2,
            auth.resources().to_vec(),
            auth.workspace_root().to_path_buf(),
            crate::authorization::GrantKind::Auto,
        );
        let result = execute_authorized(forged_auth, None);
        assert!(!result.success, "resource mismatch should be rejected");
        assert!(
            result.content.contains("Resource mismatch"),
            "should report resource mismatch: {}",
            result.content
        );
        assert_eq!(
            TEST_HANDLER_COUNT.load(Ordering::SeqCst),
            0,
            "handler must never be reached"
        );
    }
}
