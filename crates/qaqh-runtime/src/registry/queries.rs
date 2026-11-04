//! `AgentRegistry::queries` —— 只借用父模块（`super`）的既有定义与状态，
//! 不在这里新增字段。

use super::*;

use crate::error::QaqhResult;

impl AgentRegistry {
    /// Bounded-cardinality logical memory gauges for the daemon diagnostic API.
    pub fn memory_components(&self) -> Vec<qaqh_memwatch::ComponentMemory> {
        let mut worker_heap = self
            .instances
            .capacity()
            .saturating_mul(std::mem::size_of::<(String, AgentInstance)>());
        let mut session_id_bytes = 0usize;
        let mut session_workers = 0_u64;
        let mut subagent_workers = 0_u64;
        for (session_id, instance) in &self.instances {
            session_id_bytes = session_id_bytes.saturating_add(session_id.capacity());
            worker_heap = worker_heap.saturating_add(std::mem::size_of::<AgentInstance>());
            match &instance.kind {
                AgentKind::Session => session_workers += 1,
                AgentKind::Subagent(_) => subagent_workers += 1,
            }
        }
        // 回合循环不缓存上下文:消息流每回合从 SQLite 组装,无 mailbox/索引
        // 常驻(旧 actor 的 context/mailbox gauges 随旧框架移除)。
        vec![
            qaqh_memwatch::ComponentMemory {
                name: "agent_registry.workers".into(),
                item_count: self.instances.len() as u64,
                payload_bytes: Some(session_id_bytes as u64),
                heap_estimate_bytes: Some(worker_heap as u64),
                ..Default::default()
            },
            qaqh_memwatch::ComponentMemory {
                name: "agent_registry.lifecycle_maps".into(),
                item_count: self
                    .last_spawn
                    .len()
                    .saturating_add(self.residency.len())
                    .saturating_add(self.quota_ledgers.len())
                    .saturating_add(self.outbound_attempts.len())
                    .saturating_add(self.armed_collectors.len())
                    .saturating_add(self.spawn_configs.len()) as u64,
                payload_bytes: None,
                heap_estimate_bytes: None,
                ..Default::default()
            },
            qaqh_memwatch::ComponentMemory {
                name: "agent_registry.session_workers".into(),
                item_count: session_workers,
                payload_bytes: None,
                heap_estimate_bytes: None,
                ..Default::default()
            },
            qaqh_memwatch::ComponentMemory {
                name: "agent_registry.subagent_workers".into(),
                item_count: subagent_workers,
                payload_bytes: None,
                heap_estimate_bytes: None,
                ..Default::default()
            },
        ]
    }

    /// Read logical metadata by stable `AgentId = session_id`.
    pub fn agent_metadata(&self, agent_id: &str) -> Option<AgentMetadata> {
        self.agent_catalog.get_by_id(agent_id).cloned()
    }

    /// 运行中会话的回合循环句柄(宿主 wait_agent/诊断用)。
    pub fn loop_for(
        &self,
        session_id: &str,
    ) -> Option<Arc<qaqh_agent_runtime::session_loop::SessionLoop>> {
        self.instances.get(session_id).map(|instance| {
            let AgentTransport::Loop { loop_ } = &instance.transport;
            Arc::clone(loop_)
        })
    }

    /// List logical agents for a caller, resolving relative prefixes below the
    /// caller's path and constraining absolute prefixes to the same root tree.
    pub fn list_agents_for_caller(
        &mut self,
        caller_session_id: &str,
        path_prefix: &str,
    ) -> QaqhResult<Vec<AgentMetadata>> {
        self.ensure_root_metadata(caller_session_id)?;
        let caller = self
            .agent_catalog
            .get_by_id(caller_session_id)
            .cloned()
            .ok_or_else(|| format!("caller agent metadata missing for {caller_session_id}"))?;
        let prefix = caller
            .agent_path
            .resolve(path_prefix)
            .map_err(|error| format!("invalid path prefix {path_prefix:?}: {error}"))?;
        if prefix.namespace() != caller.agent_path.namespace() {
            return Err(format!(
                "path prefix {prefix} crosses agent namespaces from {}",
                caller.agent_path
            )
            .into());
        }
        Ok(self
            .agent_catalog
            .list_prefix(caller.root_session_id.as_str(), &prefix))
    }

    /// List logical agents with their explicit status and residency snapshot.
    ///
    /// Status is derived from the loop's live activity state while loaded and
    /// from the business database once unloaded. Residency comes from the
    /// registry's explicit lifecycle map; it is never inferred from the worker
    /// table.
    pub fn list_agents_with_state_for_caller(
        &mut self,
        caller_session_id: &str,
        path_prefix: &str,
    ) -> QaqhResult<Vec<ListedAgent>> {
        let agents = self.list_agents_for_caller(caller_session_id, path_prefix)?;
        let mut listed = Vec::with_capacity(agents.len());
        for agent in agents {
            let residency = self.agent_residency(agent.agent_id.as_str());
            let status = self.listed_agent_status(&agent, residency)?;
            listed.push(ListedAgent {
                root_session_id: agent.root_session_id.as_str().to_string(),
                agent_id: agent.agent_id.as_str().to_string(),
                agent_path: agent.agent_path.as_str().to_string(),
                parent_agent_path: agent
                    .parent_agent_path
                    .as_ref()
                    .map(|path| path.as_str().to_string()),
                nickname: agent.nickname.clone(),
                role: agent.role.clone(),
                status,
                residency,
                created_at_ms: agent.created_at_ms,
                outcome: self.subagent_outcomes.get(agent.agent_id.as_str()).cloned(),
            });
        }
        Ok(listed)
    }

    pub(super) fn agent_residency(&self, agent_id: &str) -> ListedAgentResidency {
        self.residency
            .get(agent_id)
            .copied()
            .unwrap_or(ListedAgentResidency::Unloaded)
    }

    pub(super) fn set_agent_residency(&mut self, agent_id: &str, residency: ListedAgentResidency) {
        self.residency.insert(agent_id.to_string(), residency);
    }

    pub(super) fn listed_agent_status(
        &self,
        agent: &AgentMetadata,
        residency: ListedAgentResidency,
    ) -> QaqhResult<ListedAgentStatus> {
        // 结果面优先:登记过终态摘要的子代理,状态就是终态(与 residency 无关)。
        if !agent.agent_path.is_root()
            && let Some(outcome) = self.subagent_outcomes.get(agent.agent_id.as_str())
        {
            return Ok(if outcome.exit_code == 0 {
                ListedAgentStatus::Completed
            } else {
                ListedAgentStatus::Errored
            });
        }

        if residency == ListedAgentResidency::Loaded
            && let Some(instance) = self.instances.get(agent.agent_id.as_str())
        {
            let AgentTransport::Loop { loop_ } = &instance.transport;
            return Ok(if loop_.waiting_user() {
                ListedAgentStatus::WaitingUser
            } else {
                // 回合结束不改变 agent 生命周期,loaded 会话保持 Running。
                ListedAgentStatus::Running
            });
        }

        // 未加载:业务库直读。
        match self.owners.session_run_state(agent.agent_id.as_str()) {
            Ok(qaqh_session::owner::SessionRunState::Running) => Ok(ListedAgentStatus::Running),
            Ok(qaqh_session::owner::SessionRunState::Waiting) => Ok(ListedAgentStatus::WaitingUser),
            Ok(qaqh_session::owner::SessionRunState::Cancelling) => Ok(ListedAgentStatus::Running),
            Ok(qaqh_session::owner::SessionRunState::Idle) => Ok(ListedAgentStatus::Running),
            Ok(qaqh_session::owner::SessionRunState::NeverStarted) => {
                Ok(ListedAgentStatus::PendingInit)
            }
            Err(error) => Err(format!("business status for {}: {error}", agent.agent_id).into()),
        }
    }

    /// Resolve a caller-relative or absolute target path within the caller's
    /// root tree.
    pub fn resolve_agent_for_caller(
        &mut self,
        caller_session_id: &str,
        target: &str,
    ) -> QaqhResult<AgentMetadata> {
        self.ensure_root_metadata(caller_session_id)?;
        let caller = self
            .agent_catalog
            .get_by_id(caller_session_id)
            .cloned()
            .ok_or_else(|| format!("caller agent metadata missing for {caller_session_id}"))?;
        let target_path = caller
            .agent_path
            .resolve(target)
            .map_err(|error| format!("invalid target agent path {target:?}: {error}"))?;
        if target_path.namespace() != caller.agent_path.namespace() {
            return Err(format!(
                "target {target_path} crosses agent namespaces from {}",
                caller.agent_path
            )
            .into());
        }
        self.agent_catalog
            .get_by_path(caller.root_session_id.as_str(), &target_path)
            .cloned()
            .ok_or_else(|| format!("target agent not found: {target_path}").into())
    }

    pub(super) fn ensure_root_metadata(&mut self, session_id: &str) -> QaqhResult<()> {
        if session_id.is_empty()
            || self.agent_catalog.get_by_id(session_id).is_some()
            || self.supervisor.parent_of(session_id).is_some()
        {
            return Ok(());
        }
        // 血缘以业务库为准:sessions.parent_session_id 有值即子代理,
        // 否则登记为根。
        match self.owners.session_parent(session_id) {
            Ok(Some(_)) => {}
            Ok(None) => {
                self.register_root_agent(session_id, unix_ms())?;
            }
            Err(error) => {
                return Err(format!("business parent lookup for {session_id}: {error}").into());
            }
        }
        Ok(())
    }

    pub fn activities(&self) -> Vec<qaqh_ringing_contract::SessionActivity> {
        let now = unix_ms().max(0) as u64;
        self.instances
            .values()
            .map(|instance| self.activity_of(instance, now))
            .collect()
    }

    pub fn activity(&self, session_id: &str) -> Option<qaqh_ringing_contract::SessionActivity> {
        let instance = self.instances.get(session_id)?;
        Some(self.activity_of(instance, unix_ms().max(0) as u64))
    }

    fn activity_of(
        &self,
        instance: &AgentInstance,
        now: u64,
    ) -> qaqh_ringing_contract::SessionActivity {
        let AgentTransport::Loop { loop_ } = &instance.transport;
        let state = if loop_.waiting_user() {
            qaqh_ringing_contract::SessionActivityState::WaitingUser
        } else {
            match loop_.activity_state() {
                qaqh_ringing_contract::fact::ActivityState::Running => {
                    qaqh_ringing_contract::SessionActivityState::Running
                }
                qaqh_ringing_contract::fact::ActivityState::WaitingUser => {
                    qaqh_ringing_contract::SessionActivityState::WaitingUser
                }
                qaqh_ringing_contract::fact::ActivityState::Cancelling => {
                    qaqh_ringing_contract::SessionActivityState::Cancelling
                }
                _ => qaqh_ringing_contract::SessionActivityState::Idle,
            }
        };
        qaqh_ringing_contract::SessionActivity {
            session_id: instance.session_id.clone(),
            state,
            turn_id: loop_.active_turn(),
            seq: 0,
            updated_at: now,
        }
    }

    pub fn is_running(&self, session_id: &str) -> bool {
        self.instances.contains_key(session_id)
    }
}
