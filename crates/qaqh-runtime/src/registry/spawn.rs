//! `AgentRegistry::spawn` —— 只借用父模块（`super`）的既有定义与状态，
//! 不在这里新增字段。

use super::*;

use crate::error::{QaqhError, QaqhResult};

/// Forwards in-flight generation updates to the Ringing v2 conversation stream.
///
/// The hub keeps only the latest value per block identity, so a slow or
/// disconnected client sees the current snapshot instead of a token backlog.
/// Publishing is best-effort: a missing or closed session must never fail a
/// generation that is otherwise fine.
struct V2ProgressSink {
    hub: Arc<V2ProjectionHub>,
    session_dir: std::path::PathBuf,
    session_id: String,
}

impl V2ProgressSink {
    fn new(hub: &Arc<V2ProjectionHub>, session_dir: &std::path::Path, session_id: &str) -> Self {
        Self {
            hub: Arc::clone(hub),
            session_dir: session_dir.to_path_buf(),
            session_id: session_id.to_string(),
        }
    }
}

impl qaqh_agent_runtime::progress::ProgressSink for V2ProgressSink {
    fn assistant_block(&self, update: qaqh_agent_runtime::progress::AssistantProgress<'_>) {
        let kind = match update.kind {
            qaqh_agent_runtime::progress::ProgressBlockKind::Reasoning => {
                qaqh_session::session_fact_v2::AssistantBlockKind::Reasoning
            }
            qaqh_agent_runtime::progress::ProgressBlockKind::Answer => {
                qaqh_session::session_fact_v2::AssistantBlockKind::Answer
            }
        };
        if let Err(error) = self.hub.publish_assistant_progress(
            &self.session_dir,
            &self.session_id,
            update.turn_id,
            update.block_id,
            kind,
            update.model,
            update.text,
        ) {
            log::debug!(
                "[progress] assistant block publish failed for {}: {error}",
                self.session_id
            );
        }
    }

    fn tool_input(&self, update: qaqh_agent_runtime::progress::ToolInputProgress<'_>) {
        if let Err(error) = self.hub.publish_tool_input_progress(
            &self.session_dir,
            &self.session_id,
            update.turn_id,
            update.call_id,
            update.tool_name,
            update.args,
        ) {
            log::debug!(
                "[progress] tool input publish failed for {}: {error}",
                self.session_id
            );
        }
    }

    fn attempt_finished(&self, _turn_id: &str, block_ids: &[String], call_ids: &[String]) {
        for block_id in block_ids {
            if let Err(error) =
                self.hub
                    .clear_assistant_progress(&self.session_dir, &self.session_id, block_id)
            {
                log::debug!(
                    "[progress] block cleanup failed for {}: {error}",
                    self.session_id
                );
            }
        }
        for call_id in call_ids {
            if let Err(error) =
                self.hub
                    .clear_tool_input_progress(&self.session_dir, &self.session_id, call_id)
            {
                log::debug!(
                    "[progress] tool input cleanup failed for {}: {error}",
                    self.session_id
                );
            }
        }
    }
}

/// Resolve and materialize the directory a session's tools run in.
///
/// A session without a bound workspace gets the default session workspace,
/// created on demand. That exact path is what the model is told about, what the
/// sandbox receives as a writable root and what the process starts in, so it has
/// to exist before the first `exec` — otherwise the sandbox preparation fails.
/// A workspace the user picked must already exist; creating an empty directory
/// with the same name would silently hide a vanished project.
pub(crate) fn ensure_session_workspace(
    sessions: &qaqh_session::SessionManager,
    session_id: &str,
) -> Result<std::path::PathBuf, String> {
    let bound = sessions
        .workspace_cwd(session_id)
        .filter(|cwd| !cwd.is_empty() && cwd != ".");
    match bound {
        Some(cwd) => {
            let path = std::path::PathBuf::from(cwd);
            if !path.is_dir() {
                if !qaqh_types::platform::is_managed_session_workspace(&path) {
                    return Err(format!(
                        "session workspace does not exist: {}",
                        path.display()
                    ));
                }
                std::fs::create_dir_all(&path).map_err(|error| {
                    format!("create session workspace {}: {error}", path.display())
                })?;
            }
            Ok(path)
        }
        None => {
            let path = qaqh_types::platform::default_session_workspace_dir(session_id);
            std::fs::create_dir_all(&path).map_err(|error| {
                format!(
                    "create default session workspace {}: {error}",
                    path.display()
                )
            })?;
            Ok(path)
        }
    }
}

impl AgentRegistry {
    /// Register a root session as `/root`.
    ///
    /// This is idempotent for the same root session. A conflicting agent id or
    /// path is rejected instead of silently replacing logical metadata.
    pub fn register_root_agent(
        &mut self,
        session_id: &str,
        created_at_ms: i64,
    ) -> QaqhResult<AgentMetadata> {
        let session_dir = self.sessions.session_path_dir(session_id);
        let identity = CanonicalSessionIdentity::open_or_create(&session_dir)
            .map_err(|error| format!("open canonical identity for root {session_id}: {error}"))?;
        self.agent_catalog
            .register_root_with_alias(identity.session_id, Some(session_id), created_at_ms)
            .map_err(|error| error.into())
    }

    pub fn get_or_spawn(&mut self, session_id: &str) -> QaqhResult<()> {
        self.ensure_root_metadata(session_id)?;
        if self.instances.contains_key(session_id) {
            return Ok(());
        }
        // B9/R2：worker 存活登记在 spawn 之前完成。
        if let Some(hub) = self.hub.as_ref() {
            hub.mark_worker_live(session_id);
        }
        self.spawn(session_id, None)?;
        Ok(())
    }

    pub fn spawn_new(&mut self, session_id: &str) -> QaqhResult<()> {
        if self.instances.contains_key(session_id) {
            return Err(format!("agent already running for {session_id}").into());
        }
        self.spawn_with(session_id, Some(session_id), &[])
    }

    /// Spawn an isolated subagent worker **inside the daemon process**.
    ///
    /// Knife-1 step 1 removes `Command::new(current_exe)` and the child-process
    /// leg of the subagent path.
    ///
    /// The subagent 是一个普通会话 agent（`qaqh_agent_runtime::session_agent`
    /// 驱动），跑在 daemon 进程内的独立 actor 线程上；其命令/事件通道与
    /// pipe wire 是同一套 typed envelope，所以 daemon 侧的发布逻辑不用改。
    pub fn spawn_subagent(
        &mut self,
        session_id: &str,
        tools: &[String],
        model: Option<&str>,
        base_url: Option<&str>,
        max_tokens: Option<u32>,
    ) -> QaqhResult<()> {
        self.spawn_subagent_in_workspace(session_id, tools, model, base_url, max_tokens, None)
    }

    /// Spawn a subagent with the workspace the caller explicitly requested.
    ///
    /// The explicit path takes the same route as session creation: it is
    /// validated once and written into the child's genesis fact, instead of
    /// being inferred later from the parent metadata (report 2026-10-31
    /// §11.1 F09, requirement 4).
    pub fn spawn_subagent_in_workspace(
        &mut self,
        session_id: &str,
        tools: &[String],
        model: Option<&str>,
        base_url: Option<&str>,
        max_tokens: Option<u32>,
        workspace: Option<&str>,
    ) -> QaqhResult<()> {
        let persist = std::env::var("QAQH_SUBAGENT_PERSIST")
            .is_ok_and(|value| matches!(value.as_str(), "1" | "true" | "on"));
        let options = SubagentSpawnOptions {
            tools,
            model,
            base_url,
            max_tokens,
            workspace,
            ephemeral: !persist,
            // legacy/bootstrap 无父会话上下文，回落全局 config（fail-closed 1）。
            permission_level: qaqh_config::Config::load()
                .ok()
                .map(|cfg| cfg.permission_level)
                .unwrap_or(1),
        };
        let parent_session = qaqh_workspace::runtime::context()
            .map(|ctx| ctx.active_session)
            .unwrap_or_default();
        if parent_session.is_empty() {
            let child_dir = self.sessions.session_path_dir(session_id);
            let child_identity =
                CanonicalSessionIdentity::open_or_create(&child_dir).map_err(|error| {
                    format!("open child canonical identity for {session_id}: {error}")
                })?;
            return self.spawn_subagent_internal(
                session_id,
                &parent_session,
                None,
                child_identity.session_id,
                options,
            );
        }
        let requested_name = legacy_child_name(session_id);
        self.spawn_subagent_v2(session_id, &parent_session, &requested_name, options)
            .map(|_| ())
    }

    /// V2 spawn path: allocate a tree-relative child path, create the actor,
    /// and register logical metadata before the caller commits the canonical
    /// `SubagentSpawned` edge.
    pub(crate) fn spawn_subagent_v2(
        &mut self,
        session_id: &str,
        parent_session_id: &str,
        requested_name: &str,
        options: SubagentSpawnOptions<'_>,
    ) -> QaqhResult<SpawnedSubagentInfo> {
        if parent_session_id.is_empty() {
            return Err("subagent v2 requires a parent_session_id".into());
        }
        self.ensure_root_metadata(parent_session_id)?;
        let parent = self
            .agent_catalog
            .get_by_id(parent_session_id)
            .cloned()
            .ok_or_else(|| format!("subagent parent metadata missing for {parent_session_id}"))?;
        if parent.agent_path.depth() >= self.max_depth {
            return Err(format!(
                "subagent max depth {} reached at parent {} (depth {}); raise config.subagent.max_depth and restart the daemon to allow deeper nesting",
                self.max_depth,
                parent.agent_path,
                parent.agent_path.depth()
            ).into());
        }
        let child_path = parent
            .agent_path
            .child(requested_name)
            .map_err(|error| format!("invalid subagent name {requested_name:?}: {error}"))?;
        if self
            .agent_catalog
            .get_by_path(parent.root_session_id.as_str(), &child_path)
            .is_some()
        {
            return Err(format!(
                "subagent path {child_path} already exists in root {}",
                parent.root_session_id
            )
            .into());
        }

        let parent_dir = self.sessions.session_path_dir(parent_session_id);
        let parent_identity =
            CanonicalSessionIdentity::open_or_create(&parent_dir).map_err(|error| {
                format!("open parent canonical identity for {parent_session_id}: {error}")
            })?;
        let child_dir = self.sessions.session_path_dir(session_id);
        let child_identity = CanonicalSessionIdentity::open_or_create(&child_dir)
            .map_err(|error| format!("open child canonical identity for {session_id}: {error}"))?;
        // Explicit request first: the child genesis fact must carry the path
        // the caller named, not a value inferred from parent metadata.
        let requested = options
            .workspace
            .map(str::trim)
            .filter(|workspace| !workspace.is_empty() && *workspace != ".");
        let cwd = match requested {
            Some(workspace) => {
                let path = std::path::PathBuf::from(workspace);
                let path = std::fs::canonicalize(&path)
                    .map(crate::service::normalize_workspace_path)
                    .map_err(|error| {
                        format!(
                            "subagent workspace does not exist: {} ({error})",
                            path.display()
                        )
                    })?;
                if !path.is_dir() {
                    return Err(format!(
                        "subagent workspace is not a directory: {}",
                        path.display()
                    )
                    .into());
                }
                path.to_string_lossy().into_owned()
            }
            None => self
                .sessions
                .workspace_cwd(session_id)
                .filter(|cwd| !cwd.is_empty() && cwd != ".")
                .or_else(|| {
                    self.sessions
                        .workspace_cwd(parent_session_id)
                        .filter(|cwd| !cwd.is_empty() && cwd != ".")
                })
                .unwrap_or_else(|| {
                    // 父子会话都没有工作区：落默认工作根（与
                    // `load_session_workspace` 同源），绝不兜底进程 cwd——
                    // daemon 的进程 cwd 是安装目录。
                    qaqh_types::platform::default_session_workspace_dir(session_id)
                        .to_string_lossy()
                        .into_owned()
                }),
        };
        crate::service::materialize_canonical_session_in(
            &child_dir,
            &cwd,
            options.model.unwrap_or("unknown"),
            Some(parent_identity.session_id.clone()),
            true,
        )?;

        let identity = Some((parent.clone(), child_path.clone()));
        self.spawn_subagent_internal(
            session_id,
            parent_session_id,
            identity,
            child_identity.session_id.clone(),
            options,
        )?;
        Ok(SpawnedSubagentInfo {
            child_session_id: child_identity.session_id,
            parent_agent_path: parent.agent_path,
            child_agent_path: child_path,
        })
    }

    /// Roll back a child registration after its canonical spawn edge failed.
    ///
    /// The actor may already exist; closing it is safe, while the durable child
    /// session remains available for recovery because V2 children are
    /// persistent.
    pub fn rollback_subagent(&mut self, session_id: &str, child_session_id: &str) {
        self.agent_catalog.remove(child_session_id);
        self.residency.remove(child_session_id);
        // 结果面同步回滚：边从未成为权威事实，不得留下终态摘要。
        self.subagent_outcomes.remove(child_session_id);
        self.close(session_id);
    }

    pub(super) fn spawn_subagent_internal(
        &mut self,
        session_id: &str,
        parent_session: &str,
        identity: Option<(AgentMetadata, AgentPath)>,
        child_session_id: SessionId,
        options: SubagentSpawnOptions<'_>,
    ) -> QaqhResult<()> {
        let SubagentSpawnOptions {
            tools,
            model,
            base_url,
            max_tokens,
            workspace: _,
            ephemeral,
            permission_level,
        } = options;
        if self.instances.contains_key(session_id) {
            return Err(format!("agent already running for {session_id}").into());
        }
        if !ephemeral {
            self.sessions.set_ephemeral(session_id, false);
        }
        let root_session = if parent_session.is_empty() {
            session_id.to_string()
        } else {
            self.supervisor.root_of(parent_session)
        };
        // Durable reservation must exist before the child actor can perform
        // any side effect.
        let reservation = self.reserve_spawn(&root_session, session_id)?;
        let parent_cancel = self.cancel_for_session(parent_session);
        if let Err(error) = self.spawn_subagent_inprocess(
            session_id,
            SubagentSpawnSpec {
                tools: tools.to_vec(),
                model: model.map(str::to_string),
                base_url: base_url.map(str::to_string),
                max_tokens,
                permission_level,
            },
            parent_cancel,
        ) {
            let _ = self.release_spawn(
                &root_session,
                &reservation.reservation_id,
                ReleaseReason::Cancelled,
            );
            return Err(error);
        }
        if !parent_session.is_empty()
            && parent_session != session_id
            && let Err(error) = self.link_subagent(parent_session, session_id)
        {
            self.close(session_id);
            let _ = self.release_spawn(
                &root_session,
                &reservation.reservation_id,
                ReleaseReason::Cancelled,
            );
            return Err(error);
        }
        if let Some((parent, child_path)) = identity {
            let metadata = AgentMetadata {
                root_session_id: parent.root_session_id.clone(),
                agent_id: child_session_id.clone(),
                agent_path: child_path,
                parent_agent_path: Some(parent.agent_path),
                nickname: None,
                role: None,
                created_at_ms: unix_ms(),
            };
            if let Err(error) = self
                .agent_catalog
                .register_with_alias(metadata, Some(session_id))
            {
                self.close(session_id);
                let _ = self.release_spawn(
                    &root_session,
                    &reservation.reservation_id,
                    ReleaseReason::Reconciliation,
                );
                return Err(error.into());
            }
            self.publish_agent_residency(child_session_id.as_str());
        }
        if let Err(error) = self.commit_spawn(&root_session, &reservation.reservation_id) {
            self.agent_catalog.remove(child_session_id.as_str());
            self.close(session_id);
            let _ = self.release_spawn(
                &root_session,
                &reservation.reservation_id,
                ReleaseReason::Reconciliation,
            );
            return Err(error);
        }
        Ok(())
    }

    pub(super) fn spawn_subagent_inprocess(
        &mut self,
        session_id: &str,
        spec: SubagentSpawnSpec,
        _parent_cancel: Option<crate::agent::types::CancelToken>,
    ) -> QaqhResult<()> {
        if self.instances.contains_key(session_id) {
            return Err(format!("agent already running for {session_id}").into());
        }
        self.last_spawn
            .insert(session_id.to_string(), std::time::Instant::now());
        let (activity_generation, _) = self.activity.begin(session_id);

        let session_dir = self.sessions.session_path_dir(session_id);
        let (mut agent_config, _session_profile) =
            crate::agent::session_config::session_effective_config(
                Some(&self.sessions),
                session_id,
            )
            .unwrap_or_default();
        if let Some(model) = &spec.model {
            agent_config.model = model.clone();
        }
        if let Some(base_url) = &spec.base_url {
            agent_config.base_url = base_url.clone();
        }
        if let Some(max_tokens) = spec.max_tokens {
            agent_config.max_tokens = max_tokens;
        }

        let ws_dir = ensure_session_workspace(&self.sessions, session_id)?;
        let prepared =
            crate::session_agent::assembly::assemble(crate::session_agent::assembly::SpawnInput {
                session_id,
                session_dir: &session_dir,
                config: &agent_config,
                allowed_tools: spec.tools.clone(),
                permission_level: spec.permission_level,
                sandbox: crate::session_agent::assembly::subagent_sandbox(),
                workspace_root: ws_dir,
            })?;

        let (handle, task) = crate::session_agent::AgentHost::spawn_with_activity(
            session_id.to_string(),
            prepared.commit,
            prepared.provider,
            prepared.port,
            Some(crate::session_agent::ActivityTap {
                tracker: self.activity.clone(),
                generation: activity_generation,
            }),
            self.v2_hub.as_ref().map(|hub| {
                Arc::new(V2ProgressSink::new(hub, &session_dir, session_id))
                    as Arc<dyn qaqh_agent_runtime::progress::ProgressSink>
            }),
        )
        .map_err(|e| format!("spawn thin subagent for {session_id}: {e}"))?;
        let handle =
            handle.with_memory(Arc::new(crate::session_agent::SessionAgentMemory::default()));
        self.instances.insert(
            session_id.to_string(),
            AgentInstance {
                session_id: session_id.to_string(),
                transport: AgentTransport::ThinAgent { handle, task },
                kind: AgentKind::Subagent(spec),
                subscription_actor: SessionActor::new(16),
                liveness: None,
                reader: None,
                thread: None,
            },
        );
        self.set_agent_residency(session_id, ListedAgentResidency::Loaded);
        if let Some(hub) = self.hub.as_ref() {
            hub.mark_worker_live(session_id);
        }
        log::info!("[subagent] spawned thin SessionAgent async task for subagent={session_id}");
        Ok(())
    }

    pub(super) fn spawn(&mut self, session_id: &str, new_session: Option<&str>) -> QaqhResult<()> {
        self.spawn_with(session_id, new_session, &[])
    }

    pub(super) fn spawn_with(
        &mut self,
        session_id: &str,
        new_session: Option<&str>,
        extra_args: &[String],
    ) -> QaqhResult<()> {
        self.spawn_session_inprocess(session_id, new_session, extra_args)
    }

    pub(super) fn spawn_session_inprocess(
        &mut self,
        session_id: &str,
        _new_session: Option<&str>,
        _extra_args: &[String],
    ) -> QaqhResult<()> {
        if self.instances.contains_key(session_id) {
            return Err(format!("agent already running for {session_id}").into());
        }
        self.last_spawn
            .insert(session_id.to_string(), std::time::Instant::now());
        let (activity_generation, _) = self.activity.begin(session_id);

        let session_dir = self.sessions.session_path_dir(session_id);
        let (agent_config, _session_profile) =
            crate::agent::session_config::session_effective_config(
                Some(&self.sessions),
                session_id,
            )
            .unwrap_or_default();

        let ws_dir = ensure_session_workspace(&self.sessions, session_id)?;
        let prepared = crate::session_agent::assembly::assemble(
            crate::session_agent::assembly::SpawnInput::for_session(
                session_id,
                &session_dir,
                &agent_config,
                ws_dir,
            ),
        )?;

        let (handle, task) = crate::session_agent::AgentHost::spawn_with_activity(
            session_id.to_string(),
            prepared.commit,
            prepared.provider,
            prepared.port,
            Some(crate::session_agent::ActivityTap {
                tracker: self.activity.clone(),
                generation: activity_generation,
            }),
            self.v2_hub.as_ref().map(|hub| {
                Arc::new(V2ProgressSink::new(hub, &session_dir, session_id))
                    as Arc<dyn qaqh_agent_runtime::progress::ProgressSink>
            }),
        )
        .map_err(|e| format!("spawn thin agent for {session_id}: {e}"))?;
        let handle =
            handle.with_memory(Arc::new(crate::session_agent::SessionAgentMemory::default()));
        self.instances.insert(
            session_id.to_string(),
            AgentInstance {
                session_id: session_id.to_string(),
                transport: AgentTransport::ThinAgent { handle, task },
                kind: AgentKind::Session,
                subscription_actor: SessionActor::new(16),
                liveness: None,
                reader: None,
                thread: None,
            },
        );
        self.set_agent_residency(session_id, ListedAgentResidency::Loaded);
        log::info!("[session] spawned thin SessionAgent async task for session={session_id}");
        Ok(())
    }

    /// Materialize the delivery target without turning a logical child into a
    /// root session. Roots use the ordinary get-or-spawn path; children reload
    /// only through a loaded immediate parent and reuse the durable spawn
    /// config recorded in the parent's canonical `SubagentSpawned` fact.
    pub(super) fn ensure_loaded_for_command(&mut self, session_id: &str) -> QaqhResult<()> {
        if self.instances.contains_key(session_id) {
            return Ok(());
        }
        let Some(metadata) = self.agent_catalog.get_by_id(session_id).cloned() else {
            return self.get_or_spawn(session_id);
        };
        if metadata.agent_path.is_root() {
            return self.get_or_spawn(session_id);
        }
        let parent_path = metadata
            .parent_agent_path
            .as_ref()
            .ok_or_else(|| format!("child agent {session_id} has no parent path"))?;
        let parent = self
            .agent_catalog
            .get_by_path(metadata.root_session_id.as_str(), parent_path)
            .cloned()
            .ok_or_else(|| {
                format!(
                    "child agent {session_id} parent metadata missing at {parent_path} in root {}",
                    metadata.root_session_id
                )
            })?;
        if !self.instances.contains_key(parent.agent_id.as_str()) {
            return Err(format!(
                "child agent {session_id} cannot reload: immediate parent {} is unloaded",
                parent.agent_path
            )
            .into());
        }
        let (config, _) = self.subagent_spawn_config(parent.agent_id.as_str(), session_id)?;
        self.reload_subagent_internal(session_id, parent.agent_id.as_str(), config)
    }

    pub(super) fn reload_subagent_internal(
        &mut self,
        session_id: &str,
        parent_id: &str,
        config: SubagentSpawnConfig,
    ) -> QaqhResult<()> {
        if self.instances.contains_key(session_id) {
            return Ok(());
        }
        if !config.ephemeral {
            self.sessions.set_ephemeral(session_id, false);
        }
        let parent_cancel = self.cancel_for_session(parent_id);
        self.spawn_subagent_inprocess(
            session_id,
            SubagentSpawnSpec {
                tools: config.tools,
                model: config.model,
                base_url: config.base_url,
                max_tokens: config.max_tokens,
                // reload 路径无父会话上下文，回落全局 config（fail-closed 1）。
                permission_level: qaqh_config::Config::load()
                    .ok()
                    .map(|cfg| cfg.permission_level)
                    .unwrap_or(1),
            },
            parent_cancel,
        )?;
        if let Err(error) = self.link_subagent(parent_id, session_id) {
            self.close(session_id);
            return Err(format!(
                "reload child {session_id} failed to restore parent edge: {error}"
            )
            .into());
        }
        log::info!(
            "[registry] reloaded child agent {session_id} through loaded parent {parent_id}"
        );
        Ok(())
    }

    /// T-1-4：登记父会话 → 子代理的派生关系（幂等）。
    pub(super) fn link_subagent(&mut self, parent: &str, child: &str) -> QaqhResult<()> {
        self.supervisor.link(parent, child).map_err(QaqhError::from)
    }

    /// T-1-4：父会话当前登记的子代理 seed（排序后返回，便于日志与测试）。
    pub(super) fn children_of(&self, parent: &str) -> Vec<String> {
        self.supervisor.children_of(parent)
    }

    pub(super) fn cancel_for_session(
        &self,
        _session_id: &str,
    ) -> Option<crate::agent::types::CancelToken> {
        None
    }

    /// T-1-4 测试/运维只读视图：父会话登记的子代理 seed。
    #[doc(hidden)]
    pub fn subagent_children(&self, parent: &str) -> Vec<String> {
        self.children_of(parent)
    }
}
