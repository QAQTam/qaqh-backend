//! `AgentRegistry::messaging` —— 只借用父模块（`super`）的既有定义与状态，
//! 不在这里新增字段。

use super::*;

use crate::error::{QaqhError, QaqhResult};

impl AgentRegistry {
    /// 发送 Ringing worker 命令帧（携带 `wire` 判别字段；worker reader 按 wire 解析）。
    pub fn send_ringing(
        &mut self,
        session_id: &str,
        env: &qaqh_ringing::RingingWorkerCommandEnvelope,
    ) -> QaqhResult<()> {
        if let Some(metadata) = self.agent_catalog.get_by_id(session_id)
            && !metadata.agent_path.is_root()
            && matches!(
                &env.command,
                qaqh_ringing::RingingCommand::Conversation(
                    qaqh_domain::ConversationCommand::ConversationSendMessage {
                        inter_agent: None,
                        ..
                    }
                )
            )
        {
            return Err(format!(
                "parent-owned agent {} rejects direct ConversationSendMessage without inter-agent metadata",
                metadata.agent_path
            ).into());
        }
        self.ensure_loaded_for_command(session_id)?;
        // Resolve reload parameters before the write closure so the closure
        // only borrows the instance, never the whole registry.
        let reload_spec = match &env.command {
            qaqh_ringing::RingingCommand::Control(
                qaqh_domain::ControlCommand::AgentReloadConfig,
            ) => crate::agent::session_config::session_effective_config(
                Some(&self.sessions),
                session_id,
            )
            .map(|(config, _)| crate::session_agent::assembly::provider_spec(&config, Vec::new())),
            _ => None,
        };
        let write = |instance: &AgentInstance| -> QaqhResult<()> {
            match &instance.transport {
                AgentTransport::ThinAgent { handle, .. } => match &env.command {
                    qaqh_ringing::RingingCommand::Conversation(
                        qaqh_domain::ConversationCommand::ConversationSendMessage {
                            text,
                            attachments,
                            ..
                        },
                    ) => {
                        let att_refs: Vec<qaqh_session::session_fact_v2::ContentRef> = attachments
                            .as_ref()
                            .map(|list| {
                                list.iter()
                                    .map(|r| {
                                        qaqh_session::session_fact_v2::ContentRef::new(
                                            qaqh_session::session_fact_v2::ContentHash::new(
                                                r.content_id.clone(),
                                            ),
                                        )
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                        handle
                            .submit_user_input(text, att_refs, Some(env.command_id.clone()))
                            .map(|_| ())
                            .map_err(|e| format!("thin agent submit input error: {e:?}").into())
                    }
                    qaqh_ringing::RingingCommand::Conversation(
                        qaqh_domain::ConversationCommand::ConversationCancel { .. },
                    ) => {
                        let target = match &env.command {
                            qaqh_ringing::RingingCommand::Conversation(
                                qaqh_domain::ConversationCommand::ConversationCancel { turn_id },
                            ) => turn_id.as_ref().map(|turn| {
                                qaqh_session::session_fact_v2::TurnId::new(turn.clone())
                            }),
                            _ => None,
                        };
                        qaqh_workspace::set_session_cancel(session_id, true);
                        handle
                            .cancel(target, "user cancel")
                            .map_err(|e| format!("thin agent cancel error: {e:?}").into())
                    }
                    qaqh_ringing::RingingCommand::Control(
                        qaqh_domain::ControlCommand::InteractionAskRespond {
                            interaction_id,
                            answers,
                        },
                    ) => {
                        let ans: Vec<(String, String)> = answers
                            .iter()
                            .map(|a| (a.question_id.clone(), a.answer.clone()))
                            .collect();
                        handle
                            .resolve_interaction(
                                qaqh_session::session_fact_v2::InteractionId::new(interaction_id),
                                ans,
                            )
                            .map_err(|e| format!("thin agent interaction error: {e:?}").into())
                    }
                    qaqh_ringing::RingingCommand::Tool(
                        qaqh_domain::ToolCommand::ToolPermissionRespond {
                            tool_call_id,
                            approved,
                            trust_folder,
                        },
                    ) => handle
                        .respond_tool_permission(
                            qaqh_session::session_fact_v2::ToolCallId::new(tool_call_id),
                            *approved,
                            *trust_folder,
                        )
                        .map_err(|e| format!("thin agent permission error: {e:?}").into()),
                    qaqh_ringing::RingingCommand::Control(
                        qaqh_domain::ControlCommand::SessionShutdown,
                    ) => handle
                        .shutdown()
                        .map_err(|e| format!("thin agent shutdown error: {e:?}").into()),
                    qaqh_ringing::RingingCommand::Control(
                        qaqh_domain::ControlCommand::AgentReloadConfig,
                    ) => {
                        let provider = reload_spec
                            .clone()
                            .ok_or_else(|| format!("reload config unavailable for {session_id}"))?;
                        handle
                            .reload_provider(provider)
                            .map_err(|e| format!("thin agent reload error: {e:?}").into())
                    }
                    other => Err(format!(
                        "thin agent for {session_id} does not implement command {}",
                        ringing_command_name(other)
                    )
                    .into()),
                },
            }
        };
        if write(self.instances.get(session_id).expect("spawned instance")).is_ok() {
            // T-1-4：父会话取消传播到它派生的子 seed。放在投递成功之后——
            // 取消帧确实进入了父 worker 才谈得上「取消已经发生」。registry
            // 是 Ringing 命令的唯一咽喉（daemon RPC、宿主直连、广播都经此），
            // 因此这里也是唯一能同时触达 hub（mark_worker_dead）与子实例
            // cmd_tx 的位置。
            if matches!(
                &env.command,
                qaqh_ringing::RingingCommand::Conversation(
                    qaqh_domain::ConversationCommand::ConversationCancel { .. }
                )
            ) {
                self.cancel_subagent_children(session_id);
            }
            return Ok(());
        }
        let kind = self
            .instances
            .get(session_id)
            .map(AgentInstance::kind_name)
            .unwrap_or(AgentKind::Session);
        let parent = self.supervisor.parent_of(session_id);
        let parent_cancel = parent
            .as_ref()
            .and_then(|parent| self.cancel_for_session(parent));
        self.close(session_id);
        match kind {
            AgentKind::Session => self.get_or_spawn(session_id)?,
            AgentKind::Subagent(spec) => {
                self.spawn_subagent_inprocess(session_id, spec, parent_cancel)?;
                if let Some(parent) = parent {
                    self.link_subagent(&parent, session_id)?;
                }
            }
        }
        write(self.instances.get(session_id).expect("respawned instance"))
    }

    /// 向所有活跃 worker（含子代理）广播同一条 Ringing 命令。
    /// 只发给已运行的实例，不触发 spawn。返回失败项列表（seed: error）。
    pub fn broadcast_ringing(&mut self, command: &qaqh_ringing::RingingCommand) -> Vec<String> {
        let sessions: Vec<String> = self.instances.keys().cloned().collect();
        let mut failed = Vec::new();
        for session_id in sessions {
            let env = qaqh_ringing::RingingWorkerCommandEnvelope::new(
                &session_id,
                broadcast_command_id(),
                command.clone(),
            );
            if let Err(error) = self.send_ringing(&session_id, &env) {
                failed.push(format!("{session_id}: {error}"));
            }
        }
        failed
    }

    pub fn subscribe_channel(
        &mut self,
        session_id: &str,
        connection_id: ConnectionId,
        channel: RingingChannel,
    ) -> QaqhResult<bool> {
        let instance = self
            .instances
            .get_mut(session_id)
            .ok_or_else(|| format!("session {session_id} is not running"))?;
        match instance.apply_subscription(SubscriptionCommand::Subscribe {
            connection_id,
            channel,
        })? {
            SubscriptionEffect::Subscribed { changed, .. } => Ok(changed),
            effect => Err(format!("unexpected subscribe effect: {effect:?}").into()),
        }
    }

    pub fn unsubscribe_channel(
        &mut self,
        session_id: &str,
        connection_id: ConnectionId,
        channel: RingingChannel,
    ) -> QaqhResult<bool> {
        let instance = self
            .instances
            .get_mut(session_id)
            .ok_or_else(|| format!("session {session_id} is not running"))?;
        match instance.apply_subscription(SubscriptionCommand::Unsubscribe {
            connection_id,
            channel,
        })? {
            SubscriptionEffect::Unsubscribed { changed, .. } => Ok(changed),
            effect => Err(format!("unexpected unsubscribe effect: {effect:?}").into()),
        }
    }

    pub fn connection_closed(
        &mut self,
        session_id: &str,
        connection_id: ConnectionId,
    ) -> QaqhResult<usize> {
        let Some(instance) = self.instances.get_mut(session_id) else {
            return Ok(0);
        };
        match instance
            .apply_subscription(SubscriptionCommand::ConnectionClosed { connection_id })?
        {
            SubscriptionEffect::ConnectionClosed { removed, .. } => Ok(removed),
            effect => Err(format!("unexpected connection-close effect: {effect:?}").into()),
        }
    }

    /// Set root-tree quota limits. Existing in-memory ledgers are dropped;
    /// durable files remain and are replayed on next access.
    pub fn set_quota_limits(&mut self, limits: QuotaLimits) {
        self.quota_limits = limits;
        self.quota_ledgers.clear();
    }

    /// Set the maximum subagent tree depth. `1` means root may spawn children,
    /// but children may not spawn grandchildren.
    pub fn set_max_depth(&mut self, max_depth: usize) {
        self.max_depth = max_depth.max(1);
    }

    /// Set message safety-valve limits. `0` means unlimited.
    pub fn set_message_quota_limits(&mut self, in_flight_per_pair: u64, outbound_per_sender: u64) {
        self.message_in_flight_per_pair = in_flight_per_pair;
        self.message_outbound_per_sender = outbound_per_sender;
    }

    /// Admit one inter-agent send after the caller has resolved the target and
    /// computed the current canonical in-flight count.
    pub(crate) fn admit_outbound_message(
        &mut self,
        root_session_id: &str,
        author_path: &str,
        in_flight: u64,
    ) -> QaqhResult<()> {
        if self.message_in_flight_per_pair > 0 && in_flight >= self.message_in_flight_per_pair {
            return Err(format!(
                "message in-flight limit {} reached for {author_path}",
                self.message_in_flight_per_pair
            )
            .into());
        }
        let attempts = self
            .outbound_attempts
            .entry(root_session_id.to_string())
            .or_default()
            .entry(author_path.to_string())
            .or_insert(0);
        if self.message_outbound_per_sender > 0 && *attempts >= self.message_outbound_per_sender {
            return Err(format!(
                "outbound message limit {} reached for {author_path}",
                self.message_outbound_per_sender
            )
            .into());
        }
        *attempts = attempts.saturating_add(1);
        Ok(())
    }

    /// P2-7 test/ops view for one root ledger.
    #[doc(hidden)]
    pub fn quota_snapshot(&mut self, root: &str) -> QaqhResult<crate::quota_ledger::QuotaSnapshot> {
        self.quota_ledger_mut(root)?.snapshot()
    }

    pub(super) fn quota_ledger_mut(&mut self, root: &str) -> QaqhResult<&mut QuotaLedger> {
        if !self.quota_ledgers.contains_key(root) {
            let ledger = QuotaLedger::open(root, self.quota_limits)?;
            self.quota_ledgers.insert(root.to_string(), ledger);
        }
        self.quota_ledgers
            .get_mut(root)
            .ok_or_else(|| format!("quota ledger for {root} missing after open").into())
    }

    pub(super) fn reserve_spawn(
        &mut self,
        root: &str,
        child: &str,
    ) -> QaqhResult<QuotaReservation> {
        self.quota_ledger_mut(root)?
            .reserve(
                QuotaKind::Spawn,
                1,
                format!("subagent-spawn:{child}"),
                Some(child.to_string()),
            )
            .map_err(QaqhError::from)
    }

    pub(super) fn commit_spawn(&mut self, root: &str, reservation_id: &str) -> QaqhResult<()> {
        self.quota_ledger_mut(root)?
            .commit(reservation_id)
            .map_err(QaqhError::from)
    }

    pub(super) fn release_spawn(
        &mut self,
        root: &str,
        reservation_id: &str,
        reason: ReleaseReason,
    ) -> QaqhResult<()> {
        self.quota_ledger_mut(root)?
            .release(reservation_id, reason)
            .map_err(QaqhError::from)
    }

    /// 向所有存活 agent 广播同一 Ringing 命令。
    pub fn send_ringing_all(&mut self, command: qaqh_ringing::RingingCommand) {
        let sessions: Vec<_> = self.instances.keys().cloned().collect();
        for session_id in sessions {
            let env = qaqh_ringing::RingingWorkerCommandEnvelope::new(
                session_id.clone(),
                "daemon-broadcast",
                command.clone(),
            );
            let _ = self.send_ringing(&session_id, &env);
        }
    }
}
