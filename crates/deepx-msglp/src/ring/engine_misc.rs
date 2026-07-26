//! MiscEngine: undo, dashboard, mode, notifications.
//!
//! Handles commands that need direct access to `SyncSender<Agent2Ui>`
//! and `NotifyHandle` (not mediated through the Emitter trait).
//!
//! # Undo consistency (cross-engine transaction)
//!
//! Undo is NOT just a message-store operation. It must also ensure
//! TurnEngine and ToolEngine are reset, because they may hold references
//! to the deleted turn (suspended state, pending approvals).
//! The Loop orchestrates this by calling `turn.reset()` and `tool.reset()`
//! BEFORE calling `handle_undo()`.

use std::sync::mpsc;

use deepx_proto::Agent2Ui;

use crate::state::agent::AgentState;
use crate::services::dashboard;
use crate::util;

const INITIAL_LOAD_COUNT: usize = 20;

pub struct MiscEngine;

impl MiscEngine {
    pub fn new() -> Self {
        Self
    }
    pub fn reset(&mut self) {}

    // ── Undo ──
    ///
    /// Caller (Loop) MUST call `turn.reset()` and `tool.reset()` before
    /// calling this method, to ensure cross-engine consistency.
    pub fn handle_undo(
        &self,
        agent: &mut AgentState,
        turn_id: &str,
        tx: &mpsc::SyncSender<Agent2Ui>,
    ) {
        log::info!(
            "[MISC] UndoTurn {turn_id} — turns before: {}",
            agent.msg.turn_count()
        );
        if agent.msg.truncate_before_turn(turn_id) {
            log::info!(
                "[MISC] UndoTurn — truncated, turns after: {}",
                agent.msg.turn_count()
            );
            agent
                .msg
                .snapshot_full(&agent.config.model, &agent.config.reasoning_effort);
            let total = agent.msg.turn_count() as u32;
            let start = total.saturating_sub(INITIAL_LOAD_COUNT as u32) as usize;
            let recent =
                util::build_turns_from_context(agent, Some(start), Some(INITIAL_LOAD_COUNT));
            let _ = tx.send(Agent2Ui::SessionRestored {
                seed: agent.session.seed.clone(),
                turns: recent,
                tokens_used: 0,
                cache_hit_pct: 0.0,
                total_turns: total,
                has_more: start > 0,
            });
        } else {
            log::info!("[MISC] UndoTurn — no changes");
        }
    }

    // ── Dashboard ──

    pub fn emit_dashboard(&self, agent: &AgentState, tx: &mpsc::SyncSender<Agent2Ui>) {
        // Write context stats to disk
        let (
            chat_text,
            thinking,
            tool_calls,
            tool_results,
            tools_schema,
            system_prompt,
            thinking_blocks,
            tool_call_blocks,
        ) = agent.msg.compute_context_stats(Some(&agent.tool_defs));
        let stats = serde_json::json!({
            "chat_text": chat_text, "thinking": thinking,
            "tool_calls": tool_calls, "tool_results": tool_results,
            "tools_schema": tools_schema, "system_prompt": system_prompt,
            "thinking_blocks": thinking_blocks, "tool_call_blocks": tool_call_blocks,
            "messages": 0,
        });
        let stats_dir = deepx_types::platform::sessions_dir().join(&agent.session.seed);
        let _ = std::fs::create_dir_all(&stats_dir);
        let _ = std::fs::write(stats_dir.join("context_stats.json"), stats.to_string());

        let _ = tx.send(Agent2Ui::Dashboard {
            hp_connected: true,
            session_seed: agent.session.seed.clone(),
            context_limit: agent.config.context_limit,
            tool_calls_total: 0,
            tool_failures: 0,
            current_phase: "single".into(),
            streaming: false,
            dsml_compat_count: agent.dsml_compat_count,
            documents: dashboard::build_documents(),
            recent_edits: dashboard::build_recent_edits(),
            tasks: dashboard::build_tasks(),
            session_title: agent.session.title.clone(),
            usage: None,
            model: Some(agent.config.model.clone()),
        });
    }

    // ── Mode ──

    pub fn set_mode(&self, agent: &mut AgentState, mode_str: &str) {
        let m: u8 = match mode_str {
            "plan" => 1,
            "code" => 2,
            _ => 0,
        };
        deepx_tools::runtime::set_mode(m);
        if !agent.session.seed.is_empty() {
            deepx_session::SessionManager::global().persist_mode(&agent.session.seed, m);
        }
        log::info!("[MISC] mode set to {mode_str} (internal={m})");
    }

    // ── After-turn notification ──

    pub fn maybe_notify(
        &self,
        agent: &AgentState,
        notify_tx: &mpsc::Sender<crate::services::notification::NotifyMessage>,
    ) {
        let preview = agent
            .msg
            .turns()
            .last()
            .and_then(|t| t.steps.last())
            .map(|s| {
                s.assistant
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        deepx_types::ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
        if !preview.is_empty() {
            let words: Vec<_> = preview.split_whitespace().take(20).collect();
            let body = if preview.split_whitespace().count() > 20 {
                format!("{}...", words.join(" "))
            } else {
                words.join(" ")
            };
            let _ = notify_tx.send(crate::services::notification::NotifyMessage::Toast(body));
        }
    }
}
