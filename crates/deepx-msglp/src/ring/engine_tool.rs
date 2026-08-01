//! ToolEngine: permission admission + tool execution.
//!
//! Owns: pending_approvals, trusted_folders.
//! Handles: UI tool calls (via handle_ui_tool_call) and LLM tool calls
//!          (via admit_batch from TurnEngine).
//!
//! Key design: a single admit() entry point for both UI and LLM paths.
//! The old code had two separate code paths; now they converge here.

use std::collections::{HashMap, VecDeque};

use crate::state::agent::PendingApproval;
use crate::services::dashboard;
use deepx_proto::{Agent2Ui, AskMode, AskQuestion};

use super::types::*;

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
                    deepx_workspace::permission::PermissionRisk::Low => deepx_domain::PermissionRisk::Low,
                    deepx_workspace::permission::PermissionRisk::Medium => {
                        deepx_domain::PermissionRisk::Medium
                    }
                    deepx_workspace::permission::PermissionRisk::High => {
                        deepx_domain::PermissionRisk::High
                    }
                };
                ctx.emitter.emit(Agent2Ui::PermissionRequest {
                    tool_call_id: challenge.call_id().to_string(),
                    tool_name: challenge.tool_name().to_string(),
                    reason: challenge.reason().to_string(),
                    paths: challenge
                        .resources()
                        .iter()
                        .map(|p| p.to_string_lossy().to_string())
                        .collect(),
                    category: cat_str,
                    level: deepx_workspace::permission::PermissionLevel::from_u8(
                        ctx.agent.config.permission_level,
                    )
                    .to_u8(),
                    risk: Self::protocol_risk(challenge.risk()),
                    consequence: challenge.consequence().to_string(),
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
                ctx.emitter.emit(Agent2Ui::TurnStart {
                    turn_id: turn_id.clone(),
                    user_text: format!("tool: {name}"),
                });
                ctx.emitter.emit(Agent2Ui::ToolResults {
                    turn_id: turn_id.clone(),
                    round_num: 0,
                    results: vec![deepx_proto::ToolResultDef {
                        tool_call_id: id.to_string(),
                        output: format!("[DENIED] '{name}' — {reason}"),
                        success: false,
                        file: None,
                    }],
                });
                // Ringing 双发：ToolFailed（拒绝是结构化失败终态）
                ctx.emitter.emit_domain(deepx_domain::DomainEvent::Tool(
                    deepx_domain::ToolEvent::ToolFailed {
                        tool_call_id: id.to_string(),
                        turn_id: turn_id.clone(),
                        round_num: 0,
                        error: deepx_domain::DomainError {
                            error_id: format!("tool-denied-{id}"),
                            code: "tool_denied".into(),
                            message: reason.to_string(),
                            retryable: false,
                            dedupe_key: Some(format!("tool:{id}")),
                        },
                    },
                ));
                ctx.emitter.emit(Agent2Ui::TurnEnd {
                    turn_id,
                    stop_reason: None,
                    usage: None,
                });
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
                    if auth.tool_name() == "ask_user" {
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
                    } else if auth.tool_name() == "todo"
                        && auth.args().as_object()
                            .and_then(|obj| obj.get("action"))
                            .and_then(|v| v.as_str())
                            == Some("submit")
                    {
                        match deepx_workspace::todo::load_todo() {
                            Ok(store) if store.items.is_empty() => {
                                ctx.agent.msg.push_tool_result_direct(
                                    auth.call_id(),
                                    "[ERROR] No todo items to submit. Use todo create first.",
                                    false,
                                );
                            }
                            Ok(_store) => pending_plans.push_back(PendingPlan {
                                call_id: auth.call_id().to_string(),
                                content: String::new(),
                            }),
                            Err(error) => ctx.agent.msg.push_tool_result_direct(
                                auth.call_id(),
                                &format!("[ERROR] Cannot read todo: {error}"),
                                false,
                            ),
                        }
                    } else if auth.tool_name() == "todo"
                        && auth.args().as_object()
                            .and_then(|obj| obj.get("action"))
                            .and_then(|v| v.as_str())
                            == Some("activate")
                    {
                        match deepx_workspace::todo::load_todo() {
                            Ok(store) if store.items.is_empty() => {
                                ctx.agent.msg.push_tool_result_direct(
                                    auth.call_id(),
                                    "[ERROR] No todo items to activate. Use todo_create first.",
                                    false,
                                );
                            }
                            Ok(store) => {
                                use deepx_workspace::todo::TodoStatus;
                                let items: Vec<deepx_proto::TodoActivationItem> = store.items.iter()
                                    .filter(|item| matches!(item.status, TodoStatus::Pending | TodoStatus::InProgress))
                                    .map(|item| deepx_proto::TodoActivationItem {
                                        id: item.id.clone(),
                                        title: item.title.clone(),
                                        description: item.description.clone(),
                                        complexity: String::new(),
                                    })
                                    .collect();
                                if items.is_empty() {
                                    ctx.agent.msg.push_tool_result_direct(
                                        auth.call_id(),
                                        "[ERROR] No pending or in-progress todo items to activate.",
                                        false,
                                    );
                                } else {
                                    pending_todo_activation = Some(PendingTodoActivation {
                                        call_id: auth.call_id().to_string(),
                                        items,
                                    });
                                }
                            }
                            Err(error) => ctx.agent.msg.push_tool_result_direct(
                                auth.call_id(),
                                &format!("[ERROR] Cannot read todo: {error}"),
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
                    ctx.emitter.emit(Agent2Ui::PermissionRequest {
                        tool_call_id: call_id.clone(),
                        tool_name: challenge.tool_name().to_string(),
                        reason: challenge.reason().to_string(),
                        paths: challenge
                            .resources()
                            .iter()
                            .map(|p| p.to_string_lossy().to_string())
                            .collect(),
                        category: cat_str,
                        level: deepx_workspace::permission::PermissionLevel::from_u8(
                            ctx.agent.config.permission_level,
                        )
                        .to_u8(),
                        risk: Self::protocol_risk(challenge.risk()),
                        consequence: challenge.consequence().to_string(),
                    });
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
        _approved: bool,
    ) {
        let turn_id = format!("tc_{id}");
        let args_display: String = args
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or(name)
            .chars()
            .take(80)
            .collect();

        ctx.emitter.emit(Agent2Ui::TurnStart {
            turn_id: turn_id.clone(),
            user_text: format!("tool: {name}"),
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
        ctx.emitter.emit(Agent2Ui::RoundComplete {
            turn_id: turn_id.clone(),
            round_num: 0,
            thinking: None,
            answer: None,
            tool_calls: vec![deepx_proto::ToolCallDef {
                id: id.to_string(),
                name: name.to_string(),
                args_display: args_display.clone(),
                args_json: args.to_string(),
            }],
            blocks: vec![deepx_proto::RoundBlock::Tool {
                card: deepx_proto::ToolCallDef {
                    id: id.to_string(),
                    name: name.to_string(),
                    args_display,
                    args_json: args.to_string(),
                },
            }],
            is_final: false,
        });
        // Ringing 双发：RoundCompleted（工具回合的 initial round 终态）
        ctx.emitter.emit_domain(deepx_domain::DomainEvent::Conversation(
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
                    result.content,
                    result.success,
                    result.code_delta,
                    result.skill_effects,
                )
            })
            .expect("failed to spawn tool thread");

        // Drain progress
        self.drain_progress(ctx, progress_rx, &id.to_string());

        let (tid, output, success, code_delta, skill_effects) =
            handle.join().unwrap_or_else(|_| {
                (
                    id.to_string(),
                    "[ERROR] tool thread panicked".into(),
                    false,
                    None,
                    Vec::new(),
                )
            });

        ctx.agent.apply_tool_effects(skill_effects);

        // Instant refresh for todo tools
        if name.starts_with("todo_") {
            ctx.emitter.emit(Agent2Ui::Dashboard {
                hp_connected: true,
                session_seed: ctx.agent.session.seed.clone(),
                context_limit: ctx.agent.config.context_limit,
                tool_calls_total: 0,
                tool_failures: 0,
                current_phase: "single".into(),
                streaming: false,
                dsml_compat_count: ctx.agent.dsml_compat_count,
                documents: dashboard::build_documents(),
                recent_edits: dashboard::build_recent_edits(),
                tasks: dashboard::build_tasks(),
current_todo_id: dashboard::build_current_todo_id(),
                session_title: ctx.agent.session.title.clone(),
                usage: None,
                model: Some(ctx.agent.config.model.clone()),
            });
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
        }

        if let Some(ref delta) = code_delta {
            ctx.stats.push_delta(delta.clone());
            ctx.emitter.emit_delta(Agent2Ui::CodeDelta {
                lines_added: delta.lines_added,
                lines_removed: delta.lines_removed,
                files_created: delta.files_created,
                files_deleted: delta.files_deleted,
                file: delta.file.clone(),
            });
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

        ctx.emitter.emit(Agent2Ui::ToolResults {
            turn_id: turn_id.clone(),
            round_num: 0,
            results: vec![deepx_proto::ToolResultDef {
                tool_call_id: tid.clone(),
                output: output.clone(),
                success,
                file: None,
            }],
        });
        // Ringing 双发：ToolFinished（terminal）
        ctx.emitter.emit_domain(deepx_domain::DomainEvent::Tool(
            deepx_domain::ToolEvent::ToolFinished {
                tool_call_id: tid,
                turn_id: turn_id.clone(),
                round_num: 0,
                result: deepx_domain::ToolResult {
                    success,
                    summary: output.clone(),
                    output_ref: None,
                },
            },
        ));
        ctx.emitter.emit(Agent2Ui::TurnEnd {
            turn_id,
            stop_reason: None,
            usage: None,
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
        default_id: &str,
    ) {
        loop {
            match rx.recv_timeout(std::time::Duration::from_millis(50)) {
                Ok(first) => {
                    let mut events = vec![first];
                    while let Ok(event) = rx.try_recv() {
                        events.push(event);
                    }
                    for event in events {
                        ctx.emitter.emit_delta(Agent2Ui::ExecProgress {
                            tool_call_id: event.tool_call_id.clone(),
                            stream: event.stream.as_str().to_string(),
                            seq: event.seq,
                            chunk: event.chunk.clone(),
                        });
                        // Ringing 双发：ToolProgress（replaceable 增量）
                        ctx.emitter.emit_domain(deepx_domain::DomainEvent::Tool(
                            deepx_domain::ToolEvent::ToolProgress {
                                tool_call_id: event.tool_call_id.clone(),
                                turn_id: default_id.to_string(),
                                round_num: 0,
                                stream: event.stream.as_str().to_string(),
                                seq_start: event.seq,
                                seq_end: event.seq + event.chunk.len() as u64,
                                chunk: event.chunk.clone(),
                                dropped_bytes: 0,
                                truncated: false,
                            },
                        ));
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
        default_id: &str,
    ) {
        loop {
            match rx.recv_timeout(std::time::Duration::from_millis(50)) {
                Ok(first) => {
                    let mut events = vec![first];
                    while let Ok(event) = rx.try_recv() {
                        events.push(event);
                    }
                    for event in events {
                        ctx.emitter.emit_delta(Agent2Ui::ExecProgress {
                            tool_call_id: event.tool_call_id.clone(),
                            stream: event.stream.as_str().to_string(),
                            seq: event.seq,
                            chunk: event.chunk.clone(),
                        });
                        // Ringing 双发：ToolProgress（replaceable 增量）
                        ctx.emitter.emit_domain(deepx_domain::DomainEvent::Tool(
                            deepx_domain::ToolEvent::ToolProgress {
                                tool_call_id: event.tool_call_id.clone(),
                                turn_id: default_id.to_string(),
                                round_num: 0,
                                stream: event.stream.as_str().to_string(),
                                seq_start: event.seq,
                                seq_end: event.seq + event.chunk.len() as u64,
                                chunk: event.chunk.clone(),
                                dropped_bytes: 0,
                                truncated: false,
                            },
                        ));
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    }

    fn emit_denied(&self, ctx: &mut RingContext, call_id: &str, tool_name: &str, reason: &str) {
        let turn_id = format!("tc_{call_id}");
        ctx.emitter.emit(Agent2Ui::TurnStart {
            turn_id: turn_id.clone(),
            user_text: format!("tool: {tool_name}"),
        });
        ctx.emitter.emit(Agent2Ui::ToolResults {
            turn_id: turn_id.clone(),
            round_num: 0,
            results: vec![deepx_proto::ToolResultDef {
                tool_call_id: call_id.to_string(),
                output: format!("[DENIED] '{tool_name}' ({reason})"),
                success: false,
                file: None,
            }],
        });
        // Ringing 双发：ToolFailed（拒绝是结构化失败终态）
        ctx.emitter.emit_domain(deepx_domain::DomainEvent::Tool(
            deepx_domain::ToolEvent::ToolFailed {
                tool_call_id: call_id.to_string(),
                turn_id: turn_id.clone(),
                round_num: 0,
                error: deepx_domain::DomainError {
                    error_id: format!("tool-denied-{call_id}"),
                    code: "tool_denied".into(),
                    message: reason.to_string(),
                    retryable: false,
                    dedupe_key: Some(format!("tool:{call_id}")),
                },
            },
        ));
        ctx.emitter.emit(Agent2Ui::TurnEnd {
            turn_id,
            stop_reason: None,
            usage: None,
        });
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

    fn protocol_risk(risk: deepx_workspace::permission::PermissionRisk) -> deepx_proto::PermissionRisk {
        match risk {
            deepx_workspace::permission::PermissionRisk::Low => deepx_proto::PermissionRisk::Low,
            deepx_workspace::permission::PermissionRisk::Medium => deepx_proto::PermissionRisk::Medium,
            deepx_workspace::permission::PermissionRisk::High => deepx_proto::PermissionRisk::High,
        }
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
