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

    /// 读取子代理终态摘要；`None` = 尚未结束（或不是子代理）。
    ///
    /// 纯内存读，供 `wait_agent(targets=...)` 的完成判定使用——不去读
    /// canonical 日志，避免每次轮询都做文件 I/O。
    pub fn subagent_outcome(&self, child_session_id: &str) -> Option<AgentOutcome> {
        self.subagent_outcomes.get(child_session_id).cloned()
    }

    pub(crate) fn has_subagent_outcome(&self, child_session_id: &str) -> bool {
        self.subagent_outcomes.contains_key(child_session_id)
    }

    pub(crate) fn subagent_spawn_config(
        &self,
        parent_id: &str,
        child_id: &str,
    ) -> QaqhResult<(SubagentSpawnConfig, String)> {
        let parent_dir = self.sessions.session_path_dir(parent_id);
        let identity = CanonicalSessionIdentity::open(&parent_dir)
            .map_err(|error| format!("open parent canonical identity for {parent_id}: {error}"))?;
        let reader = CommittedFactReader::open(
            &parent_dir,
            identity.session_id.clone(),
            identity.log_id.clone(),
        )
        .map_err(|error| format!("open parent canonical facts for {parent_id}: {error}"))?;
        let child_id = SessionId::new(child_id);
        reader
            .read_all()
            .map_err(|error| format!("read parent canonical facts for {parent_id}: {error}"))?
            .into_iter()
            .rev()
            .find_map(|fact| match fact.payload {
                FactPayload::SubagentSpawned(payload) if payload.child_session_id == child_id => {
                    payload
                        .spawn_config
                        .map(|config| (config, payload.parent_call_id.as_str().to_string()))
                }
                _ => None,
            })
            .ok_or_else(|| {
                format!(
                    "child agent {child_id} has no canonical spawn config in parent {parent_id}"
                )
                .into()
            })
    }

    pub(crate) fn prepare_collector_arm(
        &mut self,
        child_id: &str,
    ) -> QaqhResult<Option<CollectorArmSpec>> {
        if self.armed_collectors.contains(child_id) {
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
        let (config, parent_call_id) =
            self.subagent_spawn_config(parent.agent_id.as_str(), child_id)?;
        self.armed_collectors.insert(child_id.to_string());
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
            timeout_secs: config.timeout_secs,
            root_session_id: child.root_session_id.as_str().to_string(),
            parent_agent_path: parent.agent_path.as_str().to_string(),
            child_agent_path: child.agent_path.as_str().to_string(),
        }))
    }

    pub(crate) fn mark_collector_armed(&mut self, child_id: &str) {
        self.armed_collectors.insert(child_id.to_string());
    }

    pub(crate) fn unmark_collector_armed(&mut self, child_id: &str) {
        self.armed_collectors.remove(child_id);
    }

    pub fn close(&mut self, session_id: &str) {
        let descendants = self.supervisor.begin_unload(session_id);
        for child in descendants {
            let parent = self.supervisor.parent_of(&child);
            self.finish_for_unload(&child, parent.as_deref());
            self.supervisor.unlink(&child);
            self.armed_collectors.remove(&child);
        }

        let parent = self.supervisor.parent_of(session_id);
        self.finish_for_unload(session_id, parent.as_deref());
        self.supervisor.unlink(session_id);
        self.supervisor.parent_unload_ack(session_id);
        self.armed_collectors.remove(session_id);
    }

    /// Signal, observe terminal, then join one worker. For a child, the parent
    /// edge is closed before the join, which is the P2-5 ordering contract.
    pub(super) fn finish_for_unload(&mut self, session_id: &str, parent: Option<&str>) {
        self.set_agent_residency(session_id, ListedAgentResidency::Unloaded);
        if let Some(parent) = parent {
            self.supervisor.cancel_sent(parent, session_id);
        }
        let Some(mut instance) = self.instances.remove(session_id) else {
            if let Some(parent) = parent {
                self.supervisor.child_terminal(parent, session_id);
                self.supervisor.parent_subagent_finished(parent, session_id);
                self.supervisor.child_joined(parent, session_id);
            }
            qaqh_workspace::remove_session_cancel(session_id);
            return;
        };

        instance.signal_shutdown();
        if !instance.wait_until_stopped(session_id) {
            log::warn!("[registry] worker {session_id} did not stop before join timeout");
        }
        if let Some(parent) = parent {
            self.supervisor.child_terminal(parent, session_id);
            self.supervisor.parent_subagent_finished(parent, session_id);
        }
        instance.finish_shutdown();
        if let Some(parent) = parent {
            self.supervisor.child_joined(parent, session_id);
        }
        qaqh_workspace::remove_session_cancel(session_id);
    }

    /// P2-5 测试/运维只读视图：parent/child 生命周期事件顺序。
    #[doc(hidden)]
    pub fn subagent_lifecycle_trace(&self) -> Vec<String> {
        self.supervisor
            .trace()
            .iter()
            .map(|event| match event {
                LifecycleEvent::EdgeLinked { parent, child } => {
                    format!("edge_linked:{parent}:{child}")
                }
                LifecycleEvent::EdgeUnlinked { parent, child } => {
                    format!("edge_unlinked:{parent}:{child}")
                }
                LifecycleEvent::ParentUnloadRequested { parent } => {
                    format!("parent_unload_requested:{parent}")
                }
                LifecycleEvent::ChildCancelSent { parent, child } => {
                    format!("child_cancel_sent:{parent}:{child}")
                }
                LifecycleEvent::ChildTerminal { parent, child } => {
                    format!("child_terminal:{parent}:{child}")
                }
                LifecycleEvent::ParentSubagentFinished { parent, child } => {
                    format!("parent_subagent_finished:{parent}:{child}")
                }
                LifecycleEvent::ChildJoined { parent, child } => {
                    format!("child_joined:{parent}:{child}")
                }
                LifecycleEvent::ParentUnloadAck { parent } => {
                    format!("parent_unload_ack:{parent}")
                }
            })
            .collect()
    }

    /// T-1-4：把父会话的取消传播到它派生的全部子 seed（递归覆盖孙代）。
    ///
    /// 子代理的取消入口与父会话一致：会话键控取消标记（子 seed 在途工具在
    /// 轮询点立即中止）+ `ConversationCancel` 命令（子 Loop 收尾当前回合并
    /// 发布 `ConversationCancelled`）。子代理同时从 `live_workers` 摘除：父
    /// 取消后它不再是活 worker，bootstrap 的孤儿收尾必须能收掉它遗留的
    /// running 状态，否则前端会把它投影为仍在运行。
    ///
    /// 只对**已在册**的子实例投递命令——绝不 `get_or_spawn`：已取消/已退出
    /// 的子代理不得因为一次取消传播而被重新拉起。
    pub(super) fn cancel_subagent_children(&mut self, parent: &str) {
        let children = self.children_of(parent);
        for child in children {
            self.supervisor.cancel_sent(parent, &child);
            qaqh_workspace::set_session_cancel(&child, true);
            if let Some(hub) = self.hub.as_ref() {
                hub.mark_worker_dead(&child);
            }
            let command_id = format!(
                "parent-cancel-{:x}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            );
            let env = qaqh_ringing::RingingWorkerCommandEnvelope::new(
                child.clone(),
                command_id.clone(),
                qaqh_ringing::RingingCommand::Conversation(
                    qaqh_domain::ConversationCommand::ConversationCancel { turn_id: None },
                ),
            );
            let delivered = match self.instances.get(&child) {
                Some(AgentInstance {
                    transport: AgentTransport::ThinAgent { handle, .. },
                    ..
                }) => handle.cancel(None, "subagent-parent-cancel").is_ok(),
                None => false,
            };
            log::info!(
                "[subagent] parent cancel {parent} propagated to child {child} (delivered={delivered})"
            );
            // 子 actor 自身分派取消时不经过 registry，孙代在此逐层传播。
            self.cancel_subagent_children(&child);
        }
    }

    /// Test/ops hook: the idle-unload liveness handle of a worker, if it has
    /// one (in-process actors only).
    #[doc(hidden)]
    pub fn worker_liveness(
        &self,
        session_id: &str,
    ) -> Option<std::sync::Arc<crate::agent::liveness::WorkerLiveness>> {
        self.instances
            .get(session_id)
            .and_then(|instance| instance.liveness.clone())
    }

    /// E: idle-unload workers whose last dispatch is older than `idle_secs`.
    /// Only in-process Session actors participate; subagents are short-lived
    /// and legacy process workers have no liveness handle. The close path is
    /// the regular graceful shutdown (signal → join → drop bundle → final
    /// flush/drain), so the next input resumes from disk via
    /// `load_for_resume`. Returns the seeds that were unloaded.
    pub fn unload_idle_sessions(&mut self, idle_secs: u64) -> Vec<String> {
        if self.shutting_down || idle_secs == 0 {
            return Vec::new();
        }
        let unloadable: Vec<String> = self
            .instances
            .iter()
            .filter_map(|(session_id, instance)| {
                if let Some(liveness) = instance.liveness.as_ref() {
                    return (liveness.unloadable() && liveness.idle_secs() >= idle_secs)
                        .then(|| session_id.clone());
                }
                // Thin agents have no liveness handle: idle is derived from the
                // activity tracker, and only a settled (idle) session may go.
                let activity = self.activity.get(session_id)?;
                if activity.state != qaqh_domain::ActivityState::Idle {
                    return None;
                }
                let idle_ms = unix_ms().saturating_sub(activity.updated_at as i64);
                (idle_ms >= (idle_secs as i64).saturating_mul(1000)).then(|| session_id.clone())
            })
            .collect();
        let mut unloaded = Vec::new();
        for session_id in unloadable {
            let idle = self
                .instances
                .get(&session_id)
                .and_then(|instance| instance.liveness.as_ref())
                .map(|liveness| liveness.idle_secs())
                .unwrap_or(0);
            log::info!("[registry] idle unload seed={session_id} idle={idle}s");
            self.close(&session_id);
            unloaded.push(session_id);
        }
        unloaded
    }

    pub fn shutdown_all(&mut self) {
        self.shutting_down = true;
        // Close roots through the supervisor so every child tree follows
        // terminal -> edge finish -> join -> parent ack.
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
        // Defensive fallback for an instance whose edge state was already
        // removed before shutdown.
        let leftovers: Vec<AgentInstance> = self
            .instances
            .drain()
            .map(|(_, instance)| instance)
            .collect();
        for instance in leftovers {
            instance.shutdown();
        }
    }

    /// F4: 拉起所有已退出且非优雅关闭的 worker。由 daemon 侧周期任务调用；
    /// 带 1 秒退避防止崩溃-重启风暴。优雅关闭（收到 Shutdown 帧后退出、
    /// 或被 `close`/`shutdown_all` 主动结束）的实例不会重启。
    pub fn respawn_dead_agents(&mut self) {
        if self.shutting_down {
            return;
        }
        let dead: Vec<(String, AgentKind, Option<crate::agent::types::CancelToken>)> = self
            .instances
            .iter()
            .filter(|(_, instance)| instance.is_dead())
            .filter(|(session_id, _)| {
                self.supervisor.parent_of(session_id).is_none_or(|parent| {
                    !self
                        .instances
                        .get(&parent)
                        .is_some_and(AgentInstance::is_dead)
                })
            })
            .map(|(session_id, instance)| {
                let parent_cancel = self
                    .supervisor
                    .parent_of(session_id)
                    .as_ref()
                    .and_then(|parent| self.cancel_for_session(parent));
                (session_id.clone(), instance.kind_name(), parent_cancel)
            })
            .collect();
        for (session_id, kind, parent_cancel) in dead {
            // 退避：同一 seed 最近 1 秒内刚 spawn 过（例如刚拉起又立刻崩溃）
            // 则跳过本轮，避免无意义的重启风暴。
            if self
                .last_spawn
                .get(&session_id)
                .is_some_and(|at| at.elapsed() < std::time::Duration::from_secs(1))
            {
                log::warn!(
                    "[AGENT:{session_id}] worker exited immediately after spawn; backing off"
                );
                continue;
            }
            if !self.children_of(&session_id).is_empty() {
                log::warn!(
                    "[AGENT:{session_id}] dead parent detected; closing child tree before respawn"
                );
            }
            self.close(&session_id);
            log::warn!("[AGENT:{session_id}] in-process worker died; respawning");
            // B9/R2：worker 存活登记在 spawn 之前完成。
            if let Some(hub) = self.hub.as_ref() {
                hub.mark_worker_live(&session_id);
            }
            let spawned = match kind {
                AgentKind::Session => self.spawn(&session_id, None),
                AgentKind::Subagent(spec) => {
                    self.spawn_subagent_inprocess(&session_id, spec, parent_cancel)
                }
            };
            if let Err(error) = spawned {
                log::error!("[AGENT:{session_id}] respawn failed: {error}");
            }
        }
    }

    /// Test/ops hook: whether the worker's loop thread has exited without
    /// having been reaped by `close` or `respawn_dead_agents`.
    #[doc(hidden)]
    pub fn worker_finished(&self, session_id: &str) -> bool {
        self.instances
            .get(session_id)
            .is_some_and(AgentInstance::is_dead)
    }
}
