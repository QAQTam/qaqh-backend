//! `AgentRegistry::lifecycle` —— 只借用父模块（`super`）的既有定义与状态，
//! 不在这里新增字段。

use super::*;

impl AgentRegistry {
    pub(crate) fn forget_sessions(&mut self, ids: &[String]) {
        for id in ids {
            self.agent_catalog.remove(id);
            self.residency.remove(id);
            self.outbound_attempts.remove(id);
            self.quota_ledgers.remove(id);
        }
    }

    pub(crate) fn loops_for_tree(
        &self,
        root: &str,
    ) -> Vec<Arc<qaqh_agent_runtime::session_loop::SessionLoop>> {
        self.supervisor
            .descendants_postorder(root)
            .into_iter()
            .chain(std::iter::once(root.to_string()))
            .filter_map(|id| {
                self.instances
                    .get(&id)
                    .map(|instance| Arc::clone(&instance.loop_))
            })
            .collect()
    }

    pub fn close(&mut self, session_id: &str) {
        let descendants = self.supervisor.descendants_postorder(session_id);
        for child in descendants {
            self.finish_for_unload(&child);
            self.supervisor.unlink(&child);
        }

        self.finish_for_unload(session_id);
        self.supervisor.unlink(session_id);
    }

    /// Detach the loop and request cancellation; its turn settles through the owner.
    pub(super) fn finish_for_unload(&mut self, session_id: &str) {
        self.set_agent_residency(session_id, ListedAgentResidency::Unloaded);
        if let Some(instance) = self.instances.remove(session_id) {
            instance.shutdown();
        }
    }

    /// T-1-4：把父会话的取消传播到它派生的全部子 seed（递归覆盖孙代）。
    ///
    /// 直接取消子循环的活动回合，由循环结算工具、交互和回合状态。
    ///
    /// 只对**已在册**的子实例投递命令——绝不 `get_or_spawn`：已取消/已退出
    /// 的子代理不得因为一次取消传播而被重新拉起。
    pub(super) fn cancel_subagent_children(&mut self, parent: &str) {
        let children = self.children_of(parent);
        for child in children {
            let delivered = self
                .instances
                .get(&child)
                .is_some_and(|instance| instance.loop_.cancel_turn(None));
            log::info!(
                "[subagent] parent cancel {parent} propagated to child {child} (delivered={delivered})"
            );
            // 子循环自身分派取消时不经过 registry，孙代在此逐层传播。
            self.cancel_subagent_children(&child);
        }
    }

    /// E: idle-unload loops whose last turn activity is older than `idle_secs`.
    /// The idle clock comes from the loop itself (last turn start/finish);
    /// only a settled (idle) session may go. The close path is the regular
    /// graceful shutdown (cancel + abandon), so the next input resumes from
    /// SQLite via a fresh loop. Returns the seeds that were unloaded.
    pub fn unload_idle_sessions(&mut self, idle_secs: u64) -> Vec<String> {
        if self.shutting_down || idle_secs == 0 {
            return Vec::new();
        }
        let unloadable: Vec<String> = self
            .instances
            .iter()
            .filter_map(|(session_id, instance)| {
                let loop_ = &instance.loop_;
                (loop_.activity_state() == qaqh_ringing_contract::fact::ActivityState::Idle
                    && loop_.idle_secs() >= idle_secs)
                    .then(|| session_id.clone())
            })
            .collect();
        let mut unloaded = Vec::new();
        for session_id in unloadable {
            let idle = self
                .instances
                .get(&session_id)
                .map(|instance| instance.loop_.idle_secs())
                .unwrap_or(0);
            log::info!("[registry] idle unload seed={session_id} idle={idle}s");
            self.close(&session_id);
            unloaded.push(session_id);
        }
        unloaded
    }

    pub fn shutdown_all(&mut self) {
        self.shutting_down = true;
        // Unload descendants before their parent loops.
        let mut roots: Vec<String> = self
            .instances
            .keys()
            .filter(|session_id| {
                self.supervisor
                    .parent_of(session_id)
                    .is_none_or(|parent| !self.instances.contains_key(&parent))
            })
            .cloned()
            .collect();
        roots.sort();
        for session_id in roots {
            self.close(&session_id);
        }
        self.exec_sessions.shutdown();
    }

    /// ADR-0014：回合终态路由——子会话的 inter-agent 触发回合终态时，向父
    /// 会话发 canonical 回传消息。mailbox 持久：父 idle 开新回合、忙则入队
    /// 回合边界合流、冷则先加载。不需要重试线程。
    pub fn route_turn_terminal(
        &mut self,
        notice: qaqh_agent_runtime::session_loop::TurnTerminalNotice,
    ) {
        use qaqh_ringing::ConversationSendMessage;
        use qaqh_ringing_contract::commands::{
            InterAgentEnvelope, SubagentTerminalKind, SubagentTerminalNotification,
        };
        use qaqh_ringing_contract::fact::{InputPurpose, InterAgentDelivery, TurnTerminal};

        // ADR-0015：goal 记账/续跑先于 inter-agent 判定——用户触发的回合也要记账。
        self.route_goal_terminal(&notice);

        let Some(trigger) = notice.trigger else {
            return;
        };
        let Ok(Some(parent)) = self.owners.session_parent(&notice.session_id) else {
            return;
        };
        let Some(child) = self.agent_catalog.get_by_id(&notice.session_id).cloned() else {
            return;
        };
        let Some(parent_meta) = self.agent_catalog.get_by_id(&parent).cloned() else {
            return;
        };

        let answer = self
            .owners
            .db()
            .read_blocking({
                let session_id = notice.session_id.clone();
                let turn_id = notice.turn_id.clone();
                move |conn| qaqh_session::db::view::turn_answer_text(conn, &session_id, &turn_id)
            })
            .unwrap_or_default();

        let (kind, state_tag, exit) = match notice.terminal {
            TurnTerminal::Completed => (SubagentTerminalKind::Completed, "completed", 0),
            TurnTerminal::Cancelled => (SubagentTerminalKind::Cancelled, "cancelled", 0),
            TurnTerminal::Failed => (SubagentTerminalKind::Failed, "error", 1),
        };
        let name = child.role.clone().unwrap_or_else(|| {
            child
                .agent_path
                .as_str()
                .rsplit('/')
                .next()
                .unwrap_or("subagent")
                .to_string()
        });
        // T-1-2：被取消的终态不注入正文（防复活已取消的父会话）。
        let text = if matches!(kind, SubagentTerminalKind::Cancelled) {
            String::new()
        } else {
            let header = if matches!(kind, SubagentTerminalKind::Failed) {
                format!("subagent '{name}' failed (exit={exit})")
            } else {
                format!("subagent '{name}' completed")
            };
            let body = if answer.trim().is_empty() {
                notice.error.clone().unwrap_or_default()
            } else {
                answer
            };
            format!(
                "<qaqh_subagent_result name=\"{name}\" state=\"{state_tag}\" exit=\"{exit}\">\n{header}\n{body}\n</qaqh_subagent_result>"
            )
        };
        let message_id = format!("subagent-result:{}:{}", notice.session_id, notice.turn_id);
        let envelope = InterAgentEnvelope {
            message_id: message_id.clone(),
            root_session_id: child.root_session_id.as_str().to_string(),
            author: child.agent_path.as_str().to_string(),
            recipient: parent_meta.agent_path.as_str().to_string(),
            other_recipients: vec![],
            task_id: None,
            reply_to: Some(trigger.message_id.clone()),
            causation_id: None,
            delivery: InterAgentDelivery::Queue,
            created_at_ms: unix_ms(),
        };
        let command =
            qaqh_ringing::RingingCommand::ConversationSendMessage(ConversationSendMessage {
                text,
                images: vec![],
                attachments: None,
                message_id: Some(message_id),
                input_purpose: InputPurpose::TriggerTurn,
                as_system: false,
                inter_agent: Some(envelope),
                subagent_terminal: Some(SubagentTerminalNotification {
                    child_session_id: notice.session_id.clone(),
                    parent_call_id: trigger.message_id,
                    terminal: kind,
                }),
                widget_context: None,
            });
        let env = qaqh_ringing::RingingWorkerCommandEnvelope::new(
            &parent,
            format!("subagent-terminal:{}", notice.turn_id),
            command,
        );
        if let Err(error) = self.send_ringing(&parent, &env) {
            log::warn!(
                "[subagent] terminal return to parent {parent} failed: {error}; answer recoverable from child {} turn {}",
                notice.session_id,
                notice.turn_id
            );
        } else {
            log::info!(
                "[subagent] terminal return delivered to parent {parent} (child={}, state={state_tag})",
                notice.session_id
            );
        }
    }
}
