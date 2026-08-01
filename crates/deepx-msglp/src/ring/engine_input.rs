//! InputEngine: user input handler.
//!
//! Receives raw user text, handles auto-session-creation, compliance guard,
//! and routes to TurnEngine for LLM processing.

use deepx_proto::{Agent2Ui, ImageBlock};

use super::types::*;

pub struct InputEngine;

impl InputEngine {
    pub fn new() -> Self {
        Self
    }

    /// Handle user input. Returns an Outcome telling the Loop whether
    /// to start a turn, yield, or report an error.
    pub fn handle_user_input(
        &self,
        ctx: &mut RingContext,
        text: &str,
        images: Vec<deepx_proto::ImageBlock>,
    ) -> Outcome {
        log::info!("[INPUT] handle_user_input called, text_len={}", text.len());
        // Auto-create session on first input
        if ctx.agent.session.seed.is_empty() {
            log::info!("[INPUT] auto-creating session on first user input");
            crate::state::lifecycle::create_session(ctx.agent);
            ctx.agent.rebind_store();
            ctx.emitter.emit(Agent2Ui::SessionCreated {
                seed: ctx.agent.session.seed.clone(),
            });
        }

        let text = if text == "[DeepX Goal: resume]" {
            match deepx_workspace::todo::load_todo() {
                Ok(store) if store.mode == deepx_workspace::todo::TodoMode::Goal => {
                    if let Some(current_id) = store.current_id {
                        if let Some(item) = store.items.iter().find(|i| i.id == current_id) {
                            format!(
                                "[自动执行计划 / 目标模式]\n\n继续执行 T{}: {}\n{}",
                                item.id, item.title, item.description
                            )
                        } else {
                            "目标模式无法恢复：当前步骤已丢失。".to_string()
                        }
                    } else {
                        "目标模式无法恢复：没有当前步骤。".to_string()
                    }
                }
                Ok(_) => "目标模式无法恢复：当前没有激活的 goal。使用 todo_activate 开始。".to_string(),
                Err(e) => format!("目标模式恢复失败：{e}"),
            }
        } else {
            if let Ok(mut store) = deepx_workspace::todo::load_todo() {
                if store.mode == deepx_workspace::todo::TodoMode::Goal {
                    if let Some(ref current_id) = store.current_id.clone() {
                        if let Some(item) = store.items.iter_mut().find(|i| &i.id == current_id) {
                            if item.status == deepx_workspace::todo::TodoStatus::InProgress {
                                item.status = deepx_workspace::todo::TodoStatus::Pending;
                            }
                        }
                    }
                    store.mode = deepx_workspace::todo::TodoMode::Manual;
                    let _ = deepx_workspace::todo::save_todo(&store);
                }
            }
            text.to_string()
        };

        ctx.cancel.clear();
        ctx.agent.reset_annotation();
        deepx_workspace::CANCEL.store(false, std::sync::atomic::Ordering::SeqCst);

        deepx_workspace::runtime::set_context(
            &ctx.agent.session.seed,
            ctx.agent.config.permission_level,
        );

        if ctx.agent.config.compliance_enabled {
            if let Err(reason) = deepx_gate::guard::content_guard(&text) {
                log::info!("[INPUT] compliance blocked: {reason}");
                ctx.emitter.emit(Agent2Ui::Error { message: reason.clone() });
                // Ringing 双发：OperationFailed（Control 频道错误终态）
                ctx.emitter.emit_domain(deepx_domain::DomainEvent::Control(
                    deepx_domain::ControlEvent::OperationFailed {
                        occurrence_id: format!(
                            "op-failed-{}",
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_millis())
                                .unwrap_or(0),
                        ),
                        scope: deepx_domain::ErrorScope::Control,
                        error: deepx_domain::DomainError {
                            error_id: format!(
                                "compliance-{}",
                                std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.as_millis())
                                    .unwrap_or(0),
                            ),
                            code: "compliance_block".into(),
                            message: reason,
                            retryable: false,
                            dedupe_key: Some("compliance_block".into()),
                        },
                        operation_id: None,
                    },
                ));
                ctx.emitter.emit(Agent2Ui::TurnEnd {
                    turn_id: "blocked".into(),
                    stop_reason: Some("compliance_block".into()),
                    usage: None,
                });
                ctx.emitter.emit(Agent2Ui::Done);
                return Outcome::Handled;
            }
        }

        ctx.agent.activate_explicit_skills(&text);

        {
            let workspace = deepx_workspace::CURRENT_WORKSPACE.read().unwrap_or_else(|e| e.into_inner()).clone();
            let status = ctx.agent.build_skills_status(&workspace);
            ctx.emitter.emit(Agent2Ui::SkillsChanged { status: status.clone() });
            // Ringing 双发：SkillsUpdated（skill 目录/激活状态）
            ctx.emitter.emit_domain(deepx_domain::DomainEvent::Control(
                deepx_domain::ControlEvent::SkillsUpdated {
                    available: status
                        .available
                        .iter()
                        .map(|s| deepx_domain::SkillInfo {
                            name: s.name.clone(),
                            description: s.description.clone(),
                            scope: s.scope.clone(),
                            source: s.source.clone(),
                        })
                        .collect(),
                    active: status.active.clone(),
                    catalog_revision: Some(status.catalog_revision.clone()),
                    operation_revision: Some(status.operation_revision),
                },
            ));
        }

        log::info!("[INPUT] pushing user message to store");
        let turn_id = ctx.agent.msg.allocate_turn_id();
        ctx.agent.msg.push_user(&text);

        // Add image blocks to the user message and register them globally
        // so image_query can look them up by index.
        for img in &images {
            ctx.agent.msg.push_image_to_last_user(&img.mime_type, &img.data);
            deepx_workspace::image_query::store_image(
                &ctx.agent.session.seed,
                &img.mime_type,
                &img.data,
            );
        }
        log::info!("[INPUT] flushing meta");
        ctx.agent.msg.flush_meta(&ctx.agent.config.model, &ctx.agent.config.reasoning_effort);

        log::info!("[INPUT] emitting TurnStart turn_id={} round_num=0", turn_id);
        ctx.emitter.emit(Agent2Ui::TurnStart { turn_id: turn_id.clone(), user_text: text.clone() });
        // Ringing 双发：TurnStarted（权威开始事件）
        ctx.emitter.emit_domain(deepx_domain::DomainEvent::Conversation(
            deepx_domain::ConversationEvent::TurnStarted {
                turn_id: turn_id.clone(),
                user_text: text,
            },
        ));

        Outcome::ContinueTurn { turn_id, round_num: 0, usage: None }
    }
}
