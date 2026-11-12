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
            .map_err(QaqhError::message)?;
        self.agent_catalog
            .register_root_with_alias(
                SessionId::new(session_id.to_string()),
                Some(session_id),
                created_at_ms,
            )
            .map_err(|error| error.into())
    }

    pub fn get_or_spawn(&mut self, session_id: &str) -> QaqhResult<()> {
        let owned = session_id.to_string();
        if !self
            .owners
            .db()
            .read_blocking(move |conn| qaqh_session::db::store::session_exists(conn, &owned))
            .map_err(QaqhError::message)?
        {
            return Err(QaqhError::not_found(format!(
                "session {session_id} not found"
            )));
        }
        self.ensure_agent_projection(session_id)?;
        if self.instances.contains_key(session_id) {
            return Ok(());
        }
        self.spawn_session_inprocess(session_id)?;
        Ok(())
    }

    pub fn spawn_new(&mut self, session_id: &str) -> QaqhResult<()> {
        if self.instances.contains_key(session_id) {
            return Err(format!("agent already running for {session_id}").into());
        }
        self.spawn_session_inprocess(session_id)
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
        self.ensure_agent_projection(parent_session_id)?;
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
        let parent_id = parent_session_id.to_string();
        let parent_row_selection = self
            .owners
            .db()
            .read_blocking(move |conn| qaqh_session::db::store::session_row(conn, &parent_id))
            .map_err(QaqhError::message)?
            .ok_or_else(|| QaqhError::not_found("subagent parent session not found"))?
            .model_selection
            ;
        // 显式指定的 selection 优先；否则子会话行继承父会话行的选择。
        let child_selection = options
            .selection
            .cloned()
            .or(parent_row_selection)
            .map(|value| qaqh_types::SettingValue::Use { value });
        // 业务权威先行:子会话行落 SQLite(血缘 = 父会话, cwd 一并落库)。
        self.owners
            .ensure_session(session_id, Some(parent_session_id), options.ephemeral)
            .map_err(|error| {
                format!("ensure child business session row for {session_id}: {error}")
            })?;
        // ADR-0014：工具白名单落会话行（reload 的唯一事实源）。空白名单 =
        // STANDARD（无会话级限制）；非空 = CUSTOM。档位与普通会话同律：
        // 装配时从有效 config 解析，不存在运行时继承链。
        let (tool_mode, custom_tools) = if options.tools.is_empty() {
            (qaqh_types::tool_mode::STANDARD.to_string(), Vec::new())
        } else {
            (
                qaqh_types::tool_mode::CUSTOM.to_string(),
                options.tools.to_vec(),
            )
        };
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
                            title_source: None,
                            cwd: Some(cwd),
                            model: None,
                            tool_mode: Some(tool_mode),
                            model_selection: child_selection,
                            custom_tools: Some(custom_tools),
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
            options.ephemeral,
        )?;
        Ok(SpawnedSubagentInfo {
            child_session_id: SessionId::new(session_id.to_string()),
            parent_agent_path: parent.agent_path,
            child_agent_path: child_path,
        })
    }

    pub(super) fn spawn_subagent_internal(
        &mut self,
        session_id: &str,
        parent_session: &str,
        identity: Option<(AgentMetadata, AgentPath)>,
        child_session_id: SessionId,
        ephemeral: bool,
    ) -> QaqhResult<()> {
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
        // ADR-0014：子代理就是会话——同一条装配路径，sandbox 由血缘派生。
        if let Err(error) = self.spawn_session_inprocess(session_id) {
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

    pub(super) fn spawn_session_inprocess(&mut self, session_id: &str) -> QaqhResult<()> {
        if self.instances.contains_key(session_id) {
            return Err(format!("agent already running for {session_id}").into());
        }

        let agent_config = qaqh_agent_runtime::agent::session_config::session_effective_config(
            &self.owners,
            session_id,
        )
        .map_err(QaqhError::message)?;

        let ws_dir = ensure_session_workspace(&self.owners, session_id)?;
        let session = crate::session_meta::read(&self.owners, session_id)
            .ok_or_else(|| format!("session metadata unavailable for {session_id}"))?;
        let owner = self.owners.get_or_create(session_id);
        let mut input = qaqh_agent_runtime::session_agent::assembly::SpawnInput::for_session(
            session_id,
            &agent_config,
            &session.tool_mode,
            &session.custom_tools,
            ws_dir,
            owner,
            self.exec_sessions(),
        )?;
        // ADR-0014：sandbox 角色由血缘派生——会话行有父即子代理 sandbox。
        if self
            .owners
            .session_parent(session_id)
            .map_err(QaqhError::message)?
            .is_some()
        {
            input.sandbox = qaqh_agent_runtime::session_agent::assembly::subagent_sandbox();
        }
        // ADR-0014：子代理工具面进入每个会话（spawn/send/wait/list/cancel）。
        input.tool_registrars = &[qaqh_subagent::register];
        let loop_ = qaqh_agent_runtime::session_agent::assembly::assemble(input)?;
        // ADR-0014：终态回传路由注入（loop 报告终态，registry 判定血缘）。
        if let Some(router) = &self.terminal_router {
            loop_.set_terminal_port(router.clone());
        }

        self.instances.insert(
            session_id.to_string(),
            AgentInstance {
                session_id: session_id.to_string(),
                loop_,
            },
        );
        self.set_agent_residency(session_id, ListedAgentResidency::Loaded);
        log::info!("[session] spawned session loop for session={session_id}");
        Ok(())
    }

    /// ADR-0014：子代理就是会话——冷会话一律走普通 get-or-spawn。
    /// 血缘在会话行里，reload 不需要父加载，也不需要内存 spawn 档案。
    pub(super) fn ensure_loaded_for_command(&mut self, session_id: &str) -> QaqhResult<()> {
        self.get_or_spawn(session_id)
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
