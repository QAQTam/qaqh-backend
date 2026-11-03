//! `AgentRegistry::spawn` —— 只借用父模块（`super`）的既有定义与状态，
//! 不在这里新增字段。

use super::*;

use crate::error::{QaqhError, QaqhResult};

/// 会话绑定的 cwd(业务库 `sessions.cwd`),空串/`.` 视为未绑定。
pub(crate) fn session_cwd(
    owners: &qaqh_session::owner::OwnerDirectory,
    session_id: &str,
) -> Option<String> {
    let owned = session_id.to_string();
    owners
        .db()
        .read_blocking(move |conn| qaqh_session::db::store::session_row(conn, &owned))
        .ok()
        .flatten()
        .and_then(|row| row.cwd)
        .filter(|cwd| !cwd.is_empty() && cwd != ".")
}

/// Resolve the directory a session's tools run in.
///
/// A session without a bound workspace gets the default session workspace,
/// created on demand. That exact path is what the model is told about, what the
/// sandbox receives as a writable root and what the process starts in, so it has
/// to exist before the first `exec` — otherwise the sandbox preparation fails.
/// A workspace the user picked must already exist; creating an empty directory
/// with the same name would silently hide a vanished project.
pub(crate) fn ensure_session_workspace(
    owners: &qaqh_session::owner::OwnerDirectory,
    session_id: &str,
) -> Result<std::path::PathBuf, String> {
    let bound = session_cwd(owners, session_id);
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
        self.owners
            .ensure_session(session_id, None, false)
            .map_err(|error| format!("ensure business session row for {session_id}: {error}"))?;
        self.agent_catalog
            .register_root_with_alias(
                SessionId::new(session_id.to_string()),
                Some(session_id),
                created_at_ms,
            )
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
            return self.spawn_subagent_internal(
                session_id,
                &parent_session,
                None,
                SessionId::new(session_id.to_string()),
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

        // Explicit request first: the child business row must carry the path
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
            None => session_cwd(&self.owners, session_id)
                .or_else(|| session_cwd(&self.owners, parent_session_id))
                .unwrap_or_else(|| {
                    // 父子会话都没有工作区：落默认工作根（与
                    // `load_session_workspace` 同源），绝不兜底进程 cwd——
                    // daemon 的进程 cwd 是安装目录。
                    qaqh_types::platform::default_session_workspace_dir(session_id)
                        .to_string_lossy()
                        .into_owned()
                }),
        };
        // 业务权威先行:子会话行落 SQLite(血缘 = 父会话, cwd 一并落库)。
        self.owners
            .ensure_session(session_id, Some(parent_session_id), options.ephemeral)
            .map_err(|error| {
                format!("ensure child business session row for {session_id}: {error}")
            })?;
        self.owners
            .db()
            .write_blocking({
                let owned = session_id.to_string();
                move |conn| {
                    qaqh_session::db::store::update_session_columns(
                        conn,
                        &owned,
                        &qaqh_session::db::store::SessionColumnPatch {
                            title: None,
                            cwd: Some(cwd),
                            model: None,
                            effort: None,
                            profile: None,
                            mode: None,
                            tool_mode: None,
                            model_selection: None,
                            custom_tools: None,
                            skills: None,
                            frozen_annotation: None,
                            archived: None,
                            ephemeral: None,
                            expected_settings_revision: None,
                            increment_settings_revision: false,
                        },
                    )
                }
            })
            .map_err(|error| format!("persist child cwd for {session_id}: {error}"))?;

        let identity = Some((parent.clone(), child_path.clone()));
        self.spawn_subagent_internal(
            session_id,
            parent_session_id,
            identity,
            SessionId::new(session_id.to_string()),
            options,
        )?;
        Ok(SpawnedSubagentInfo {
            child_session_id: SessionId::new(session_id.to_string()),
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
            self.owners
                .set_ephemeral(session_id, false)
                .map_err(|error| {
                    format!("persist subagent persistence for {session_id}: {error}")
                })?;
        }
        let root_session = if parent_session.is_empty() {
            session_id.to_string()
        } else {
            self.supervisor.root_of(parent_session)
        };
        // Durable reservation must exist before the child actor can perform
        // any side effect.
        let reservation = self.reserve_spawn(&root_session, session_id)?;
        if let Err(error) = self.spawn_subagent_inprocess(
            session_id,
            SubagentSpawnSpec {
                tools: tools.to_vec(),
                model: model.map(str::to_string),
                base_url: base_url.map(str::to_string),
                max_tokens,
                permission_level,
            },
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
    ) -> QaqhResult<()> {
        if self.instances.contains_key(session_id) {
            return Err(format!("agent already running for {session_id}").into());
        }
        self.last_spawn
            .insert(session_id.to_string(), std::time::Instant::now());

        let (mut agent_config, _session_profile) =
            qaqh_agent_runtime::agent::session_config::session_effective_config(
                Some(&self.owners),
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

        let ws_dir = ensure_session_workspace(&self.owners, session_id)?;
        let owner = self.owners.get_or_create(session_id);
        let loop_ = qaqh_agent_runtime::session_agent::assembly::assemble(
            qaqh_agent_runtime::session_agent::assembly::SpawnInput {
                session_id,
                owner,
                config: &agent_config,
                allowed_tools: spec.tools.clone(),
                global_allowed_tools: None,
                permission_level: spec.permission_level,
                sandbox: qaqh_agent_runtime::session_agent::assembly::subagent_sandbox(),
                workspace_root: ws_dir,
            },
        )?;

        self.instances.insert(
            session_id.to_string(),
            AgentInstance {
                session_id: session_id.to_string(),
                transport: AgentTransport::Loop { loop_ },
                kind: AgentKind::Subagent(spec.clone()),
            },
        );
        self.spawn_configs.insert(session_id.to_string(), spec);
        self.set_agent_residency(session_id, ListedAgentResidency::Loaded);
        if let Some(hub) = self.hub.as_ref() {
            hub.mark_worker_live(session_id);
        }
        log::info!("[subagent] spawned session loop for subagent={session_id}");
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
        // 业务权威登记:根会话行不存在则补默认行(create_new 直 spawn 的路径)。
        self.owners
            .ensure_session(session_id, None, false)
            .map_err(|error| format!("ensure business session row for {session_id}: {error}"))?;

        let (agent_config, _session_profile) =
            qaqh_agent_runtime::agent::session_config::session_effective_config(
                Some(&self.owners),
                session_id,
            )
            .unwrap_or_default();

        let ws_dir = ensure_session_workspace(&self.owners, session_id)?;
        let session = crate::session_meta::read(&self.owners, session_id)
            .ok_or_else(|| format!("session metadata unavailable for {session_id}"))?;
        let owner = self.owners.get_or_create(session_id);
        let loop_ = qaqh_agent_runtime::session_agent::assembly::assemble(
            qaqh_agent_runtime::session_agent::assembly::SpawnInput::for_session(
                session_id,
                &agent_config,
                &session.tool_mode,
                &session.custom_tools,
                ws_dir,
                owner,
            )?,
        )?;

        self.instances.insert(
            session_id.to_string(),
            AgentInstance {
                session_id: session_id.to_string(),
                transport: AgentTransport::Loop { loop_ },
                kind: AgentKind::Session,
            },
        );
        self.set_agent_residency(session_id, ListedAgentResidency::Loaded);
        log::info!("[session] spawned session loop for session={session_id}");
        Ok(())
    }

    /// Materialize the delivery target without turning a logical child into a
    /// root session. Roots use the ordinary get-or-spawn path; children reload
    /// only through a loaded immediate parent and reuse the spawn config
    /// recorded by the registry at spawn time.
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
        let spec = self.spawn_configs.get(session_id).cloned().ok_or_else(|| {
            format!("child agent {session_id} has no recorded spawn config to reload")
        })?;
        self.reload_subagent_internal(session_id, parent.agent_id.as_str(), spec)
    }

    pub(super) fn reload_subagent_internal(
        &mut self,
        session_id: &str,
        parent_id: &str,
        spec: SubagentSpawnSpec,
    ) -> QaqhResult<()> {
        if self.instances.contains_key(session_id) {
            return Ok(());
        }
        self.owners
            .set_ephemeral(session_id, false)
            .map_err(|error| format!("persist reload persistence for {session_id}: {error}"))?;
        self.spawn_subagent_inprocess(session_id, spec)?;
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

    /// T-1-4 测试/运维只读视图：父会话登记的子代理 seed。
    #[doc(hidden)]
    pub fn subagent_children(&self, parent: &str) -> Vec<String> {
        self.children_of(parent)
    }
}
