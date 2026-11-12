//! `AgentRegistry::lifecycle` —— 只借用父模块（`super`）的既有定义与状态，
//! 不在这里新增字段。

use super::*;

use crate::error::QaqhResult;

impl AgentRegistry {
    /// Record a child's terminal outcome on the **result plane**.
    ///
    /// Called exactly once by the collector (through
    /// `SubagentHost::finish_subagent`). This is the replacement for the old
    /// `ProcessRegistry::set_answer` + `mark_exited` pair: the answer now lives
    /// with the agent identity instead of the exec process table.
    pub fn record_subagent_outcome(
        &mut self,
        child_session_id: &str,
        answer: &str,
        exit_code: i32,
    ) {
        if child_session_id.is_empty() {
            return;
        }
        self.subagent_outcomes.insert(
            child_session_id.to_string(),
            AgentOutcome {
                answer: (!answer.is_empty()).then(|| answer.to_string()),
                exit_code,
            },
        );
    }

    pub(crate) fn has_subagent_outcome(&self, child_session_id: &str) -> bool {
        self.subagent_outcomes.contains_key(child_session_id)
    }

    pub(crate) fn prepare_collector_arm(
        &mut self,
        child_id: &str,
    ) -> QaqhResult<Option<CollectorArmSpec>> {
        if self.armed_collectors.contains_key(child_id) {
            return Ok(None);
        }
        let child = self
            .agent_catalog
            .get_by_id(child_id)
            .cloned()
            .ok_or_else(|| format!("child agent metadata missing for {child_id}"))?;
        if child.agent_path.is_root() {
            return Ok(None);
        }
        let parent_path = child
            .parent_agent_path
            .as_ref()
            .ok_or_else(|| format!("child agent {child_id} has no parent path"))?;
        let parent = self
            .agent_catalog
            .get_by_path(child.root_session_id.as_str(), parent_path)
            .cloned()
            .ok_or_else(|| format!("child agent {child_id} parent metadata missing"))?;
        if !self.instances.contains_key(parent.agent_id.as_str()) {
            return Err(format!(
                "child agent {child_id} collector cannot arm: parent {} is unloaded",
                parent.agent_path
            )
            .into());
        }
        let parent_call_id = self
            .armed_collectors
            .get(child_id)
            .cloned()
            .unwrap_or_default();
        self.armed_collectors
            .insert(child_id.to_string(), parent_call_id.clone());
        Ok(Some(CollectorArmSpec {
            child_session_id: child.agent_id.as_str().to_string(),
            name: child.role.clone().unwrap_or_else(|| {
                child
                    .agent_path
                    .as_str()
                    .rsplit('/')
                    .next()
                    .unwrap_or("subagent")
                    .to_string()
            }),
            parent_session_id: parent.agent_id.as_str().to_string(),
            parent_call_id,
            // 收集器超时:spawn spec 不再携带 canonical 超时配置,取 15 分钟
            // 上界(子代理回合自身有 provider/tool 超时兜底)。
            timeout_secs: 900,
            root_session_id: child.root_session_id.as_str().to_string(),
            parent_agent_path: parent.agent_path.as_str().to_string(),
            child_agent_path: child.agent_path.as_str().to_string(),
        }))
    }

    pub(crate) fn mark_collector_armed(&mut self, child_id: &str, parent_call_id: &str) {
        self.armed_collectors
            .insert(child_id.to_string(), parent_call_id.to_string());
    }

    pub(crate) fn unmark_collector_armed(&mut self, child_id: &str) {
        self.armed_collectors.remove(child_id);
    }

    pub(crate) fn forget_sessions(&mut self, ids: &[String]) {
        for id in ids {
            self.agent_catalog.remove(id);
            self.residency.remove(id);
            self.subagent_outcomes.remove(id);
            self.outbound_attempts.remove(id);
            self.armed_collectors.remove(id);
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
            self.armed_collectors.remove(&child);
        }

        self.finish_for_unload(session_id);
        self.supervisor.unlink(session_id);
        self.armed_collectors.remove(session_id);
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
}
