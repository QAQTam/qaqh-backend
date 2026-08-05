//! ToolEngine: permission admission + tool execution.
//!
//! Owns: pending_approvals, trusted_folders.
//! Handles: UI tool calls (via handle_ui_tool_call) and LLM tool calls
//!          (via admit_batch from TurnEngine).
//!
//! Key design: a single admit() entry point for both UI and LLM paths.
//! The old code had two separate code paths; now they converge here.

use std::collections::{HashMap, VecDeque};

use crate::services::dashboard;
use crate::state::agent::PendingApproval;
use deepx_domain::{AskMode, AskQuestion};

use super::types::*;

fn timeline_tool(
    tool_call_id: &str,
    name: &str,
    state: deepx_domain::TimelineToolState,
    args_json: Option<String>,
    output: Option<String>,
    failure: Option<deepx_domain::TimelineFailure>,
) -> deepx_domain::TimelineTool {
    deepx_domain::TimelineTool {
        tool_call_id: tool_call_id.to_string(),
        name: name.to_string(),
        state,
        summary: output.clone(),
        args_json,
        output,
        progress: String::new(),
        failure,
        permission: None,
    }
}

fn emit_timeline_tool_progress(
    ctx: &mut RingContext,
    turn_id: &str,
    round_num: u32,
    tool_call_id: &str,
    chunk: String,
) {
    ctx.emitter
        .emit_timeline(deepx_domain::TimelineIntent::ToolProgress {
            turn_id: turn_id.to_string(),
            round_num,
            block_id: format!("tool:{tool_call_id}"),
            chunk,
        });
}

pub struct ToolEngine {
    /// Pending permission approvals (keyed by tool_call_id).
    pub(crate) pending: HashMap<String, PendingApproval>,
    /// Persisted trusted folders.
    pub(crate) trusted: deepx_workspace::permission::TrustedFolderSet,
}

impl ToolEngine {
    pub fn new() -> Self {
        Self {
            pending: HashMap::new(),
            trusted: deepx_workspace::permission::TrustedFolderSet::load(""),
        }
    }

    /// Native lifecycle update for a model-originated tool block. The block is
    /// opened by TurnEngine while parsing the assistant response; execution
    /// only changes its mutable state.
    pub fn emit_timeline_tool_running(
        ctx: &mut RingContext,
        turn_id: &str,
        round_num: u32,
        tool_call_id: &str,
        name: &str,
        args: &serde_json::Value,
    ) {
        ctx.emitter
            .emit_timeline(deepx_domain::TimelineIntent::ToolUpdated {
                turn_id: turn_id.to_string(),
                round_num,
                block_id: format!("tool:{tool_call_id}"),
                tool: timeline_tool(
                    tool_call_id,
                    name,
                    deepx_domain::TimelineToolState::Running,
                    Some(args.to_string()),
                    None,
                    None,
                ),
            });
    }

    pub fn emit_timeline_tool_result(
        ctx: &mut RingContext,
        turn_id: &str,
        round_num: u32,
        tool_call_id: &str,
        name: &str,
        args: &str,
        output: &str,
        success: bool,
    ) {
        let failure = (!success).then(|| deepx_domain::TimelineFailure {
            code: "TOOL_EXECUTION_FAILED".into(),
            message: output.to_string(),
        });
        ctx.emitter
            .emit_timeline(deepx_domain::TimelineIntent::ToolUpdated {
                turn_id: turn_id.to_string(),
                round_num,
                block_id: format!("tool:{tool_call_id}"),
                tool: timeline_tool(
                    tool_call_id,
                    name,
                    if success {
                        deepx_domain::TimelineToolState::Succeeded
                    } else {
                        deepx_domain::TimelineToolState::Failed
                    },
                    Some(args.to_string()),
                    Some(output.to_string()),
                    failure,
                ),
            });
    }

    // ═══════════════════════════════════════════════════
    // UI-initiated tool call
    // ═══════════════════════════════════════════════════

    pub fn handle_ui_tool_call(
        &mut self,
        ctx: &mut RingContext,
        id: &str,
        name: &str,
        action: &str,
        args: &serde_json::Value,
    ) {
        let effective_name = crate::util::resolve_effective_name(name, action, args);
        let ws_root = Self::resolve_workspace();

        deepx_workspace::runtime::set_context(
            &ctx.agent.session.seed,
            ctx.agent.config.permission_level,
        );

        let inv = deepx_workspace::authorization::ToolInvocation {
            session_id: ctx.agent.session.seed.clone(),
            call_id: id.to_string(),
            tool_name: effective_name.clone(),
            action: String::new(),
            args: args.clone(),
        };

        match deepx_workspace::authorization::admit(
            inv,
            ctx.agent.config.permission_level,
            &ws_root,
            self.trusted.set(),
        ) {
            deepx_workspace::authorization::Admission::Authorized(authorized) => {
                self.execute_and_emit(ctx, id, &effective_name, args, authorized, false);
            }
            deepx_workspace::authorization::Admission::ApprovalRequired(challenge) => {
                let cat_str = Self::category_str(challenge.category());
                let cat_domain = match challenge.category() {
                    deepx_workspace::permission::ToolCategory::Read => {
                        deepx_domain::PermissionCategory::Read
                    }
                    deepx_workspace::permission::ToolCategory::Write => {
                        deepx_domain::PermissionCategory::Write
                    }
                    deepx_workspace::permission::ToolCategory::Exec => {
                        deepx_domain::PermissionCategory::Exec
                    }
                    deepx_workspace::permission::ToolCategory::Net => {
                        deepx_domain::PermissionCategory::Net
                    }
                };
                let risk_domain = match challenge.risk() {
                    deepx_workspace::permission::PermissionRisk::Low => {
                        deepx_domain::PermissionRisk::Low
                    }
                    deepx_workspace::permission::PermissionRisk::Medium => {
                        deepx_domain::PermissionRisk::Medium
                    }
                    deepx_workspace::permission::PermissionRisk::High => {
                        deepx_domain::PermissionRisk::High
                    }
                };
                let turn_id = format!("tc_{}", challenge.call_id());
                let permission = deepx_domain::TimelineToolPermission {
                    reason: challenge.reason().to_string(),
                    paths: challenge
                        .resources()
                        .iter()
                        .map(|path| path.to_string_lossy().to_string())
                        .collect(),
                    category: cat_str.clone(),
                    level: deepx_workspace::permission::PermissionLevel::from_u8(
                        ctx.agent.config.permission_level,
                    )
                    .to_u8(),
                    risk: match risk_domain {
                        deepx_domain::PermissionRisk::Low => "low",
                        deepx_domain::PermissionRisk::Medium => "medium",
                        deepx_domain::PermissionRisk::High => "high",
                    }
                    .to_string(),
                    consequence: challenge.consequence().to_string(),
                };
                ctx.emitter
                    .emit_timeline(deepx_domain::TimelineIntent::TurnOpened {
                        turn_id: turn_id.clone(),
                        user_text: format!("tool: {name}"),
                    });
                ctx.emitter
                    .emit_timeline(deepx_domain::TimelineIntent::BlockOpened {
                        turn_id,
                        round_num: 0,
                        block_id: format!("tool:{}", challenge.call_id()),
                        kind: deepx_domain::TimelineBlockKind::Tool,
                        tool: Some(deepx_domain::TimelineTool {
                            tool_call_id: challenge.call_id().to_string(),
                            name: challenge.tool_name().to_string(),
                            state: deepx_domain::TimelineToolState::Prepared,
                            summary: None,
                            args_json: Some(args.to_string()),
                            output: None,
                            progress: String::new(),
                            failure: None,
                            permission: Some(permission),
                        }),
                    });
                // Ringing 双发：ToolPermissionRequested（权限请求归 Tool 频道）
                ctx.emitter.emit_domain(deepx_domain::DomainEvent::Tool(
                    deepx_domain::ToolEvent::ToolPermissionRequested {
                        tool_call_id: challenge.call_id().to_string(),
                        turn_id: format!("tc_{}", challenge.call_id()),
                        round_num: 0,
                        tool_name: challenge.tool_name().to_string(),
                        reason: challenge.reason().to_string(),
                        paths: challenge
                            .resources()
                            .iter()
                            .map(|p| p.to_string_lossy().to_string())
                            .collect(),
                        category: cat_domain,
                        level: deepx_workspace::permission::PermissionLevel::from_u8(
                            ctx.agent.config.permission_level,
                        )
                        .to_u8(),
                        risk: risk_domain,
                        consequence: challenge.consequence().to_string(),
                    },
                ));
                self.pending.insert(
                    challenge.call_id().to_string(),
                    PendingApproval {
                        challenge,
                        is_llm_tool: false,
                    },
                );
            }
            deepx_workspace::authorization::Admission::Denied(reason) => {
                let turn_id = format!("tc_{id}");
                Self::emit_timeline_denied(ctx, id, name, &args.to_string(), &reason, false);
                // Ringing 终态统一由 ToolFinished 承载，失败只由 result.status 表达。
                ctx.emitter.emit_domain(deepx_domain::DomainEvent::Tool(
                    deepx_domain::ToolEvent::ToolFinished {
                        tool_call_id: id.to_string(),
                        turn_id: turn_id.clone(),
                        round_num: 0,
                        result: deepx_types::ToolResult::error_with(
                            "TOOL_DENIED",
                            reason.to_string(),
                            false,
                            None,
                        ),
                    },
                ));
            }
        }
    }

    // ═══════════════════════════════════════════════════
    // Permission response handler (called from Loop::dispatch)
    // ═══════════════════════════════════════════════════

    pub fn handle_permission_response(
        &mut self,
        ctx: &mut RingContext,
        tool_call_id: &str,
        approved: bool,
        trust_folder: bool,
    ) -> PermissionDisposition {
        let pending = match self.pending.remove(tool_call_id) {
            Some(p) => p,
            None => {
                log::warn!("[TOOL] unknown permission response: {tool_call_id}");
                return PermissionDisposition::Ignored;
            }
        };

        let call_id = pending.challenge.call_id().to_string();
        let tool_name = pending.challenge.tool_name().to_string();
        let is_llm = pending.is_llm_tool;
        let resources = pending.challenge.resources().to_vec();

        match pending.challenge.approve(approved) {
            Ok(authorized) => {
                if trust_folder {
                    for path in &resources {
                        self.trusted.trust(path.parent().unwrap_or(path));
                    }
                }
                if is_llm {
                    return PermissionDisposition::LlmResolved {
                        call_id: call_id.clone(),
                        admitted: Some(AdmittedTool {
                            call_id,
                            auth: Box::new(authorized),
                        }),
                    };
                } else {
                    // UI tool: emit full result flow
                    let args = authorized.args().clone();
                    self.execute_and_emit(ctx, &call_id, &tool_name, &args, authorized, true);
                }
            }
            Err(deepx_workspace::authorization::ApprovalError::Rejected) => {
                if is_llm {
                    ctx.agent.msg.push_tool_result_direct(
                        &call_id,
                        &format!("[DENIED] '{tool_name}' (user denied permission)"),
                        false,
                    );
                } else {
                    self.emit_denied(ctx, &call_id, &tool_name, "user denied permission");
                }
            }
            Err(deepx_workspace::authorization::ApprovalError::Expired) => {
                if is_llm {
                    ctx.agent.msg.push_tool_result_direct(
                        &call_id,
                        &format!("[EXPIRED] Permission expired for '{tool_name}'."),
                        false,
                    );
                } else {
                    self.emit_denied(ctx, &call_id, &tool_name, "permission expired");
                }
            }
            Err(deepx_workspace::authorization::ApprovalError::MissingOrReplayed) => {
                log::warn!("[TOOL] replayed permission response: {call_id}");
                if is_llm {
                    ctx.agent.msg.push_tool_result_direct(
                        &call_id,
                        &format!(
                            "[EXPIRED] Permission response is no longer valid for '{tool_name}'."
                        ),
                        false,
                    );
                }
            }
        }

        if is_llm {
            PermissionDisposition::LlmResolved {
                call_id,
                admitted: None,
            }
        } else {
            PermissionDisposition::UiHandled
        }
    }

    // ═══════════════════════════════════════════════════
    // Batch admit for LLM tools (called from TurnEngine)
    // ═══════════════════════════════════════════════════

    /// Admit a batch of LLM tool calls.
    /// Denied tools are pushed directly into the message store.
    pub fn admit_batch(
        &mut self,
        ctx: &mut RingContext,
        tools: &[deepx_message::PendingTool],
        turn_id: &str,
        round_num: u32,
    ) -> BatchAdmission {
        let ws_root = Self::resolve_workspace();
        let mut authorized = Vec::new();
        let mut pending_permission_ids = Vec::new();
        let mut pending_asks = VecDeque::new();
        let mut pending_plans = VecDeque::new();
        let mut pending_todo_activation = None;

        for tool in tools {
            let inv = deepx_workspace::authorization::ToolInvocation {
                session_id: ctx.agent.session.seed.clone(),
                call_id: tool.id.clone(),
                tool_name: tool.name.clone(),
                action: String::new(),
                args: tool.args.clone(),
            };
            match deepx_workspace::authorization::admit(
                inv,
                ctx.agent.config.permission_level,
                &ws_root,
                self.trusted.set(),
            ) {
                deepx_workspace::authorization::Admission::Authorized(auth) => {
                    if auth.tool_name() == "ask" {
                        match deepx_workspace::ask_user::normalize_ask_user(auth.args()) {
                            Ok(normalized) => pending_asks.push_back(PendingAsk {
                                call_id: auth.call_id().to_string(),
                                mode: match normalized.mode {
                                    deepx_workspace::ask_user::NormalizedAskMode::Single => {
                                        AskMode::Single
                                    }
                                    deepx_workspace::ask_user::NormalizedAskMode::Batch => {
                                        AskMode::Batch
                                    }
                                },
                                questions: normalized
                                    .questions
                                    .into_iter()
                                    .map(|question| AskQuestion {
                                        id: question.id,
                                        question: question.question,
                                        options: question.options,
                                        allow_custom: question.allow_custom,
                                    })
                                    .collect(),
                            }),
                            Err(error) => ctx.agent.msg.push_tool_result_direct(
                                auth.call_id(),
                                &serde_json::json!({
                                    "status": "error",
                                    "code": error.code,
                                    "message": error.message,
                                })
                                .to_string(),
                                false,
                            ),
                        }
                    } else {
                        authorized.push(AdmittedTool {
                            call_id: tool.id.clone(),
                            auth: Box::new(auth), // Box to reduce enum size
                        });
                    }
                }
                deepx_workspace::authorization::Admission::ApprovalRequired(challenge) => {
                    let cat_str = Self::category_str(challenge.category());
                    let call_id = challenge.call_id().to_string();
                    let risk = match challenge.risk() {
                        deepx_workspace::permission::PermissionRisk::Low => "low",
                        deepx_workspace::permission::PermissionRisk::Medium => "medium",
                        deepx_workspace::permission::PermissionRisk::High => "high",
                    }
                    .to_string();
                    ctx.emitter
                        .emit_timeline(deepx_domain::TimelineIntent::ToolUpdated {
                            turn_id: turn_id.to_string(),
                            round_num,
                            block_id: format!("tool:{call_id}"),
                            tool: deepx_domain::TimelineTool {
                                tool_call_id: call_id.clone(),
                                name: challenge.tool_name().to_string(),
                                state: deepx_domain::TimelineToolState::Prepared,
                                summary: None,
                                args_json: Some(tool.args.to_string()),
                                output: None,
                                progress: String::new(),
                                failure: None,
                                permission: Some(deepx_domain::TimelineToolPermission {
                                    reason: challenge.reason().to_string(),
                                    paths: challenge
                                        .resources()
                                        .iter()
                                        .map(|path| path.to_string_lossy().to_string())
                                        .collect(),
                                    category: cat_str.clone(),
                                    level: deepx_workspace::permission::PermissionLevel::from_u8(
                                        ctx.agent.config.permission_level,
                                    )
                                    .to_u8(),
                                    risk,
                                    consequence: challenge.consequence().to_string(),
                                }),
                            },
                        });
                    // Ringing：LLM 工具轮权限请求（legacy PermissionRequest 的替代，
                    // 与 handle_ui_tool_call 路径一致）。
                    let cat_domain = match challenge.category() {
                        deepx_workspace::permission::ToolCategory::Read => {
                            deepx_domain::PermissionCategory::Read
                        }
                        deepx_workspace::permission::ToolCategory::Write => {
                            deepx_domain::PermissionCategory::Write
                        }
                        deepx_workspace::permission::ToolCategory::Exec => {
                            deepx_domain::PermissionCategory::Exec
                        }
                        deepx_workspace::permission::ToolCategory::Net => {
                            deepx_domain::PermissionCategory::Net
                        }
                    };
                    let risk_domain = match challenge.risk() {
                        deepx_workspace::permission::PermissionRisk::Low => {
                            deepx_domain::PermissionRisk::Low
                        }
                        deepx_workspace::permission::PermissionRisk::Medium => {
                            deepx_domain::PermissionRisk::Medium
                        }
                        deepx_workspace::permission::PermissionRisk::High => {
                            deepx_domain::PermissionRisk::High
                        }
                    };
                    ctx.emitter.emit_domain(deepx_domain::DomainEvent::Tool(
                        deepx_domain::ToolEvent::ToolPermissionRequested {
                            tool_call_id: call_id.clone(),
                            turn_id: turn_id.to_string(),
                            round_num,
                            tool_name: challenge.tool_name().to_string(),
                            reason: challenge.reason().to_string(),
                            paths: challenge
                                .resources()
                                .iter()
                                .map(|path| path.to_string_lossy().to_string())
                                .collect(),
                            category: cat_domain,
                            level: deepx_workspace::permission::PermissionLevel::from_u8(
                                ctx.agent.config.permission_level,
                            )
                            .to_u8(),
                            risk: risk_domain,
                            consequence: challenge.consequence().to_string(),
                        },
                    ));
                    pending_permission_ids.push(call_id.clone());
                    self.pending.insert(
                        call_id,
                        PendingApproval {
                            challenge,
                            is_llm_tool: true,
                        },
                    );
                }
                deepx_workspace::authorization::Admission::Denied(reason) => {
                    ctx.agent.msg.push_tool_result_direct(
                        &tool.id,
                        &format!(
                            "[timeis: {}]\n[DENIED] {}",
                            crate::util::chrono_local_datetime(),
                            reason
                        ),
                        false,
                    );
                }
            }
        }
        BatchAdmission {
            authorized,
            pending_permission_ids,
            pending_asks,
            pending_plans,
            pending_todo_activation,
        }
    }

    // ═══════════════════════════════════════════════════
    // Tool execution (shared by UI and LLM paths)
    // ═══════════════════════════════════════════════════

    /// Execute an authorized tool call and emit full result flow.
    fn execute_and_emit(
        &mut self,
        ctx: &mut RingContext,
        id: &str,
        name: &str,
        args: &serde_json::Value,
        authorized: deepx_workspace::authorization::AuthorizedToolCall,
        approved: bool,
    ) {
        let turn_id = format!("tc_{id}");

        // A newly authorized UI tool owns a complete native turn. A
        // permission-approved tool resumes its stable pending block.
        if !approved {
            ctx.emitter
                .emit_timeline(deepx_domain::TimelineIntent::TurnOpened {
                    turn_id: turn_id.clone(),
                    user_text: format!("tool: {name}"),
                });
            ctx.emitter
                .emit_timeline(deepx_domain::TimelineIntent::BlockOpened {
                    turn_id: turn_id.clone(),
                    round_num: 0,
                    block_id: format!("tool:{id}"),
                    kind: deepx_domain::TimelineBlockKind::Tool,
                    tool: Some(timeline_tool(
                        id,
                        name,
                        deepx_domain::TimelineToolState::Prepared,
                        Some(args.to_string()),
                        None,
                        None,
                    )),
                });
        }
        ctx.emitter
            .emit_timeline(deepx_domain::TimelineIntent::ToolUpdated {
                turn_id: turn_id.clone(),
                round_num: 0,
                block_id: format!("tool:{id}"),
                tool: timeline_tool(
                    id,
                    name,
                    deepx_domain::TimelineToolState::Running,
                    Some(args.to_string()),
                    None,
                    None,
                ),
            });

        // Ringing 双发：权限已通过 = 执行真正开始（决策记录 Q1）
        ctx.emitter.emit_domain(deepx_domain::DomainEvent::Tool(
            deepx_domain::ToolEvent::ToolStarted {
                tool_call_id: id.to_string(),
                turn_id: turn_id.clone(),
                round_num: 0,
                name: name.to_string(),
            },
        ));
        // Ringing 双发：RoundCompleted（工具回合的 initial round 终态）
        ctx.emitter
            .emit_domain(deepx_domain::DomainEvent::Conversation(
                deepx_domain::ConversationEvent::RoundCompleted {
                    turn_id: turn_id.clone(),
                    round_num: 0,
                    thinking: None,
                    answer: None,
                    output_ref: None,
                    is_final: false,
                },
            ));

        // Spawn tool thread
        let (progress_tx, progress_rx) = deepx_workspace::bounded_exec_progress_channel();
        let tool_id = id.to_string();
        let handle = std::thread::Builder::new()
            .stack_size(4 * 1024 * 1024)
            .spawn(move || {
                let result =
                    deepx_workspace::execution::execute_authorized(authorized, Some(progress_tx));
                (
                    tool_id,
                    result.result,
                    result.code_delta,
                    result.skill_effects,
                )
            })
            .expect("failed to spawn tool thread");

        // Drain progress
        self.drain_progress(ctx, progress_rx, &turn_id, 0);

        let (tid, result, code_delta, skill_effects) = handle.join().unwrap_or_else(|_| {
            (
                id.to_string(),
                deepx_types::ToolResult::error("[ERROR] tool thread panicked"),
                None,
                Vec::new(),
            )
        });
        let output = result.model_text().to_string();
        let success = result.is_success();

        ctx.agent.apply_tool_effects(skill_effects);

        // Instant refresh for todo tools
        if matches!(name, "todo") {
            // Ringing 双发：DashboardUpdated（replaceable 覆盖）
            ctx.emitter.emit_domain(deepx_domain::DomainEvent::Control(
                deepx_domain::ControlEvent::DashboardUpdated {
                    hp_connected: true,
                    session_seed: ctx.agent.session.seed.clone(),
                    tool_calls_total: 0,
                    tool_failures: 0,
                    current_phase: "single".into(),
                    streaming: false,
                },
            ));
            ctx.emitter.emit_domain(deepx_domain::DomainEvent::Control(
                deepx_domain::ControlEvent::DashboardSnapshot {
                    snapshot: dashboard::build_snapshot(ctx.agent.session.seed.clone()),
                },
            ));
        }

        if let Some(ref delta) = code_delta {
            ctx.stats.push_delta(delta.clone());
            ctx.emitter.emit_domain(deepx_domain::DomainEvent::Tool(
                deepx_domain::ToolEvent::CodeChanged {
                    lines_added: delta.lines_added,
                    lines_removed: delta.lines_removed,
                    files_created: delta.files_created,
                    files_deleted: delta.files_deleted,
                    file: delta.file.clone(),
                },
            ));
        }

        ctx.emitter.emit_domain(deepx_domain::DomainEvent::Tool(
            deepx_domain::ToolEvent::ToolFinished {
                tool_call_id: tid,
                turn_id: turn_id.clone(),
                round_num: 0,
                result,
            },
        ));
        let terminal_state = if success {
            deepx_domain::TimelineToolState::Succeeded
        } else {
            deepx_domain::TimelineToolState::Failed
        };
        let failure = (!success).then(|| deepx_domain::TimelineFailure {
            code: "TOOL_EXECUTION_FAILED".into(),
            message: output.clone(),
        });
        ctx.emitter
            .emit_timeline(deepx_domain::TimelineIntent::ToolUpdated {
                turn_id: turn_id.clone(),
                round_num: 0,
                block_id: format!("tool:{id}"),
                tool: timeline_tool(
                    id,
                    name,
                    terminal_state,
                    Some(args.to_string()),
                    Some(output.clone()),
                    failure,
                ),
            });
        ctx.emitter
            .emit_timeline(deepx_domain::TimelineIntent::BlockSealed {
                turn_id: turn_id.clone(),
                round_num: 0,
                block_id: format!("tool:{id}"),
            });
        ctx.emitter
            .emit_timeline(deepx_domain::TimelineIntent::RoundSealed {
                turn_id: turn_id.clone(),
                round_num: 0,
                is_final: true,
            });
        ctx.emitter
            .emit_timeline(deepx_domain::TimelineIntent::TurnSealed {
                turn_id: turn_id.clone(),
                state: if success {
                    deepx_domain::TimelineTurnState::Completed
                } else {
                    deepx_domain::TimelineTurnState::Failed
                },
                failure: (!success).then(|| deepx_domain::TimelineFailure {
                    code: "TOOL_EXECUTION_FAILED".into(),
                    message: output.clone(),
                }),
            });
    }

    // ═══════════════════════════════════════════════════
    // Helpers
    // ═══════════════════════════════════════════════════

    /// Drain tool progress from external caller (TurnEngine).
    /// Unlike the internal drain_progress, this takes RingContext directly.
    pub fn drain_progress_external(
        &self,
        ctx: &mut RingContext,
        rx: std::sync::mpsc::Receiver<deepx_workspace::ExecProgressEvent>,
        turn_id: &str,
        round_num: u32,
    ) {
        // A2：渲染尾部协议——尾部状态按 (tool_call_id, stream) 维护，跨事件累积。
        let mut tails: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        loop {
            match rx.recv_timeout(std::time::Duration::from_millis(50)) {
                Ok(first) => {
                    let mut events = vec![first];
                    while let Ok(event) = rx.try_recv() {
                        events.push(event);
                    }
                    for event in events {
                        Self::emit_progress_tail(ctx, turn_id, round_num, &event, &mut tails);
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    }

    fn drain_progress(
        &self,
        ctx: &mut RingContext,
        rx: std::sync::mpsc::Receiver<deepx_workspace::ExecProgressEvent>,
        turn_id: &str,
        round_num: u32,
    ) {
        // A2：渲染尾部协议（与 drain_progress_external 共用发射 helper）。
        let mut tails: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        loop {
            match rx.recv_timeout(std::time::Duration::from_millis(50)) {
                Ok(first) => {
                    let mut events = vec![first];
                    while let Ok(event) = rx.try_recv() {
                        events.push(event);
                    }
                    for event in events {
                        Self::emit_progress_tail(ctx, turn_id, round_num, &event, &mut tails);
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    }

    /// A2：渲染尾部协议——每 (tool_call_id, stream) 只保留最后 4KB 尾部，
    /// 事件携带完整尾部（`seq_start` = 尾部覆盖的起始位置，`chunk` = 尾部全文），
    /// 前端**替换**而非拼接；不连续/丢 chunk 由下一次尾部自愈。
    fn emit_progress_tail(
        ctx: &mut RingContext,
        turn_id: &str,
        round_num: u32,
        event: &deepx_workspace::ExecProgressEvent,
        tails: &mut std::collections::HashMap<String, String>,
    ) {
        const TAIL_MAX: usize = 4096;
        let key = format!("{}:{}", event.tool_call_id, event.stream.as_str());
        let buf = tails.entry(key).or_default();
        buf.push_str(&event.chunk);
        if buf.len() > TAIL_MAX {
            buf.drain(..buf.len() - TAIL_MAX);
        }
        let seq_end = event.seq + event.chunk.len() as u64;
        let tail_len = buf.len() as u64;
        ctx.emitter.emit_domain(deepx_domain::DomainEvent::Tool(
            deepx_domain::ToolEvent::ToolProgress {
                tool_call_id: event.tool_call_id.clone(),
                turn_id: turn_id.to_string(),
                round_num,
                stream: event.stream.as_str().to_string(),
                seq_start: seq_end.saturating_sub(tail_len),
                seq_end,
                chunk: buf.clone(),
                dropped_bytes: 0,
                truncated: seq_end > tail_len,
            },
        ));
        emit_timeline_tool_progress(
            ctx,
            turn_id,
            round_num,
            &event.tool_call_id,
            event.chunk.clone(),
        );
    }

    fn emit_timeline_denied(
        ctx: &mut RingContext,
        call_id: &str,
        tool_name: &str,
        args_json: &str,
        reason: &str,
        already_open: bool,
    ) {
        let turn_id = format!("tc_{call_id}");
        let output = format!("[DENIED] '{tool_name}' ({reason})");
        if !already_open {
            ctx.emitter
                .emit_timeline(deepx_domain::TimelineIntent::TurnOpened {
                    turn_id: turn_id.clone(),
                    user_text: format!("tool: {tool_name}"),
                });
            ctx.emitter
                .emit_timeline(deepx_domain::TimelineIntent::BlockOpened {
                    turn_id: turn_id.clone(),
                    round_num: 0,
                    block_id: format!("tool:{call_id}"),
                    kind: deepx_domain::TimelineBlockKind::Tool,
                    tool: Some(timeline_tool(
                        call_id,
                        tool_name,
                        deepx_domain::TimelineToolState::Prepared,
                        Some(args_json.to_string()),
                        None,
                        None,
                    )),
                });
        }
        Self::emit_timeline_tool_result(
            ctx, &turn_id, 0, call_id, tool_name, args_json, &output, false,
        );
        ctx.emitter
            .emit_timeline(deepx_domain::TimelineIntent::BlockSealed {
                turn_id: turn_id.clone(),
                round_num: 0,
                block_id: format!("tool:{call_id}"),
            });
        ctx.emitter
            .emit_timeline(deepx_domain::TimelineIntent::RoundSealed {
                turn_id: turn_id.clone(),
                round_num: 0,
                is_final: true,
            });
        ctx.emitter
            .emit_timeline(deepx_domain::TimelineIntent::TurnSealed {
                turn_id,
                state: deepx_domain::TimelineTurnState::Failed,
                failure: Some(deepx_domain::TimelineFailure {
                    code: "tool_denied".into(),
                    message: reason.to_string(),
                }),
            });
    }

    fn emit_denied(&self, ctx: &mut RingContext, call_id: &str, tool_name: &str, reason: &str) {
        let turn_id = format!("tc_{call_id}");
        Self::emit_timeline_denied(ctx, call_id, tool_name, "{}", reason, true);
        // Ringing 终态统一由 ToolFinished 承载，失败只由 result.status 表达。
        ctx.emitter.emit_domain(deepx_domain::DomainEvent::Tool(
            deepx_domain::ToolEvent::ToolFinished {
                tool_call_id: call_id.to_string(),
                turn_id: turn_id.clone(),
                round_num: 0,
                result: deepx_types::ToolResult::error_with(
                    "TOOL_DENIED",
                    reason.to_string(),
                    false,
                    None,
                ),
            },
        ));
    }

    fn resolve_workspace() -> std::path::PathBuf {
        let ws = deepx_workspace::CURRENT_WORKSPACE
            .read()
            .expect("CURRENT_WORKSPACE lock")
            .clone();
        if ws.is_empty() || ws == "." {
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
        } else {
            std::path::PathBuf::from(ws)
        }
    }

    fn category_str(cat: &deepx_workspace::permission::ToolCategory) -> String {
        match cat {
            deepx_workspace::permission::ToolCategory::Read => "read",
            deepx_workspace::permission::ToolCategory::Write => "write",
            deepx_workspace::permission::ToolCategory::Exec => "exec",
            deepx_workspace::permission::ToolCategory::Net => "net",
        }
        .to_string()
    }

    pub fn cancel_current(&self) {
        deepx_workspace::runtime::cancel_current_tool();
    }

    pub fn clear_pending(&mut self) {
        self.pending.clear();
        deepx_workspace::runtime::clear_context();
    }
}

// ═══════════════════════════════════════════════════════
// Batch admission and permission response contracts
// ═══════════════════════════════════════════════════════

pub struct BatchAdmission {
    pub authorized: Vec<AdmittedTool>,
    pub pending_permission_ids: Vec<String>,
    pub pending_asks: VecDeque<PendingAsk>,
    pub pending_plans: VecDeque<PendingPlan>,
    pub pending_todo_activation: Option<PendingTodoActivation>,
}

pub enum PermissionDisposition {
    Ignored,
    UiHandled,
    LlmResolved {
        call_id: String,
        admitted: Option<AdmittedTool>,
    },
}
