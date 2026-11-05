//! `AgentRegistry::messaging` —— 只借用父模块（`super`）的既有定义与状态，
//! 不在这里新增字段。

use super::*;

use crate::error::{QaqhError, QaqhResult};

use qaqh_agent_runtime::session_loop::PermissionAnswer;

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
                qaqh_ringing::RingingCommand::ConversationSendMessage(input)
                if input.inter_agent.is_none()
            )
        {
            return Err(format!(
                "parent-owned agent {} rejects direct ConversationSendMessage without inter-agent metadata",
                metadata.agent_path
            ).into());
        }
        self.ensure_loaded_for_command(session_id)?;
        if matches!(&env.command, qaqh_ringing::RingingCommand::ConversationSendMessage(_))
            && self.instances.get(session_id).is_some_and(|instance| matches!(instance.kind, AgentKind::Session))
        {
            let owned = session_id.to_string();
            let selection = self.owners.db().read_blocking(move |conn| {
                qaqh_session::db::store::session_row(conn, &owned)
            }).map_err(QaqhError::message)?
                .and_then(|row| row.model_selection)
                .ok_or_else(|| QaqhError::invalid("请先为此会话选择服务商和模型"))?;
            let config = qaqh_config::watch::authoritative()
                .ok_or_else(|| QaqhError::invalid("模型目录不可用"))?;
            qaqh_profile::resolve_model_selection(&config.profiles_catalog, &selection)
                .map_err(QaqhError::message)?;
        }
        // A config reload replaces the provider *and* the tool environment:
        // the provider swap alone leaves the loop running the new model against
        // the old working directory / sandbox switch (report 2026-10-31 §11.4
        // F11). Both are resolved here, before the write closure.
        let reload = matches!(
            &env.command,
            qaqh_ringing::RingingCommand::AgentReloadConfig
        )
        .then(|| {
            let (mut config, _) =
                qaqh_agent_runtime::agent::session_config::session_effective_config(
                    Some(&self.owners),
                    session_id,
                )
                .ok_or_else(|| format!("effective config unavailable for {session_id}"))?;
            let kind = self
                .instances
                .get(session_id)
                .map(AgentInstance::kind_name)
                .ok_or_else(|| format!("loaded agent unavailable for {session_id}"))?;
            let (allowed_tools, global_allowed_tools, role) = match &kind {
                AgentKind::Session => {
                    let session = crate::session_meta::read(&self.owners, session_id)
                        .ok_or_else(|| format!("session metadata unavailable for {session_id}"))?;
                    let allowed =
                        qaqh_agent_runtime::session_agent::assembly::session_tool_allowlist(
                            &session.tool_mode,
                            &session.custom_tools,
                        )?;
                    (
                        allowed,
                        config
                            .tool_allowlist_enabled
                            .then(|| config.tool_allowlist.clone()),
                        qaqh_workspace::tool_api::SandboxMode::Main,
                    )
                }
                AgentKind::Subagent(spec) => {
                    if let Some(model) = &spec.model {
                        config.model = model.clone();
                    }
                    if let Some(base_url) = &spec.base_url {
                        config.base_url = base_url.clone();
                    }
                    if let Some(max_tokens) = spec.max_tokens {
                        config.max_tokens = max_tokens;
                    }
                    config.permission_level = spec.permission_level;
                    (
                        spec.tools.clone(),
                        None,
                        qaqh_agent_runtime::session_agent::assembly::subagent_sandbox(),
                    )
                }
            };
            let provider =
                qaqh_agent_runtime::session_agent::assembly::provider_spec(&config, Vec::new());
            let workspace_root = super::spawn::ensure_session_workspace(&self.owners, session_id)?;
            let policy = qaqh_agent_runtime::session_agent::assembly::tool_policy(
                workspace_root,
                config.permission_level,
                role,
                std::collections::HashSet::new(),
                config.sandbox.enabled,
            );
            Ok::<_, String>((provider, policy, allowed_tools, global_allowed_tools))
        });
        let write = |instance: &AgentInstance| -> QaqhResult<()> {
            let AgentTransport::Loop { loop_ } = &instance.transport;
            match &env.command {
                qaqh_ringing::RingingCommand::ConversationSendMessage(input) => loop_
                    .send_input(input.clone())
                    .map_err(|e| format!("loop input error: {e}").into()),
                qaqh_ringing::RingingCommand::ConversationCancel { turn_id } => {
                    // 目标校验先行:stale/错目标的取消不预置会话级取消标记
                    // (在途工具不得被滞留的旧请求中止)。
                    let matched = loop_.cancel_turn(turn_id.as_deref());
                    if matched || turn_id.is_none() {
                        qaqh_workspace::set_session_cancel(session_id, true);
                    }
                    Ok(())
                }
                qaqh_ringing::RingingCommand::InteractionAskRespond {
                    interaction_id,
                    answers,
                } => {
                    let ans: Vec<(String, String)> = answers
                        .iter()
                        .map(|a| (a.question_id.clone(), a.answer.clone()))
                        .collect();
                    loop_.respond_interaction(interaction_id, ans);
                    Ok(())
                }
                qaqh_ringing::RingingCommand::ToolPermissionRespond {
                    tool_call_id,
                    approved,
                    trust_folder,
                } => {
                    loop_.respond_permission(
                        tool_call_id,
                        PermissionAnswer {
                            approved: *approved,
                            trust_folder: *trust_folder,
                        },
                    );
                    Ok(())
                }
                qaqh_ringing::RingingCommand::SessionShutdown => {
                    loop_.abandon();
                    Ok(())
                }
                qaqh_ringing::RingingCommand::AgentReloadConfig => {
                    let (provider, policy, allowed_tools, global_allowed_tools) = reload
                        .clone()
                        .ok_or_else(|| format!("reload config unavailable for {session_id}"))?
                        .map_err(|error| {
                            format!("reload config failed for {session_id}: {error}")
                        })?;
                    loop_.reload_runtime(provider, policy, allowed_tools, global_allowed_tools);
                    Ok(())
                }
                other => Err(format!(
                    "session loop for {session_id} does not implement command {}",
                    ringing_command_name(other)
                )
                .into()),
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
                qaqh_ringing::RingingCommand::ConversationCancel { .. }
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
        self.close(session_id);
        match kind {
            AgentKind::Session => self.get_or_spawn(session_id)?,
            AgentKind::Subagent(spec) => {
                self.spawn_subagent_inprocess(session_id, spec)?;
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
    pub fn quota_snapshot(
        &mut self,
        root: &str,
    ) -> QaqhResult<qaqh_agent_runtime::quota_ledger::QuotaSnapshot> {
        self.quota_ledger_mut(root)?
            .snapshot()
            .map_err(QaqhError::from)
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
}
