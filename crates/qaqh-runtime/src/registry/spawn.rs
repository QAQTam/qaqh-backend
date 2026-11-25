//! `AgentRegistry::spawn` —— 只借用父模块（`super`）的既有定义与状态，
//! 不在这里新增字段。

use super::*;

use crate::error::{QaqhError, QaqhResult};

// ADR-0014 切片 D/E：会话工具面的单一事实源——agent 面（spawn/send/wait/
// list/cancel）+ worktree 面（worktree_*，ADR-0041）。增删会话工具必须经
// `SessionToolHosts::registrars`（v4.5 P2-1b：原四张 const 注册器表随进程级
// 进程级 `OnceLock` 动态宿主槽一并删除，改为宿主束的注册期闭包构造），
// 白名单天花板仍在 assemble 的 apply_init 生效。画像矩阵：
// - 子·非 team（原 AGENT）：agent 五件 + gitx；
// - 子·team（原 TEAM）：task 五件 + gitx（留言板词表已随 ADR-0033-agent-team
//   退役，不得复活）；
// - 根·非 team（原 ROOT）：agent 五件 + gitx + goal 三件（子会话是派生瞬时体，
//   goal 只挂根，ADR-0015）；
// - 根·team（原 ROOT_TEAM）：agent + task + gitx + goal。

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

/// ADR-0041：会话 workspace 的回合边界重解析闭包。
///
/// 单一路径复用 [`ensure_session_workspace`]（含绑定 fallback 与失效响亮
/// warn）；解析失败（如用户工作区消失）返回 `None`，loop 侧保持现状。
pub(crate) fn workspace_resolver(
    owners: std::sync::Arc<qaqh_session::owner::OwnerDirectory>,
) -> qaqh_agent_runtime::session_loop::WorkspaceResolver {
    std::sync::Arc::new(move |session_id: &str| {
        ensure_session_workspace(&owners, session_id).ok()
    })
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
            // ADR-0041 决策 3/4：绑定会话的工具 workspace 动态落到 worktree 根；
            // `sessions.cwd` 保持源工作区（`.qaqh` 工件锚点）。绑定失效（worktree
            // 目录被清理）时响亮回退到 cwd，绝不静默沿用 worktree 假设。
            match qaqh_gitx::WorktreeManager::new(&path).record_for_session(session_id) {
                Ok(Some(record)) if record.path.is_dir() => return Ok(record.path),
                Ok(Some(record)) => {
                    log::warn!(
                        "session {session_id} is bound to worktree {} but it is missing; \
                         falling back to workspace root {}",
                        record.path.display(),
                        path.display()
                    );
                }
                Ok(None) => {}
                Err(error) => {
                    log::warn!(
                        "session {session_id} worktree binding lookup failed: {error}; \
                         falling back to workspace root {}",
                        path.display()
                    );
                }
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
                "subagent max depth {} reached at parent {} (depth {}); nesting depth is a kernel constant (ADR-0018)",
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
            .model_selection;
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

        // ADR-0014 切片G：真 fork —— 子会话行就绪后、loop 装配前种子化日志。
        // 子级首个回合的 assemble_context 即含父级已完成历史（无损）。
        if options.fork {
            let parent = parent_session_id.to_string();
            let child = session_id.to_string();
            let seeded = self
                .owners
                .db()
                .write_blocking(move |conn| {
                    qaqh_session::db::store::fork_session_prefix(conn, &parent, &child)
                })
                .map_err(|error| format!("fork seed for {session_id}: {error}"))?;
            log::info!("[subagent] fork seeded {seeded} completed turn(s) into {session_id}");
        }

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
        // v4.5 P2-1b：会话工具面宿主束由 `QaqhService::init` 注入（原 daemon
        // 装配期 5 个进程级 `install_*_host` 槽已物理删除）。未注入即响亮
        // 报错——fail-closed，不做无宿主降级。
        let hosts = self
            .tool_hosts
            .as_ref()
            .cloned()
            .ok_or_else(|| QaqhError::message(
                "agent registry has no session tool hosts (spawn is only available through QaqhService)"
                    .to_string(),
            ))?;
        let mut input = qaqh_agent_runtime::session_agent::assembly::SpawnInput::for_session(
            session_id,
            &agent_config,
            &session.tool_mode,
            &session.custom_tools,
            ws_dir,
            owner,
            self.exec_sessions(),
            Arc::clone(&hosts.todo),
        )?;
        // ADR-0014：sandbox 角色由血缘派生——会话行有父即子代理 sandbox。
        // ADR-0015：goal 工具面仅根会话注入。
        let is_child = self
            .owners
            .session_parent(session_id)
            .map_err(QaqhError::message)?
            .is_some();
        // ADR-0033:协作面按根 team_mode 条件注册(后代继承存活根的模式)。
        let root_session_id = if is_child {
            self.owners
                .session_parent(session_id)
                .map_err(QaqhError::message)?
                .unwrap_or_else(|| session_id.to_string())
        } else {
            session_id.to_string()
        };
        let team_mode = self
            .owners
            .db()
            .read_blocking({
                let root = root_session_id.clone();
                move |conn| {
                    Ok::<_, qaqh_session::db::DbError>(qaqh_session::db::store::session_team_mode(&conn, &root))
                }
            })
            .unwrap_or(false);
        if is_child {
            input.sandbox = qaqh_agent_runtime::session_agent::assembly::subagent_sandbox();
        }
        // ADR-0014：子代理工具面进入每个会话（spawn/send/wait/list/cancel）。
        // ADR-0014 切片E：协作工具面（task_*）同路进入每个会话——宿主经
        // `SessionToolHosts` 在注册期注入工具实例（无全局槽），板按根树键控，
        // lead 与子级共用同一注册器；白名单天花板仍在 apply_init 生效。
        let registrars = hosts.registrars(is_child, team_mode);
        input.tool_registrars = &registrars;
        let loop_ = qaqh_agent_runtime::session_agent::assembly::assemble(input)?;
        // ADR-0033：终态回传路由注入（loop 报告终态，registry 判定血缘）。
        if let Some(router) = &self.terminal_router {
            loop_.set_terminal_port(router.clone());
        }
        // ADR-0041：回合边界 workspace 重解析——绑定会话的下一回合自动落到
        // worktree 根（或解绑/移除后回到源工作区），无需 AgentReloadConfig 广播。
        loop_.set_workspace_resolver(workspace_resolver(std::sync::Arc::clone(
            &self.owners,
        )));

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_hosts::SessionToolHosts;
    use std::sync::Arc;

    // 注册冒烟桩（v4.5 P2-1b：注册器改宿主束闭包构造，无全局槽；
    // 断言只看词表，不触达执行路径）。
    struct StubSubagent;
    impl qaqh_subagent::SubagentHost for StubSubagent {
        fn spawn_subagent(
            &self,
            _r: qaqh_subagent::SpawnSubagentRequest<'_>,
        ) -> Result<qaqh_subagent::SpawnedSubagent, String> {
            Err("stub".into())
        }
        fn list_agents(
            &self,
            _c: &str,
            _p: &str,
        ) -> Result<Vec<qaqh_subagent::ListedAgent>, String> {
            Err("stub".into())
        }
        fn send_agent_message(
            &self,
            _r: qaqh_subagent::SendAgentMessageRequest<'_>,
        ) -> Result<qaqh_subagent::SentAgentMessage, String> {
            Err("stub".into())
        }
        fn wait_agent(
            &self,
            _r: qaqh_subagent::WaitAgentRequest<'_>,
        ) -> Result<qaqh_subagent::WaitAgentOutcome, String> {
            Err("stub".into())
        }
        fn interrupt_agent(
            &self,
            _r: qaqh_subagent::InterruptAgentRequest<'_>,
        ) -> Result<qaqh_subagent::InterruptedAgent, String> {
            Err("stub".into())
        }
        fn send_ringing(
            &self,
            _s: &str,
            _c: qaqh_ringing::RingingCommand,
        ) -> Result<(), String> {
            Err("stub".into())
        }
    }

    struct StubTask;
    impl qaqh_subagent::TaskBoardHost for StubTask {
        fn task_create(
            &self,
            _r: qaqh_subagent::TaskCreateRequest<'_>,
        ) -> Result<qaqh_subagent::TaskBoardTask, String> {
            Err("stub".into())
        }
        fn task_claim(
            &self,
            _r: qaqh_subagent::TaskClaimRequest<'_>,
        ) -> Result<qaqh_subagent::TaskBoardTask, String> {
            Err("stub".into())
        }
        fn task_update(
            &self,
            _r: qaqh_subagent::TaskUpdateRequest<'_>,
        ) -> Result<qaqh_subagent::TaskBoardTask, String> {
            Err("stub".into())
        }
        fn task_close(
            &self,
            _r: qaqh_subagent::TaskCloseRequest<'_>,
        ) -> Result<qaqh_subagent::TaskBoardTask, String> {
            Err("stub".into())
        }
        fn task_list(
            &self,
            _r: qaqh_subagent::TaskListRequest<'_>,
        ) -> Result<qaqh_subagent::TaskListPage, String> {
            Err("stub".into())
        }
    }

    struct StubGitx;
    impl qaqh_gitx::GitxHost for StubGitx {
        fn source_workspace(&self, _session_id: &str) -> Option<std::path::PathBuf> {
            None
        }
    }

    struct StubGoal;
    impl qaqh_subagent::goal::GoalHost for StubGoal {
        fn create_goal(
            &self,
            _s: &str,
            _o: &str,
            _b: Option<u64>,
        ) -> Result<qaqh_ringing_contract::state::GoalView, String> {
            Err("stub".into())
        }
        fn update_goal_status(
            &self,
            _s: &str,
            _st: qaqh_ringing_contract::state::GoalStatus,
        ) -> Result<qaqh_ringing_contract::state::GoalView, String> {
            Err("stub".into())
        }
        fn get_goal(
            &self,
            _s: &str,
        ) -> Result<Option<qaqh_ringing_contract::state::GoalView>, String> {
            Ok(None)
        }
    }

    fn stub_hosts() -> SessionToolHosts {
        SessionToolHosts {
            subagent: Arc::new(StubSubagent),
            task: Arc::new(StubTask),
            goal: Arc::new(StubGoal),
            gitx: Arc::new(StubGitx),
            todo: Arc::new(qaqh_workspace::todo::MemoryTodoHost::default()),
        }
    }

    fn manager_for(is_child: bool, team_mode: bool) -> qaqh_workspace::ToolManager {
        let hosts = stub_hosts();
        let registrars = hosts.registrars(is_child, team_mode);
        qaqh_workspace::registration::build_tool_manager(
            &registrars,
            Arc::clone(&hosts.todo),
        )
    }

    /// ADR-0033 闸门：基础面 = agent 五件 + worktree 面（ADR-0041）；
    /// team 开启的会话再加 task_* 五件；留言板词表不得复活。
    /// 注册器列表被削（回到悬置状态）时本测试必须红。
    #[test]
    fn team_mode_switches_the_collaboration_surface() {
        let agent = manager_for(true, false);
        for name in [
            "spawn_subagent",
            "send_message",
            "wait_agent",
            "list_agents",
            "interrupt_agent",
        ] {
            assert!(agent.lookup(name).is_some(), "{name} must be callable");
        }
        for name in ["task_create", "task_list", "board_post", "board_list"] {
            assert!(
                agent.lookup(name).is_none(),
                "{name} must be absent without team mode"
            );
        }
        for name in [
            "worktree_list",
            "worktree_create",
            "worktree_bind",
            "worktree_unbind",
            "worktree_remove",
        ] {
            assert!(agent.lookup(name).is_some(), "{name} must be callable");
        }

        let team = manager_for(true, true);
        for name in [
            "task_create",
            "task_claim",
            "task_update",
            "task_close",
            "task_list",
            "worktree_list",
            "worktree_create",
            "worktree_bind",
            "worktree_unbind",
            "worktree_remove",
        ] {
            assert!(team.lookup(name).is_some(), "{name} must be callable");
            assert!(
                team.lookup("board_post").is_none() && team.lookup("board_list").is_none(),
                "board_* must stay retired (ADR-0033)"
            );
        }

        let root_team = manager_for(false, true);
        assert!(root_team.lookup("create_goal").is_some(), "goal 仍只挂根");
        let root = manager_for(false, false);
        assert!(root.lookup("create_goal").is_some());
        assert!(root.lookup("task_create").is_none());
        assert!(root.lookup("spawn_subagent").is_some());
    }
}
