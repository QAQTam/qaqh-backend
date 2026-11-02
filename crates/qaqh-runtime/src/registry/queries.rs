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
        let mut live_threads = 0_u64;
        let mut context_messages = 0_u64;
        let mut context_payload_bytes = 0_u64;
        let mut context_heap_estimate_bytes = 0_u64;
        let mut async_actors = 0_u64;
        let mut active_tasks = 0_u64;
        let mut mailbox_items = 0_u64;
        let mut mailbox_bytes = 0_u64;
        let mut journal_index_entries = 0_u64;
        let mut journal_index_heap_bytes = 0_u64;
        for (session_id, instance) in &self.instances {
            session_id_bytes = session_id_bytes.saturating_add(session_id.capacity());
            worker_heap = worker_heap.saturating_add(std::mem::size_of::<AgentInstance>());
            match &instance.kind {
                AgentKind::Session => session_workers += 1,
                AgentKind::Subagent(_) => subagent_workers += 1,
            }
            if instance
                .thread
                .as_ref()
                .is_some_and(|thread| !thread.is_finished())
            {
                live_threads += 1;
            }
            let AgentTransport::ThinAgent { handle, task } = &instance.transport;
            async_actors = async_actors.saturating_add(1);
            if !task.is_finished() {
                active_tasks = active_tasks.saturating_add(1);
                let memory = handle.memory_snapshot();
                context_messages = context_messages.saturating_add(memory.context_messages);
                context_payload_bytes =
                    context_payload_bytes.saturating_add(memory.context_payload_bytes);
                context_heap_estimate_bytes = context_heap_estimate_bytes
                    .saturating_add(memory.context_heap_estimate_bytes);
                mailbox_items = mailbox_items.saturating_add(memory.mailbox_items);
                mailbox_bytes = mailbox_bytes.saturating_add(memory.mailbox_bytes);
                journal_index_entries =
                    journal_index_entries.saturating_add(memory.journal_index_entries);
                journal_index_heap_bytes = journal_index_heap_bytes
                    .saturating_add(memory.journal_index_heap_estimate_bytes);
            }
            if instance
                .reader
                .as_ref()
                .is_some_and(|thread| !thread.is_finished())
            {
                live_threads += 1;
            }
        }
        let components = vec![
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
                    .saturating_add(self.armed_collectors.len()) as u64,
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
            qaqh_memwatch::ComponentMemory {
                name: "agent_registry.live_threads".into(),
                item_count: live_threads,
                payload_bytes: None,
                heap_estimate_bytes: None,
                ..Default::default()
            },
            qaqh_memwatch::ComponentMemory {
                name: "agent_registry.async_actors".into(),
                item_count: async_actors,
                payload_bytes: None,
                heap_estimate_bytes: None,
                ..Default::default()
            },
            qaqh_memwatch::ComponentMemory {
                name: "agent_registry.active_tasks".into(),
                item_count: active_tasks,
                payload_bytes: None,
                heap_estimate_bytes: None,
                ..Default::default()
            },
            qaqh_memwatch::ComponentMemory {
                name: "agent_registry.context".into(),
                item_count: context_messages,
                payload_bytes: Some(context_payload_bytes),
                heap_estimate_bytes: Some(context_heap_estimate_bytes),
                ..Default::default()
            },
            qaqh_memwatch::ComponentMemory {
                name: "agent_registry.mailbox".into(),
                item_count: mailbox_items,
                payload_bytes: Some(mailbox_bytes),
                heap_estimate_bytes: None,
                ..Default::default()
            },
            qaqh_memwatch::ComponentMemory {
                name: "agent_registry.journal_indexes".into(),
                item_count: journal_index_entries,
                payload_bytes: None,
                heap_estimate_bytes: Some(journal_index_heap_bytes),
                ..Default::default()
            },
        ];
        components
    }

    /// Read logical metadata by stable `AgentId = session_id`.
    pub fn agent_metadata(&self, agent_id: &str) -> Option<AgentMetadata> {
        self.agent_catalog.get_by_id(agent_id).cloned()
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
    /// Status is rebuilt from canonical facts and overlaid with the current
    /// activity tracker while loaded. Residency comes from the registry's
    /// explicit lifecycle map; it is never inferred from the worker table.
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
        self.publish_agent_residency(agent_id);
    }

    pub(super) fn publish_agent_residency(&self, agent_id: &str) {
        let Some(hub) = self.v2_hub.as_ref() else {
            return;
        };
        let Some(metadata) = self.agent_catalog.get_by_id(agent_id).cloned() else {
            return;
        };
        let residency = match self.agent_residency(agent_id) {
            ListedAgentResidency::Loaded => TeamAgentResidency::Loaded,
            ListedAgentResidency::Unloaded => TeamAgentResidency::Unloaded,
        };
        let session_dir = self
            .sessions
            .session_path_dir(metadata.root_session_id.as_str());
        if let Err(error) = hub.set_team_residency(
            &session_dir,
            metadata.root_session_id.as_str(),
            &metadata.agent_id,
            residency,
        ) {
            log::debug!(
                "[registry] residency overlay unavailable for agent {}: {error}",
                metadata.agent_path
            );
        }
    }

    pub(super) fn listed_agent_status(
        &self,
        agent: &AgentMetadata,
        residency: ListedAgentResidency,
    ) -> QaqhResult<ListedAgentStatus> {
        if residency == ListedAgentResidency::Loaded
            && let Some(activity) = self.activity.get(agent.agent_id.as_str())
        {
            match activity.state {
                qaqh_domain::ActivityState::Starting => {
                    return Ok(ListedAgentStatus::PendingInit);
                }
                qaqh_domain::ActivityState::Working => {
                    return Ok(ListedAgentStatus::Running);
                }
                qaqh_domain::ActivityState::WaitingUser => {
                    return Ok(ListedAgentStatus::WaitingUser);
                }
                qaqh_domain::ActivityState::Disconnected => {
                    return Ok(ListedAgentStatus::Shutdown);
                }
                // Failed 与 Idle 同权：回合错误不改变 agent 生命周期，
                // listed 状态继续走下方按事实推导的路径。
                qaqh_domain::ActivityState::Idle | qaqh_domain::ActivityState::Failed => {}
            }
        }

        // A terminal child fact lives in the parent log. For an unloaded child
        // it is the authoritative final lifecycle state, even if a later
        // shutdown attempt emitted a generic turn interruption in the child log.
        if !agent.agent_path.is_root()
            && residency == ListedAgentResidency::Unloaded
            && let Some(status) = self.parent_terminal_status(agent)?
        {
            return Ok(status);
        }

        self.session_canonical_status(agent.agent_id.as_str())
    }

    pub(super) fn session_canonical_status(&self, agent_id: &str) -> QaqhResult<ListedAgentStatus> {
        let mut status = ListedAgentStatus::PendingInit;
        for fact in self.read_session_facts(agent_id)? {
            match &fact.payload {
                FactPayload::TurnStarted(_) => status = ListedAgentStatus::Running,
                FactPayload::TurnInterrupted(_) => status = ListedAgentStatus::Interrupted,
                FactPayload::InteractionRequested(_) => status = ListedAgentStatus::WaitingUser,
                FactPayload::InteractionResolved(_) | FactPayload::InteractionExpired(_)
                    if status == ListedAgentStatus::WaitingUser =>
                {
                    status = ListedAgentStatus::Running;
                }
                FactPayload::TurnFinished(_) if status == ListedAgentStatus::WaitingUser => {
                    status = ListedAgentStatus::Running;
                }
                FactPayload::SubagentFinished(payload) => {
                    status = listed_terminal_status(payload.status);
                }
                FactPayload::SessionDeleted(_) => status = ListedAgentStatus::Shutdown,
                _ => {}
            }
        }
        Ok(status)
    }

    pub(super) fn parent_terminal_status(
        &self,
        agent: &AgentMetadata,
    ) -> QaqhResult<Option<ListedAgentStatus>> {
        let parent_path = agent
            .parent_agent_path
            .as_ref()
            .ok_or_else(|| format!("child agent {} has no parent path", agent.agent_id))?;
        let parent = self
            .agent_catalog
            .get_by_path(agent.root_session_id.as_str(), parent_path)
            .cloned()
            .ok_or_else(|| {
                format!(
                    "child agent {} parent metadata missing at {parent_path}",
                    agent.agent_id
                )
            })?;
        let child_id = SessionId::new(agent.agent_id.as_str());
        let status = self
            .read_session_facts(parent.agent_id.as_str())?
            .into_iter()
            .rev()
            .find_map(|fact| match fact.payload {
                FactPayload::SubagentFinished(payload) if payload.child_session_id == child_id => {
                    Some(listed_terminal_status(payload.status))
                }
                _ => None,
            });
        Ok(status)
    }

    pub(super) fn read_session_facts(&self, session_id: &str) -> QaqhResult<Vec<SessionFact>> {
        let session_dir = self.sessions.session_path_dir(session_id);
        if !session_dir.join(EVENTS_COMMIT_FILE).exists() {
            return Ok(Vec::new());
        }
        let identity = CanonicalSessionIdentity::open(&session_dir)
            .map_err(|error| format!("open canonical identity for {session_id}: {error}"))?;
        if identity.session_id.as_str() != session_id {
            return Err(format!(
                "canonical identity for {session_id} is {}",
                identity.session_id
            )
            .into());
        }
        let reader = CommittedFactReader::open(
            &session_dir,
            identity.session_id.clone(),
            identity.log_id.clone(),
        )
        .map_err(|error| format!("open canonical facts for {session_id}: {error}"))?;
        reader
            .read_all()
            .map_err(|error| format!("read canonical facts for {session_id}: {error}").into())
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
        if self.canonical_parent_session_id(session_id)?.is_none() {
            self.register_root_agent(session_id, unix_ms())?;
        }
        Ok(())
    }

    /// Read the `SessionCreated.parent_session_id` recovery hint from the
    /// canonical log. This is deliberately a hint only: graph ownership is
    /// established by parent-log `SubagentSpawned` facts in SUBV2-03/04.
    pub(super) fn canonical_parent_session_id(
        &self,
        session_id: &str,
    ) -> QaqhResult<Option<String>> {
        let session_dir = self.sessions.session_path_dir(session_id);
        if !session_dir.exists() {
            return Ok(None);
        }
        // This is a read-only recovery hint. Do not create the identity
        // sidecar here: a session directory can legitimately exist before the
        // canonical baseline has been materialized.
        if !session_dir.join(CANONICAL_IDENTITY_FILE).exists() {
            return Ok(None);
        }
        let has_events = session_dir.join(EVENTS_FILE).exists();
        let has_commit = session_dir.join(EVENTS_COMMIT_FILE).exists();
        let batch_root = qaqh_session::canonical::is_batch_root(&session_dir);
        if !has_events && !has_commit && !batch_root {
            // `persist_new_session` creates a metadata-only directory before
            // the first canonical fact. That is still a root session.

            return Ok(None);
        }
        let identity = CanonicalSessionIdentity::open_or_create(&session_dir)
            .map_err(|error| format!("open canonical identity for {session_id}: {error}"))?;
        let facts = crate::agent_graph::read_session_facts(
            &session_dir,
            identity.session_id.clone(),
            identity.log_id.clone(),
        )
        .map_err(|error| format!("read committed facts for {session_id}: {error}"))?;
        for fact in facts {
            if let FactPayload::SessionCreated(created) = fact.payload {
                return Ok(created
                    .parent_session_id
                    .map(|parent| parent.as_str().to_string()));
            }
        }
        Ok(None)
    }

    pub fn activities(&self) -> Vec<qaqh_domain::SessionActivity> {
        self.activity.snapshot()
    }

    pub fn activity(&self, session_id: &str) -> Option<qaqh_domain::SessionActivity> {
        self.activity.get(session_id)
    }

    pub fn is_running(&self, session_id: &str) -> bool {
        self.instances
            .get(session_id)
            .is_some_and(|instance| !instance.is_dead())
    }
}
