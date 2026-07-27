//! InputEngine: user input handler.
//!
//! Receives raw user text, handles auto-session-creation, compliance guard,
//! and routes to TurnEngine for LLM processing.

use deepx_proto::Agent2Ui;

use super::types::*;

pub struct InputEngine;

impl InputEngine {
    pub fn new() -> Self {
        Self
    }

    /// Handle user input. Returns an Outcome telling the Loop whether
    /// to start a turn, yield, or report an error.
    pub fn handle_user_input(&self, ctx: &mut RingContext, text: &str) -> Outcome {
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
            match deepx_tools::todo::load_todo() {
                Ok(store) if store.mode == deepx_tools::todo::TodoMode::Goal => {
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
            if let Ok(mut store) = deepx_tools::todo::load_todo() {
                if store.mode == deepx_tools::todo::TodoMode::Goal {
                    if let Some(ref current_id) = store.current_id.clone() {
                        if let Some(item) = store.items.iter_mut().find(|i| &i.id == current_id) {
                            if item.status == deepx_tools::todo::TodoStatus::InProgress {
                                item.status = deepx_tools::todo::TodoStatus::Pending;
                            }
                        }
                    }
                    store.mode = deepx_tools::todo::TodoMode::Manual;
                    let _ = deepx_tools::todo::save_todo(&store);
                }
            }
            text.to_string()
        };

        ctx.cancel.clear();
        ctx.agent.reset_annotation();
        deepx_tools::CANCEL.store(false, std::sync::atomic::Ordering::SeqCst);

        deepx_tools::runtime::set_context(
            &ctx.agent.session.seed,
            ctx.agent.config.permission_level,
        );

        if ctx.agent.config.compliance_enabled {
            if let Err(reason) = deepx_gate::guard::content_guard(&text) {
                log::info!("[INPUT] compliance blocked: {reason}");
                ctx.emitter.emit(Agent2Ui::Error { message: reason.clone() });
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
            let workspace = deepx_tools::CURRENT_WORKSPACE.read().unwrap_or_else(|e| e.into_inner()).clone();
            let status = ctx.agent.build_skills_status(&workspace);
            ctx.emitter.emit(Agent2Ui::SkillsChanged { status });
        }

        ctx.agent.msg.push_user(&text);
        ctx.agent.msg.flush_meta(&ctx.agent.config.model, &ctx.agent.config.reasoning_effort);

        let turn_id = format!("t{}", ctx.agent.msg.turn_count());
        ctx.emitter.emit(Agent2Ui::TurnStart { turn_id: turn_id.clone(), user_text: text });

        Outcome::ContinueTurn { turn_id, round_num: 0, usage: None }
    }
}
