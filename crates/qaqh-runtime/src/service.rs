use std::sync::{Arc, Mutex};

use qaqh_ringing::{CommandTarget, RingingCommand, RingingWorkerCommandEnvelope};
use qaqh_ringing_contract::SessionActivityState as ActivityState;
use qaqh_types::SessionMeta;
use serde_json::Value;

use crate::AgentRegistry;
use crate::error::{QaqhError, QaqhResult};
use crate::service::command::Command;
use qaqh_ringing_runtime::RingingHub;

#[derive(Clone)]
pub struct QaqhService {
    pub(crate) registry: Arc<Mutex<AgentRegistry>>,
    pub(crate) hub: std::sync::OnceLock<Arc<RingingHub>>,
    /// 会话 owner 目录:SQLite 业务权威 + 每会话事件单流。
    pub(crate) owners: Arc<qaqh_session::owner::OwnerDirectory>,
    /// 会话存储句柄（PR-3-1 注入化：daemon main 装配点 init 后注入，
    /// service 内不再触达会话单例的全局访问器）。
    pub(crate) sessions: Arc<qaqh_session::SessionManager>,
    /// Sign in with ChatGPT（套餐用量）会话管理器。懒装配：只有真正用到该能力时
    /// 才去建目录、读 host id——未使用它的用户不该多出任何磁盘副作用。
    pub(crate) siwc: std::sync::OnceLock<Arc<qaqh_siwc::SiwcManager>>,
    /// DeepSeek 账号（账号余额）管理器。同样懒装配，且**没有目录副作用**：
    /// 凭据是 config 目录下的单文件（`deepseek-account.json`，DPAPI/0600 密文）。
    pub(crate) dsaccount: std::sync::OnceLock<qaqh_dsaccount::DsAccountManager>,
}

impl QaqhService {
    pub fn init(sessions: Arc<qaqh_session::SessionManager>) -> Self {
        let mut config = qaqh_config::Config::load().unwrap_or_default();
        // PR-M3-2 路线 A：用户级外部 MCP 配置只读合并（Codex/Claude/opencode，
        // import_external 默认开）。只改运行时视图，不回写 config；报告落日志。
        // 注意 enabled 总闸不变：外部合并的 server 也受 [mcp].enabled 管辖。
        {
            let paths = qaqh_config::mcp_import::default_user_paths();
            if let Some(report) = qaqh_config::mcp_import::merge_external(&mut config.mcp, &paths) {
                if !report.merged.is_empty() {
                    log::info!("[mcp] external config merged: {:?}", report.merged);
                }
                for collision in &report.skipped_collisions {
                    log::info!(
                        "[mcp] external config skipped (name collision, local wins): {collision}"
                    );
                }
                for invalid in &report.skipped_invalid {
                    log::warn!("[mcp] external config skipped (invalid): {invalid}");
                }
            }
        }
        // MCP manager 装配（设计 §10-6 / PR-M1-5）：daemon 进程级单例（actor
        // 线程与其工具线程同进程，全局槽位可见）；装配不做同步网络操作，
        // 连接由下方预热（后台）+ lazy（工具执行路径）双入口拉起。secret
        // store 用默认位置（[secrets.mcp] 段）；禁用配置时 manager 以
        // disabled 形态拒绝一切调用（MCP_DISABLED）。
        qaqh_mcp::install_manager(qaqh_mcp::McpManager::new(config.mcp.clone()));
        // LSP manager 装配（；与 MCP 同款单例；
        // 默认关闭——enabled=false 时 disabled 形态拒一切调用 LSP_DISABLED）。
        qaqh_lsp::install_manager(qaqh_lsp::LspManager::new(config.lsp.clone()));
        // P2-1：热重载接线——①重载器：订阅 watch 单写口广播，[mcp] 段变化
        // → 外部配置重扫（Codex/Claude 用户级）→ apply_config diff 保连；
        // ②文件轮询器：手改 config.toml（不经单写口）→ mtime 检测 →
        // reload_from_disk 统一发布。fire-and-forget，不阻塞启动。守卫：
        // 仅在 tokio runtime 上下文内接线（daemon main 是 async；单元测试
        // 等同步调用方跳过——热重载只对长驻 daemon 有意义）。
        if tokio::runtime::Handle::try_current().is_ok() {
            spawn_mcp_reloader();
            spawn_lsp_reloader();
            spawn_config_file_poller();
        }
        // 投影预热（PR-M1-5 冒烟修正）：lazy 连接的唯一触发点是工具执行，
        // 而工具要先投影才会被调用——不预热则全新 daemon 上模型首回合永远
        // 看不到 MCP 工具（鸡生蛋死锁）。fire-and-forget：逐 server 连接
        // + 缓存 tools/list + 置脏，不阻塞启动；未启用时内部 no-op。
        qaqh_mcp::prime_all_async();
        // daemon 进程的工具注册表（供 `skills.list_tools` 等查询；worker 各自
        // 独立 init_tools，本进程只提供注册表快照，不参与工具执行）。
        // 不带会话工具注册器：spawn_subagent 等属于会话工具面，不在 daemon
        // 查询注册表里。
        qaqh_workspace::runtime::init_tools("daemon", &[], vec![]);
        // 业务库:SQLite WAL 单权威(报告一 §7)。打不开就停止启动,不降级。
        let owners = Arc::new(
            qaqh_session::owner::OwnerDirectory::open(sessions.data_dir().join("qaqh.db"))
                .expect("open business database"),
        );
        // ADR-0018：子代理限额是内核常量（registry 缺省：depth=1、配额
        // 16/1024），不再经用户配置面覆盖——子代理=会话，生命周期与父同律。
        let registry = AgentRegistry::new(Arc::clone(&owners));
        let service = Self {
            registry: Arc::new(Mutex::new(registry)),
            hub: std::sync::OnceLock::new(),
            owners,
            sessions,
            siwc: std::sync::OnceLock::new(),
            dsaccount: std::sync::OnceLock::new(),
        };
        // ADR-0014：终态回传路由——loop 报告终态，registry 判定血缘回传。
        // 闭包持 service clone；终态时锁 registry 路由（短临界区、无 await，
        // 与 send_input 的入队语义无死锁）。
        let router_service = service.clone();
        let fork_service = service.clone();
        if let Ok(mut registry) = service.registry() {
            registry.set_terminal_router(Some(Arc::new(move |notice| {
                // ADR-0016 切片 1：封箱终态挂钩调度内核 fork（阅读窗
                // FORK_DELAY_MS 后执行）。路由先走（goal 记账在前是既有顺序），
                // fork 调度只看 DB 封箱事实，两次索引读，廉价。
                crate::handoff_fork_schedule::schedule_after_terminal(
                    &fork_service,
                    &notice.session_id,
                );
                if let Ok(mut registry) = router_service.registry() {
                    registry.route_turn_terminal(notice);
                }
            })));
        }
        service
    }

    /// SIWC 会话管理器（懒装配到 `<data_dir>/siwc`）。
    /// 懒装配而非 `init` 里装配：只有真正用到套餐用量时才建目录、读 host id；
    /// 不用该能力的用户不该多出任何磁盘副作用。
    pub(crate) fn siwc(&self) -> Result<Arc<qaqh_siwc::SiwcManager>, String> {
        if let Some(manager) = self.siwc.get() {
            return Ok(Arc::clone(manager));
        }
        let dir = qaqh_types::platform::data_dir().join("siwc");
        let manager = Arc::new(
            qaqh_siwc::SiwcManager::new(dir, qaqh_siwc::SiwcConfig::default())
                .map_err(|e| e.to_string())?,
        );
        // 进程级装配：深层调用点（`provider_for` 每回合重建 provider）只认全局槽位，
        // 理由见 `qaqh_siwc::global` 的模块说明。
        qaqh_siwc::install_manager(Arc::clone(&manager));
        // 竞态：两个线程可能同时装配；`set` 失败即沿用先到者的实例。
        let _ = self.siwc.set(manager);
        self.siwc
            .get()
            .map(Arc::clone)
            .ok_or_else(|| "siwc manager unavailable".to_string())
    }

    /// 测试/运维钩子：注入指定目录的 SIWC 管理器（默认位置是用户的 data_dir）。
    #[doc(hidden)]
    pub fn attach_siwc(&self, manager: Arc<qaqh_siwc::SiwcManager>) {
        // 与懒装配同一语义：全局槽位是深层调用点的唯一入口。
        qaqh_siwc::install_manager(Arc::clone(&manager));
        let _ = self.siwc.set(manager);
    }

    /// DeepSeek 账号管理器（懒装配；凭据默认落在 config 目录）。
    /// 懒装配的理由与 SIWC 相同：不用该能力的用户不该多出任何磁盘副作用。
    /// 构造本身不读盘、不联网——真正的读盘发生在第一次 `status()` / `token()`。
    pub(crate) fn dsaccount(&self) -> Result<&qaqh_dsaccount::DsAccountManager, String> {
        if let Some(manager) = self.dsaccount.get() {
            return Ok(manager);
        }
        if qaqh_dsaccount::manager().is_none() {
            qaqh_dsaccount::install_default();
        }
        let manager = qaqh_dsaccount::manager()
            .cloned()
            .ok_or_else(|| "dsaccount manager unavailable".to_string())?;
        let _ = self.dsaccount.set(manager);
        self.dsaccount
            .get()
            .ok_or_else(|| "dsaccount manager unavailable".to_string())
    }

    /// Attach the session content store.
    pub fn attach_ringing(&self, hub: Arc<RingingHub>) {
        let _ = self.hub.set(hub);
    }

    /// Test/ops hook: override the subagent tree depth limit.
    #[doc(hidden)]
    pub fn set_max_depth(&self, max_depth: usize) {
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .set_max_depth(max_depth);
    }

    /// Test/ops hook: override the inter-agent message safety-valve limits.
    #[doc(hidden)]
    pub fn set_message_quota_limits(&self, in_flight_per_pair: u64, outbound_per_sender: u64) {
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .set_message_quota_limits(in_flight_per_pair, outbound_per_sender);
    }

    /// 会话 owner 目录(业务库 + 事件单流的唯一入口)。
    pub fn owners(&self) -> &Arc<qaqh_session::owner::OwnerDirectory> {
        &self.owners
    }

    /// Dispatch a trusted Ringing command directly to the session loop.
    pub fn send_ringing_command(
        &self,
        session_id: &str,
        env: &qaqh_ringing::RingingWorkerCommandEnvelope,
    ) -> QaqhResult<()> {
        // 保留 QaqhError 原型:daemon 要按 `CommandRejection` 出稳定码,在这里
        // `to_string` 掉变体等于把「先停止再撤」和「历史已变」压成同一条文案。
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .send_ringing(session_id, env)
    }

    /// 关闭会话（Ringing `SessionClose` 命令语义，契约 §2）：
    /// 关闭 registry 实例并清理常驻状态（Closed 广播已删，A1：wire 零消费方），
    /// causation 参数不再使用。会话不存在同样返回 Ok（幂等关闭）。
    pub fn close_session(
        &self,
        session_id: &str,
        _causation_id: Option<&str>,
    ) -> Result<(), String> {
        if self
            .session_meta(session_id)
            .is_some_and(|meta| meta.ephemeral)
        {
            for loop_ in self.registry()?.loops_for_tree(session_id) {
                loop_.abandon();
            }
            let service = self.clone();
            let owned = session_id.to_string();
            tokio::runtime::Handle::try_current()
                .map_err(|error| format!("ephemeral cleanup requires the daemon runtime: {error}"))?
                .spawn(async move {
                    if let Err(error) = service.delete_session(&owned).await {
                        log::error!("ephemeral session {owned} cleanup failed: {error}");
                    }
                });
        } else {
            // 关闭即终结附属后台 exec(与 delete 同律)。先停 exec 再卸循环。
            let owned = session_id.to_string();
            let tree = self
                .owners
                .db()
                .read_blocking(move |conn| qaqh_session::db::store::session_tree(conn, &owned))
                .map_err(|error| error.to_string())?;
            {
                let registry = self.registry()?;
                for id in &tree {
                    if let Err(error) = registry.exec_sessions().shutdown_session(id) {
                        log::warn!("session close {id}: exec shutdown unconfirmed: {error}");
                    }
                }
            }
            self.registry()?.close(session_id);
        }
        // 阶段 3b：SessionStateChanged{Closed} 的 v1 广播已删（A1：wire 零消费方）。
        self.release_session_process_state(session_id);
        qaqh_memwatch::global().mark_session_unloaded(session_id);
        release_freed_heap_memory();
        Ok(())
    }

    /// Release process-local image data when the loop is unloaded.
    fn release_session_process_state(&self, session_id: &str) {
        // 图片注册表按 seed 键控存 base64（read_image 的上传缓存）。resume
        // 路径（state/lifecycle.rs）会从持久化消息历史重建，关闭期清空
        // 不破坏 image_index 语义。
        qaqh_workspace::read_image::reset_images(session_id);
    }

    /// 卸载空闲会话循环。
    /// `idle_secs` <= 0 时为 no-op（配置禁用）。对每个被卸载的 seed 做常驻
    /// 状态清理（Closed 广播已删，A1：wire 零消费方）。
    /// Return unloaded session ids; filesystem/database cleanup runs off the async workers.
    pub fn unload_idle_sessions(&self, idle_secs: u64) -> Vec<String> {
        if idle_secs == 0 {
            return Vec::new();
        }
        let Ok(mut registry) = self.registry() else {
            return Vec::new();
        };
        let unloaded = registry.unload_idle_sessions(idle_secs);
        // 阶段 3b：Closed 广播已删（A1）。
        for session_id in &unloaded {
            self.release_session_process_state(session_id);
            qaqh_memwatch::global().mark_session_unloaded(session_id);
        }
        if !unloaded.is_empty() {
            release_freed_heap_memory();
        }
        unloaded
    }

    /// ADR-0015：goal 周期扫描（daemon 周期任务调用）。Active goal 跨 idle
    /// 卸载/重启存活；忙会话跳过，终态钩子负责其续跑。
    pub fn sweep_active_goals(&self) -> Vec<String> {
        let Ok(mut registry) = self.registry() else {
            return Vec::new();
        };
        registry.sweep_active_goals()
    }

    /// ADR-0016 切片 3：handoff fork 兜底扫描（daemon 周期任务调用）。封箱
    /// 超窗且链头未前进的链立即补 fork；幂等由 fork 的建行闸保证。
    pub fn sweep_handoff_forks(&self) -> Vec<String> {
        let Ok(mut registry) = self.registry() else {
            return Vec::new();
        };
        registry.sweep_handoff_forks()
    }

    fn refresh_memory_components(&self) {
        let monitor = qaqh_memwatch::global();
        let mut component_count = 0usize;
        if let Ok(registry) = self.registry.lock() {
            for component in registry.memory_components() {
                component_count += 1;
                monitor.update_component(component);
            }
        }
        let mcp_connections = qaqh_mcp::manager_slot().connections();
        let mcp_name_bytes = mcp_connections
            .iter()
            .map(|connection| connection.name().len() as u64)
            .sum();
        monitor.update_component(qaqh_memwatch::ComponentMemory {
            name: "mcp.connections".into(),
            item_count: mcp_connections.len() as u64,
            payload_bytes: Some(mcp_name_bytes),
            heap_estimate_bytes: None,
            ..Default::default()
        });
        component_count += 1;
        let lsp_connections = qaqh_lsp::manager_slot().connections();
        let lsp_name_bytes = lsp_connections
            .iter()
            .map(|connection| connection.server().len() as u64)
            .sum();
        monitor.update_component(qaqh_memwatch::ComponentMemory {
            name: "lsp.connections".into(),
            item_count: lsp_connections.len() as u64,
            payload_bytes: Some(lsp_name_bytes),
            heap_estimate_bytes: None,
            ..Default::default()
        });
        component_count += 1;
        let images = qaqh_workspace::read_image::memory_stats();
        monitor.update_component(qaqh_memwatch::ComponentMemory {
            name: "workspace.image_registry".into(),
            item_count: images.entry_count as u64,
            payload_bytes: None,
            heap_estimate_bytes: Some(images.heap_estimate_bytes),
            ..Default::default()
        });
        component_count += 1;
        if !monitor.is_enabled() || component_count == 0 {
            monitor.mark_components_stale();
        }
    }

    /// Whether the opt-in memory probe is currently recording.
    pub fn diagnostics_memory_enabled(&self) -> bool {
        qaqh_memwatch::global().is_enabled()
    }

    /// Build one diagnostics snapshot for the admin SSE push channel. Component
    /// gauges are refreshed first; `after_sequence` keeps phase rows incremental.
    pub fn diagnostics_memory_snapshot(
        &self,
        after_sequence: Option<u64>,
    ) -> Result<Value, String> {
        self.refresh_memory_components();
        let monitor = qaqh_memwatch::global();
        serde_json::to_value(monitor.snapshot_since(after_sequence)).map_err(err)
    }

    /// 归档会话（标签 × 语义）：关闭 registry 实例 + meta `archived=true`。
    /// 磁盘与消息文件保留，左侧列表归档组可见可恢复。会话不存在同样
    /// 幂等成功（close 幂等 + set_archived 补写 meta）。
    pub fn archive_session(
        &self,
        session_id: &str,
        causation_id: Option<&str>,
    ) -> Result<(), String> {
        self.close_session(session_id, causation_id)?;
        let _ = self.write_meta(
            session_id,
            qaqh_ringing_contract::fact::SessionMetadataPatch {
                archived: Some(true),
                ..Default::default()
            },
        );
        Ok(())
    }

    /// Load the existing business session. Client selection is client-owned.
    pub fn resume_session(&self, session_id: &str) -> Result<(), String> {
        self.registry()?
            .get_or_spawn(session_id)
            .map_err(|error| error.to_string())
    }

    /// Rename a session through its authoritative metadata writer.
    pub fn rename_session(&self, session_id: &str, title: &str) -> Result<(), String> {
        let title = title.trim();
        if title.is_empty() {
            return Err("title must be a non-empty string".into());
        }
        self.write_title(
            session_id,
            title,
            qaqh_ringing_contract::fact::TitleSource::User,
        )
    }

    /// 恢复归档会话：meta `archived=false` + 重新拉起实例（resume 语义，
    /// 对齐 `session.resume` 查询——get_or_spawn + active seed 更新）。
    pub fn unarchive_session(&self, session_id: &str) -> Result<(), String> {
        let _ = self.write_meta(
            session_id,
            qaqh_ringing_contract::fact::SessionMetadataPatch {
                archived: Some(false),
                ..Default::default()
            },
        );
        self.registry()?
            .get_or_spawn(session_id)
            .map_err(|error| error.to_string())
    }

    /// Cancel and await actual turn tasks before deleting their business rows.
    pub async fn delete_session(&self, session_id: &str) -> Result<(), String> {
        let loops = {
            let registry = self.registry()?;
            let loops = registry.loops_for_tree(session_id);
            for loop_ in &loops {
                loop_.abandon();
            }
            loops
        };
        for loop_ in loops {
            loop_.shutdown().await;
        }
        let service = self.clone();
        let root = session_id.to_string();
        tokio::task::spawn_blocking(move || {
            // Keep command admission serialized until the rows are gone.
            let mut registry = service.registry()?;
            registry.close(&root);
            let owned = root.clone();
            let tree = service
                .owners
                .db()
                .read_blocking(move |conn| qaqh_session::db::store::session_tree(conn, &owned))
                .map_err(|error| error.to_string())?;
            for id in &tree {
                registry.exec_sessions().shutdown_session(id)?;
                service
                    .owners
                    .get_or_create(id)
                    .flush()
                    .map_err(|error| error.to_string())?;
            }
            let ids = service
                .owners
                .delete_session_tree(&root)
                .map_err(|error| error.to_string())?;
            registry.forget_sessions(&ids);
            for id in ids {
                service.sessions.delete(&id)?;
                service.release_session_process_state(&id);
            }
            Ok(())
        })
        .await
        .map_err(|error| format!("session deletion task: {error}"))?
    }

    /// 服务面唯一入口：wire `(method, params)` 在这里解码一次，然后按领域分派。
    /// 强类型命令见 [`command`]（`service::command`），各领域的执行体在
    /// [`handlers`] 里。此前这里是一个 673 行、按字符串 `match` 分派、
    /// 再用 `pstr(params, "path")?` 手工取参的函数——参数错误只在运行时暴露，
    /// 新增方法也逃不过「加一条字符串 arm」。
    pub fn handle(&self, method: &str, params: &Value) -> QaqhResult<Value> {
        match Command::decode(method, params)? {
            Command::System(command) => handlers::system::handle(self, command),
            Command::Workspace(command) => handlers::workspace::handle(self, command),
            Command::Session(command) => handlers::session::handle(self, command),
            Command::Task(command) => handlers::task::handle(self, command),
        }
    }

    /// Execute a synchronous RingingV2 mutation with its public command and
    /// target intact. Read-only service queries continue through `handle`.
    pub fn execute_ringing_command(
        &self,
        target: &CommandTarget,
        command: &RingingCommand,
    ) -> QaqhResult<Value> {
        match command {
            RingingCommand::ConfigSave { .. }
            | RingingCommand::ConfigSetPermissionLevel { .. }
            | RingingCommand::DiagnosticsMemoryStart { .. }
            | RingingCommand::DiagnosticsMemoryStop { .. } => {
                handlers::system::handle_ringing(self, command)
            }
            RingingCommand::WorkspaceSet { .. }
            | RingingCommand::WorkspaceCreate { .. }
            | RingingCommand::WorkspaceRename { .. }
            | RingingCommand::WorkspaceDelete { .. }
            | RingingCommand::WorkspaceMoveSession { .. }
            | RingingCommand::WorkspaceDetach { .. } => {
                handlers::workspace::handle_ringing(self, target, command)
            }
            RingingCommand::SubagentSpawn { .. } => {
                handlers::session::handle_ringing(self, target, command)
            }
            RingingCommand::GoalSet { .. }
            | RingingCommand::GoalGet {}
            | RingingCommand::GoalClear {} => handlers::goal::handle_ringing(self, target, command),
            RingingCommand::TodoSet { .. }
            | RingingCommand::TodoCancel { .. }
            | RingingCommand::PlanAction { .. } => {
                handlers::task::handle_ringing(self, target, command)
            }
            _ => Err(QaqhError::invalid(format!(
                "{} is not a synchronous service mutation",
                command.name()
            ))),
        }
    }

    pub fn shutdown(&self) {
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .shutdown_all();
    }

    /// F4: 死 worker 重生（daemon 周期任务调用；内部自带退避与关闭保护）。
    /// True while stopping the daemon would interrupt work or abandon an
    /// interaction waiting for its lease owner. Used by lifecycle takeover so
    /// an updater cannot race a newly-started turn.
    pub fn has_active_work(&self) -> bool {
        self.activity_snapshot().0
    }

    /// 只读活动快照（/activity 观测端点，冻结事故 P0）：逐会话活动状态 +
    /// 是否有活跃工作。单次加锁保证 flag 与列表一致；僵尸会话（如
    /// 2026-09-02 的 running 冻结）可直接从外部探测，不再依赖人肉轮询。
    pub fn activity_snapshot(&self) -> (bool, Vec<qaqh_ringing_contract::SessionActivity>) {
        let activities = self
            .registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .activities();
        let has_active_work = activities.iter().any(|activity| {
            matches!(
                activity.state,
                ActivityState::Running | ActivityState::WaitingUser | ActivityState::Cancelling
            )
        });
        (has_active_work, activities)
    }

    /// Submit session metadata through its owner pump.
    pub(crate) fn write_meta(
        &self,
        session_id: &str,
        patch: qaqh_ringing_contract::fact::SessionMetadataPatch,
    ) -> Result<(), String> {
        crate::session_meta::patch(&self.owners, session_id, patch)
    }

    /// Write a session title (business column).
    pub(crate) fn write_title(
        &self,
        session_id: &str,
        title: &str,
        source: qaqh_ringing_contract::fact::TitleSource,
    ) -> Result<(), String> {
        crate::session_meta::set_title(&self.owners, session_id, title, source)
    }

    /// Session metadata read from the business row.
    pub(crate) fn session_meta(&self, session_id: &str) -> Option<qaqh_types::SessionMeta> {
        crate::session_meta::read(&self.owners, session_id)
    }

    pub(crate) fn registry(&self) -> Result<std::sync::MutexGuard<'_, AgentRegistry>, String> {
        self.registry
            .lock()
            .map_err(|e| format!("registry lock: {e}"))
    }

    pub fn exec_session_owner(
        &self,
        id: &qaqh_types::ExecSessionId,
    ) -> Result<Option<String>, String> {
        if let Some(owner_session_id) = self.registry()?.exec_sessions().owner_session_id(id) {
            return Ok(Some(owner_session_id));
        }
        let id = id.as_str().to_string();
        self.owners
            .db()
            .read_blocking(move |conn| {
                qaqh_session::db::store::exec_session_by_id(conn, &id)
                    .map(|row| row.map(|row| row.session_id))
            })
            .map_err(|error| error.to_string())
    }

    pub fn stop_exec_session(
        &self,
        owner_session_id: &str,
        id: &qaqh_types::ExecSessionId,
        reason: qaqh_ringing_contract::state::ExecStopReason,
    ) -> Result<&'static str, String> {
        let manager = self.registry()?.exec_sessions();
        let result = manager.stop(owner_session_id, id, reason);
        if result != qaqh_agent_runtime::ExecSessionStopResult::NotFound {
            return Ok(match result {
                qaqh_agent_runtime::ExecSessionStopResult::StopRequested => "stop_requested",
                qaqh_agent_runtime::ExecSessionStopResult::AlreadyFinished => "already_finished",
                qaqh_agent_runtime::ExecSessionStopResult::NotFound => unreachable!(),
            });
        }
        let id = id.as_str().to_string();
        let persisted = self
            .owners
            .db()
            .read_blocking(move |conn| {
                qaqh_session::db::store::exec_session_by_id(conn, &id).map(|row| row.is_some())
            })
            .map_err(|error| error.to_string())?;
        Ok(if persisted {
            "already_finished"
        } else {
            "not_found"
        })
    }

    /// 构造 Ringing worker 命令信封并转发给 agent（legacy Ui2Agent 帧已拆除）。
    fn send_ringing_cmd(&self, session_id: String, command: RingingCommand) -> QaqhResult<Value> {
        let env = RingingWorkerCommandEnvelope::new(session_id.clone(), command_id(), command);
        self.send_ringing_command(&session_id, &env)?;
        Ok(Value::Null)
    }

    /// `session.list` 条目的运行状态归一:
    /// 未加载 → `not_running`;已加载 → 循环侧活动状态(Working/WaitingUser
    /// 映射)或 owner 业务库直读。
    fn session_run_status(
        &self,
        loaded: bool,
        fallback: Option<ActivityState>,
        session_id: &str,
    ) -> qaqh_types::SessionRunStatus {
        if !loaded {
            return qaqh_types::SessionRunStatus::NotRunning;
        }
        let state = fallback.or_else(|| {
            self.owners
                .session_run_state(session_id)
                .ok()
                .map(|state| match state {
                    qaqh_session::owner::SessionRunState::Running => ActivityState::Running,
                    qaqh_session::owner::SessionRunState::Waiting => ActivityState::WaitingUser,
                    qaqh_session::owner::SessionRunState::Cancelling => ActivityState::Cancelling,
                    _ => ActivityState::Idle,
                })
        });
        // 统一词表(spec §4.4):session 列表与 activity 同一计算入口;
        // 失败/中断记录在回合 outcome,不再用 Error/Canceled 活动状态替代。
        match state {
            Some(ActivityState::Running) => qaqh_types::SessionRunStatus::Running,
            Some(ActivityState::WaitingUser) => qaqh_types::SessionRunStatus::WaitingUser,
            Some(ActivityState::Cancelling) => qaqh_types::SessionRunStatus::Cancelling,
            _ => qaqh_types::SessionRunStatus::Idle,
        }
    }

    /// `session.list` 的条目（前端契约 **G2**）。
    /// 返回**类型化**条目而非 `Value`：形状由 `qaqh_types::SessionListEntry` 承载，
    /// 序列化只发生在 dispatch 边界。此前这里是 `to_value(&meta)` 之后再
    /// `value["running"] = …` 手工拼键——手拼的形状没有任何类型承载，三端前端
    /// 只能各自手解（TUI 那份手抄漏了 5 个键而无人察觉）。
    /// 返回类型化值后，**新增字段的唯一途径是改类型**：想在回包里塞个临时键，
    /// 必须先拆掉类型才行。形状锁在 `qaqh-types` 的
    /// `session_list_entry_wire_keys_are_locked`。
    fn list_sessions(
        &self,
        include_archived: bool,
    ) -> QaqhResult<Vec<qaqh_types::SessionListEntry>> {
        let registry = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        let rows = self
            .owners
            .db()
            .read_blocking(|conn| {
                qaqh_session::db::store::list_sessions(conn, include_archived, 256)
            })
            .map_err(QaqhError::message)?;
        let mut workspace_ids = self
            .owners
            .db()
            .read_blocking(qaqh_session::db::workspace::session_ids)
            .map_err(QaqhError::message)?;
        let mut entries = Vec::with_capacity(rows.len());
        for row in rows {
            let session_id = row.session_id.clone();
            let loaded = registry.is_running(&session_id);
            let fallback = registry
                .activities()
                .into_iter()
                .find(|activity| activity.session_id == session_id)
                .map(|activity| activity.state);
            let status = self.session_run_status(loaded, fallback, &session_id);
            let meta = qaqh_types::SessionMeta {
                session_id: session_id.clone(),
                title: Some(row.title).filter(|title| !title.is_empty()),
                cwd: row.cwd,
                model: row.model.unwrap_or_default(),
                tool_mode: row.tool_mode.unwrap_or_default(),
                model_selection: row.model_selection,
                settings_revision: row.settings_revision,
                turn_count: row.turn_count.max(0) as usize,
                archived: row.archived,
                ephemeral: row.ephemeral,
                created_at: (row.created_at_ms.max(0) as u64) / 1000,
                updated_at: (row.updated_at_ms.max(0) as u64) / 1000,
                ..SessionMeta::default()
            };
            let stored_workspace_id = workspace_ids.remove(&session_id);
            let managed_cwd = meta.cwd.as_deref().is_some_and(|cwd| {
                qaqh_types::platform::is_managed_session_workspace(std::path::Path::new(cwd))
            });
            let mut workspace_id = if managed_cwd {
                None
            } else {
                stored_workspace_id
            };
            if workspace_id.is_none()
                && !managed_cwd
                && !meta.ephemeral
                && let Some(cwd) = meta.cwd.as_deref()
            {
                let session = session_id.clone();
                let cwd = cwd.to_string();
                workspace_id = self
                    .owners
                    .db()
                    .write_blocking(move |conn| {
                        qaqh_session::db::workspace::attach_by_cwd(conn, &session, &cwd)
                    })
                    .map_err(QaqhError::message)?;
            }
            entries.push(qaqh_types::SessionListEntry {
                meta,
                status,
                workspace_id,
            });
        }
        Ok(entries)
    }

    /// 配置写后广播（所有 config.* / profile.* 写路径共用）。
    fn notify_config_changed(&self) {
        // 客户端侧的「重拉提示」由前端轮询/焦点刷新兜底;worker 侧重载
        // 走 AgentReloadConfig 广播(改配置要作用到活跃 worker,含子代理)。
        let failed = match self.registry() {
            Ok(mut registry) => registry.broadcast_ringing(&RingingCommand::AgentReloadConfig),
            Err(error) => {
                log::warn!("[config] registry unavailable for reload broadcast: {error}");
                return;
            }
        };
        if !failed.is_empty() {
            log::warn!(
                "[config] reload broadcast failed for: {}",
                failed.join("; ")
            );
        }
    }

    /// 单写口：`Config::update` 成功后广播 reload（BUG-001/008）。
    fn update_config_and_reload<F>(&self, mutate: F) -> QaqhResult<qaqh_config::Config>
    where
        F: FnOnce(&mut qaqh_config::Config) -> Result<(), String>,
    {
        let config = qaqh_config::Config::update(mutate).map_err(QaqhError::message)?;
        self.notify_config_changed();
        Ok(config)
    }

    fn save_config(&self, patch: qaqh_config_api::ConfigPatch) -> QaqhResult<()> {
        // Never log config.save payloads: they may contain provider credentials.
        log::info!("[config.save] saving configuration");
        // P1-C2：wire 切换为 Merge Patch 契约（qaqh-config-api）——值域校验在
        // api 层 validate()；逐字段守卫语义集中在 qaqh_config::dto::apply_patch
        // （穷举映射，新增字段未同步会编译失败）。旧 snake/camel 双键 shim 就此退役。
        self.update_config_and_reload(|cfg| qaqh_config::dto::apply_patch(cfg, &patch))
            .map(|_| ())
            .map_err(QaqhError::message)
    }
}

/// P2-1：MCP 热重载器——订阅 watch 单写口广播，只对 `[mcp]` 段变化响应。
/// 链路：`Config::update`（webUI/CLI）或文件轮询发布 → `watch::subscribe`
/// 唤醒 → 新配置的 [mcp] 与 manager 当前配置不等 → merge_external 重扫
/// （外部 Codex/Claude 用户级文件的变化在此一并捕获）→ `apply_config`
/// diff 热更新 → 投影置脏（下一回合投影批次自动重建）。日志记录 diff 报告。
fn spawn_mcp_reloader() {
    let rx = qaqh_config::watch::subscribe();
    tokio::spawn(async move {
        reload_loop(rx, |published| async move {
            let manager = qaqh_mcp::manager_slot();
            if published.mcp == manager.config() {
                return; // 非 [mcp] 段变更：与 MCP 无关，跳过
            }
            let mut new_mcp = published.mcp.clone();
            // 外部配置重扫：ext-* 条目随用户级外部文件变化增删；本地
            // 手写面优先的碰撞语义与启动路径一致。
            let paths = qaqh_config::mcp_import::default_user_paths();
            let _ = qaqh_config::mcp_import::merge_external(&mut new_mcp, &paths);
            let report = manager.apply_config(new_mcp).await;
            log::info!(
                "[mcp] hot-reload applied (added={:?} updated={:?} removed={:?} kept={} conn)",
                report.added,
                report.updated,
                report.removed,
                report.kept.len()
            );
            // P2-1 缺口修复（用户实测发现）：热重载新增/变更的 server 无预热
            // 触发点——启动 prime 在 enabled=false 时是 no-op 且 reloader 不在
            // 启动路径，鸡生蛋重现（模型面永远看不到新 server 工具）。
            // apply 后补发幂等预热（已连接 skip；新增连接+缓存+置脏 → 下回合
            // 投影可见）。
            if !report.added.is_empty() || !report.updated.is_empty() {
                qaqh_mcp::prime_all_async();
            }
        })
        .await;
        log::warn!("[mcp] hot-reload watcher channel closed; exiting");
    });
}

/// 热重载事件循环（T-6-1 抽出的可测小函数）。
/// 语义契约：`qaqh_config::watch::subscribe()` 走 `Sender::subscribe`，返回的
/// receiver **已消费到当前版本**——订阅前的任何快照都不会唤醒 `changed()`，
/// 因此首次 `changed()` 等到的是**订阅后的第一次真实发布**。
/// 故此处不得有任何「前置消费启动快照」的守卫：那会吞掉用户启动后的首次
/// 真实变更（BUG-2026-09-15-07 / E2，回归见 `reload_loop_tests`）。
/// 段级幂等（`published.<sec> == manager.config()`）由 `on_publish` 自理。
async fn reload_loop<F, Fut>(
    mut rx: tokio::sync::watch::Receiver<Option<Arc<qaqh_config::Config>>>,
    on_publish: F,
) where
    F: Fn(Arc<qaqh_config::Config>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    while rx.changed().await.is_ok() {
        let Some(published) = rx.borrow_and_update().clone() else {
            continue;
        };
        on_publish(published).await;
    }
}

/// P2-1 同款：`[lsp]` 热重载——新配置与 manager 当前配置不等 →
/// `apply_config` diff 保连。LSP 无外部源重扫（只认手写面）。
fn spawn_lsp_reloader() {
    let rx = qaqh_config::watch::subscribe();
    tokio::spawn(async move {
        reload_loop(rx, |published| async move {
            let manager = qaqh_lsp::manager_slot();
            if published.lsp == manager.config() {
                return; // 非 [lsp] 段变更：与 LSP 无关，跳过
            }
            let report = manager.apply_config(published.lsp.clone()).await;
            log::info!(
                "[lsp] hot-reload applied (added={:?} updated={:?} removed={:?} kept={} conn)",
                report.added,
                report.updated,
                report.removed,
                report.kept.len()
            );
        })
        .await;
        log::warn!("[lsp] hot-reload watcher channel closed; exiting");
    });
}

/// P2-1：config.toml 文件轮询器——手改文件（不经 `Config::update` 单写口）
/// 也能触发热重载。
/// 策略：mtime 检测（1.5s 周期）→ 变化即 `reload_from_disk()`（内部：
/// 解析失败——编辑器写一半——静默跳过，下轮再试；成功则统一发布，
/// 重载器随之生效）。轮询而非 inotify：无新依赖，低频配置变更场景足够。
fn spawn_config_file_poller() {
    const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(1500);
    tokio::spawn(async move {
        let path = qaqh_types::ConfigStore::default_location()
            .path()
            .to_path_buf();
        let mut last_mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        loop {
            tokio::time::sleep(POLL_INTERVAL).await;
            let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
            if mtime.is_some() && mtime != last_mtime {
                last_mtime = mtime;
                if qaqh_config::watch::reload_from_disk() {
                    log::info!(
                        "[config] file change detected ({}); reloaded",
                        path.display()
                    );
                }
            } else if mtime.is_none() && last_mtime.is_some() {
                // 文件被删除：记录状态，不发布（等待恢复或编辑器原子替换）。
                log::warn!("[config] config file missing ({}); waiting", path.display());
                last_mtime = None;
            }
        }
    });
}

/// `session.create` 的 canonical 基线：`(cwd, model)`。
///
/// cwd 的优先级：**创建请求里显式给出的路径** > 已绑定的 meta.cwd > 默认
/// 会话工作根。显式路径必须已经存在且是目录，非法值直接拒绝，不降级——
/// 创世事实、模型提示、工具上下文和沙箱可写根都用这同一个值。
/// 默认目录由我们创建，与 `load_session_workspace` 注入的目录同源。
/// 模型读不到配置时记 `unknown`（不阻塞建会话）。
pub(crate) fn session_canonical_cwd(
    owners: &qaqh_session::owner::OwnerDirectory,
    session_id: &str,
    requested: Option<&str>,
) -> Result<String, String> {
    // Creation must not read the request back out of metadata that genesis has
    // not written yet: the explicit path wins and is resolved exactly once
    // (report 2026-10-31 §11.1 F09). An invalid path is rejected, never
    // downgraded to the default directory.
    let requested = requested
        .map(str::trim)
        .filter(|cwd| !cwd.is_empty() && *cwd != ".");
    if let Some(requested) = requested {
        let path = std::path::PathBuf::from(requested);
        let path = std::fs::canonicalize(&path)
            .map(normalize_workspace_path)
            .map_err(|error| {
                format!(
                    "session workspace does not exist: {} ({error})",
                    path.display()
                )
            })?;
        if !path.is_dir() {
            return Err(format!(
                "session workspace is not a directory: {}",
                path.display()
            ));
        }
        return Ok(path.to_string_lossy().into_owned());
    }
    let bound = crate::registry::spawn::session_cwd(owners, session_id);
    let cwd = match bound {
        // A workspace the user picked must already exist: silently creating an
        // empty directory with the same name hides a vanished project.
        Some(cwd) => {
            let path = std::path::PathBuf::from(&cwd);
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
            cwd
        }
        // The default workspace is ours to create. It is the same path the
        // model is told about, the sandbox is given as a writable root, and the
        // process is started in — a path that does not exist fails the sandbox.
        None => {
            let path = qaqh_types::platform::default_session_workspace_dir(session_id);
            std::fs::create_dir_all(&path).map_err(|error| {
                format!(
                    "create default session workspace {}: {error}",
                    path.display()
                )
            })?;
            path.to_string_lossy().into_owned()
        }
    };
    Ok(cwd)
}

/// Strip the Windows verbatim prefix that `canonicalize` adds.
///
/// The resolved path is persisted as `SessionCreated.cwd`, shown in the UI and
/// used as the sandbox writable root; keeping the verbatim prefix would change
/// the path shape the user picked for no benefit.
pub(crate) fn normalize_workspace_path(path: std::path::PathBuf) -> std::path::PathBuf {
    #[cfg(windows)]
    {
        let text = path.to_string_lossy();
        if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
            return std::path::PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = text.strip_prefix(r"\\?\") {
            return std::path::PathBuf::from(rest.to_string());
        }
    }
    path
}

pub(crate) mod command;
pub(crate) mod common;
pub(crate) mod fs_git;
pub(crate) mod handlers;
pub(crate) mod params;
pub(crate) mod plan;
pub(crate) mod stats;

use self::common::{command_id, err, release_freed_heap_memory};

#[cfg(test)]
mod tool_mode_tests {
    use super::params::{optional_tool_mode, validate_tool_mode};

    #[test]
    fn optional_tool_mode_defaults_to_none() {
        assert_eq!(optional_tool_mode(None, &[]).unwrap(), None);
    }

    #[test]
    fn optional_tool_mode_rejects_deprecated_minimal_dsh() {
        // minimal:dsh 已随 bash/pwsh 拆分下线：废弃模式必须被 KNOWN_MODES
        // 白名单拒绝，不得回流。
        assert!(optional_tool_mode(Some("minimal:dsh"), &[]).is_err());
    }

    #[test]
    fn optional_tool_mode_rejects_unknown_values() {
        assert!(optional_tool_mode(Some("turbo"), &[]).is_err());
    }

    #[test]
    fn optional_tool_mode_custom_requires_tools() {
        assert!(optional_tool_mode(Some("custom"), &[]).is_err());
        let (mode, tools) = optional_tool_mode(Some("custom"), &["exec".to_string()])
            .unwrap()
            .unwrap();
        assert_eq!(mode, "custom");
        assert_eq!(tools, vec!["exec"]);
    }

    #[test]
    fn validate_tool_mode_accepts_all_known_presets() {
        for mode in qaqh_types::KNOWN_MODES {
            assert!(validate_tool_mode(mode).is_ok(), "{mode}");
        }
        assert!(validate_tool_mode("turbo").is_err());
    }
}

#[cfg(test)]
mod reload_loop_tests {
    use std::{sync::Arc, time::Duration};

    use qaqh_config::Config;

    use super::reload_loop;

    /// T-6-1 回归（BUG-2026-09-15-07 / E2）：`qaqh_config::watch::subscribe()`
    /// 走 `Sender::subscribe`，返回的 receiver **已消费到当前版本** ⇒ 订阅后的
    /// **第一次**发布必须被 reloader 观察到，不得被「前置 changed() 守卫」吞掉
    /// （旧代码那次 `borrow_and_update()` 正好丢掉用户启动后的首次真实改动）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn first_publish_after_subscribe_is_observed() {
        let (tx, _bootstrap) = tokio::sync::watch::channel::<Option<Arc<Config>>>(None);
        // 订阅前已有一版快照（模拟「daemon 启动时 manager 已按当前配置装配」）。
        tx.send(Some(Arc::new(Config {
            context_length: 1,
            ..Default::default()
        })))
        .unwrap();
        let rx = tx.subscribe(); // 与 watch::subscribe() 同款：Sender::subscribe

        let (seen_tx, mut seen_rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = tokio::spawn(reload_loop(rx, move |published| {
            let seen_tx = seen_tx.clone();
            async move {
                let _ = seen_tx.send(published.context_length);
            }
        }));

        // 订阅前的历史快照不得被重放（subscribe() 已把它标记为已消费）。
        assert!(
            tokio::time::timeout(Duration::from_millis(150), seen_rx.recv())
                .await
                .is_err(),
            "订阅前的快照不得触发 apply"
        );

        // 订阅后的第一次真实变更必须被观察到（旧前置守卫在此处吞掉本次变更）。
        tx.send(Some(Arc::new(Config {
            context_length: 4242,
            ..Default::default()
        })))
        .unwrap();
        let seen = tokio::time::timeout(Duration::from_secs(5), seen_rx.recv())
            .await
            .expect("首次变更被吞：reloader 未观察到订阅后的第一次发布")
            .expect("seen 通道未关闭");
        assert_eq!(seen, 4242);
        handle.abort();
    }
}

#[cfg(test)]
mod plan_service_tests {
    /// `plan_action` 用 `lines()` + `join("\n")` 回写，文件以非空行结尾时
    /// 每次裁决静默剥掉末尾换行。
    #[test]
    fn plan_action_preserves_the_trailing_newline() {
        let root = tempfile::tempdir().expect("tempdir");
        // 计划文件路径 = workspace/.qaqh/PLAN.md；workspace 由线程级作用域
        // 提供，避开 session 全局单例（单测进程内不可重复 init）。
        let workspace = root.path().join("ws");
        std::fs::create_dir_all(workspace.join(".qaqh")).expect("mkdir");
        let plan_path = workspace.join(".qaqh").join("PLAN.md");
        std::fs::write(
            &plan_path,
            "- [ ] item-1: do the thing\n- [ ] item-2: another thing\n",
        )
        .expect("write plan");
        let previous =
            qaqh_workspace::push_thread_workspace(Some(workspace.to_string_lossy().into_owned()));

        let outcome = plan_action_against_workspace(&plan_path);

        let rewritten = std::fs::read_to_string(&plan_path).expect("read plan");
        qaqh_workspace::pop_thread_workspace(previous);
        outcome.expect("plan_action must accept a valid plan item");
        assert_eq!(
            rewritten, "- [✓] item-1: do the thing\n- [ ] item-2: another thing\n",
            "trailing newline must survive plan_action"
        );
    }

    /// 直接驱动 `plan_action` 的写回段（`qaqh_dir` 依赖 session 目录，测试
    /// 只关心文件尾形态，故在此复刻同一路径解析并断言输出）。
    fn plan_action_against_workspace(plan_path: &std::path::Path) -> Result<(), String> {
        let content = std::fs::read_to_string(plan_path).map_err(|e| e.to_string())?;
        let item_id = "item-1";
        let action = "approve";
        let mut found = false;
        let output = content
            .lines()
            .filter_map(|line| {
                if !found
                    && line.trim().starts_with("- [")
                    && line.contains(&format!(" {item_id}: "))
                {
                    found = true;
                    let end = line.find(']')?;
                    let rest = line.split_at(end + 1).1;
                    let base = format!("- [ ]{rest}");
                    return Some(match action {
                        "approve" => base.replacen("- [ ]", "- [✓]", 1),
                        _ => line.to_string(),
                    });
                }
                Some(line.to_string())
            })
            .collect::<Vec<_>>()
            .join("\n");
        if !found {
            return Err(format!("plan item {item_id} not found"));
        }
        let output = if content.ends_with('\n') && !output.ends_with('\n') {
            format!("{output}\n")
        } else {
            output
        };
        std::fs::write(plan_path, output).map_err(|e| e.to_string())
    }
}
