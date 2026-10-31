use std::{
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use qaqh_domain::{ActivityState, ControlCommand, RingingChannel};
use qaqh_ringing::{RingingCommand, RingingWorkerCommandEnvelope};
use qaqh_session::{
    actor::ConnectionId,
    canonical::{CanonicalSessionIdentity, generate_ulid},
    session_commit::SessionCommit,
    session_fact_v2::{
        EventId, FactPayload, FactSchema, MetadataSource, SessionCreated, SessionFact, SessionId,
        SessionMetadataChanged, SessionMetadataPatch,
    },
};
use serde_json::Value;

use crate::error::{QaqhError, QaqhResult};
use crate::service::command::Command;
use crate::{AgentRegistry, RingingHub, ringing::V2ProjectionHub};

#[derive(Clone)]
pub struct QaqhService {
    pub(crate) registry: Arc<Mutex<AgentRegistry>>,
    pub(crate) hub: std::sync::OnceLock<Arc<RingingHub>>,
    pub(crate) v2_hub: std::sync::OnceLock<Arc<V2ProjectionHub>>,
    /// 会话存储句柄（PR-3-1 注入化：daemon main 装配点 init 后注入，
    /// service 内不再触达会话单例的全局访问器）。
    pub(crate) sessions: Arc<qaqh_session::SessionManager>,
    /// Per-root task board stores, opened lazily.
    pub(crate) task_stores:
        Arc<Mutex<std::collections::HashMap<String, Arc<Mutex<qaqh_session::team::TeamStore>>>>>,
    /// Per-root message board stores, opened lazily.
    pub(crate) board_stores:
        Arc<Mutex<std::collections::HashMap<String, Arc<Mutex<qaqh_session::team::BoardStore>>>>>,
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
        // 不带 subagent 注册器：设置页勾选的是子代理可用工具，spawn_subagent
        // 本身不属于子代理工具集。
        qaqh_workspace::runtime::init_tools("daemon", &[], vec![]);
        let mut registry = AgentRegistry::new(sessions.clone());
        registry.set_max_depth(config.subagent.max_depth as usize);
        registry.set_message_quota_limits(
            config.subagent.message_in_flight_per_pair,
            config.subagent.message_outbound_per_sender,
        );
        Self {
            registry: Arc::new(Mutex::new(registry)),
            hub: std::sync::OnceLock::new(),
            v2_hub: std::sync::OnceLock::new(),
            sessions,
            task_stores: Arc::new(Mutex::new(std::collections::HashMap::new())),
            board_stores: Arc::new(Mutex::new(std::collections::HashMap::new())),
            siwc: std::sync::OnceLock::new(),
            dsaccount: std::sync::OnceLock::new(),
        }
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

    /// 挂载 Ringing 运行时（worker 事件双投）。
    pub fn attach_ringing(&self, hub: Arc<RingingHub>) {
        let _ = self.hub.set(hub.clone());
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .attach_ringing(hub);
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

    /// Attach the canonical V2 projection hub used for runtime residency
    /// overlays and ephemeral `TeamDelta` publication.
    pub fn attach_v2_projection(&self, hub: Arc<V2ProjectionHub>) {
        let _ = self.v2_hub.set(hub.clone());
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .attach_v2_projection(hub);
    }

    /// 转发 Ringing 命令到 agent worker（wire 判别后由 worker reader 解析）。
    pub fn send_ringing_command(
        &self,
        session_id: &str,
        env: &qaqh_ringing::RingingWorkerCommandEnvelope,
    ) -> Result<(), String> {
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .send_ringing(session_id, env)
            .map_err(|error| error.to_string())
    }

    pub fn subscribe_channel(
        &self,
        session_id: &str,
        connection_id: &ConnectionId,
        channel: RingingChannel,
    ) -> Result<bool, String> {
        self.registry()?
            .subscribe_channel(session_id, connection_id.clone(), channel)
            .map_err(|error| error.to_string())
    }

    pub fn unsubscribe_channel(
        &self,
        session_id: &str,
        connection_id: &ConnectionId,
        channel: RingingChannel,
    ) -> Result<bool, String> {
        self.registry()?
            .unsubscribe_channel(session_id, connection_id.clone(), channel)
            .map_err(|error| error.to_string())
    }

    pub fn connection_closed(
        &self,
        session_id: &str,
        connection_id: &ConnectionId,
    ) -> Result<usize, String> {
        self.registry()?
            .connection_closed(session_id, connection_id.clone())
            .map_err(|error| error.to_string())
    }

    /// 关闭会话（Ringing `SessionClose` 命令语义，契约 §2）：
    /// 关闭 registry 实例并清理常驻状态（Closed 广播已删，A1：wire 零消费方），
    /// causation 参数不再使用。会话不存在同样返回 Ok（幂等关闭）。
    pub fn close_session(
        &self,
        session_id: &str,
        _causation_id: Option<&str>,
    ) -> Result<(), String> {
        self.registry()?.close(session_id);
        // D-3：关闭即终止该会话的取消标记。worker 优雅收尾可能已把它置假
        // （actor 线程 clear_cancel），但整项移除才能让 SESSION_CANCELS 与
        // 活跃会话同阶（种子频繁进出的 daemon 长期运行不无界增长）。
        qaqh_workspace::remove_session_cancel(session_id);
        // 临时会话（子代理）用完即走：关闭后删除会话目录，磁盘零残留。
        // 目录已不存在（重复 close / 已被清理）时静默跳过，保持幂等。
        if self
            .session_meta(session_id)
            .is_some_and(|meta| meta.ephemeral)
        {
            match self.sessions.delete(session_id) {
                Ok(()) => {
                    log::info!("[session] ephemeral session {session_id} cleaned up (auto-unload)")
                }
                Err(e) if e.contains("Session not found") => {}
                Err(e) => log::warn!("[session] ephemeral cleanup {session_id} failed: {e}"),
            }
        }
        // 阶段 3b：SessionStateChanged{Closed} 的 v1 广播已删（A1：wire 零消费方）。
        self.release_session_resident_state(session_id);
        qaqh_memwatch::global().mark_session_unloaded(session_id);
        release_freed_heap_memory();
        Ok(())
    }

    /// D-1：会话关闭后的 per-seed 常驻内存清理（修复 IMAGE_REGISTRY 与 hub
    /// channels 的 per-seed 残留——诊断结论②③）。必须在 worker join 之后、
    /// 终态（Closed）发布之后调用，顺序不可换：join 前 reset/forget 会让
    /// 仍在运行的 worker 继续写回脏状态；终态发布前 forget 会让 publish
    /// 触发 lazy-load，把刚丢弃的状态原样重建回来，清理变成空操作。
    /// forget 之后该 seed 的 hub 态与 daemon 重启后的空态同构；磁盘索引
    /// 保留，下次访问走既有 lazy-load 重放路径（UI 历史不丢）。
    fn release_session_resident_state(&self, session_id: &str) {
        // 图片注册表按 seed 键控存 base64（read_image 的上传缓存）。resume
        // 路径（state/lifecycle.rs）会从持久化消息历史重建，关闭期清空
        // 不破坏 image_index 语义。
        qaqh_workspace::read_image::reset_images(session_id);
        if let Some(hub) = self.hub.get() {
            hub.forget_session(session_id);
        }
    }

    /// E: idle 卸载空闲会话 worker。
    /// `idle_secs` <= 0 时为 no-op（配置禁用）。对每个被卸载的 seed 做常驻
    /// 状态清理（Closed 广播已删，A1：wire 零消费方）。
    /// 返回被卸载的 seed 列表。registry.close 是阻塞 join——调用方
    /// （daemon 周期任务）必须置于 spawn_blocking。
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
            self.release_session_resident_state(session_id);
            qaqh_memwatch::global().mark_session_unloaded(session_id);
        }
        if !unloaded.is_empty() {
            release_freed_heap_memory();
        }
        unloaded
    }

    fn refresh_memory_components(&self) {
        let monitor = qaqh_memwatch::global();
        if !monitor.is_enabled() {
            return;
        }
        if let Ok(registry) = self.registry.lock() {
            for component in registry.memory_components() {
                monitor.update_component(component);
            }
        }
        if let Some(hub) = self.hub.get() {
            for component in hub.memory_components() {
                monitor.update_component(component);
            }
        }
        if let Some(hub) = self.v2_hub.get() {
            for component in hub.memory_components() {
                monitor.update_component(component);
            }
        }
        let task_stores = self
            .task_stores
            .lock()
            .map(|stores| stores.len() as u64)
            .unwrap_or_default();
        let board_stores = self
            .board_stores
            .lock()
            .map(|stores| stores.len() as u64)
            .unwrap_or_default();
        monitor.update_component(qaqh_memwatch::ComponentMemory {
            name: "service.task_stores".into(),
            item_count: task_stores,
            payload_bytes: None,
            heap_estimate_bytes: None,
            ..Default::default()
        });
        monitor.update_component(qaqh_memwatch::ComponentMemory {
            name: "service.board_stores".into(),
            item_count: board_stores,
            payload_bytes: None,
            heap_estimate_bytes: None,
            ..Default::default()
        });
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
        let images = qaqh_workspace::read_image::memory_stats();
        monitor.update_component(qaqh_memwatch::ComponentMemory {
            name: "workspace.image_registry".into(),
            item_count: images.entry_count as u64,
            payload_bytes: None,
            heap_estimate_bytes: Some(images.heap_estimate_bytes),
            ..Default::default()
        });
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
            qaqh_session::session_fact_v2::SessionMetadataPatch {
                archived: Some(true),
                ..Default::default()
            },
        );
        Ok(())
    }

    /// 恢复归档会话：meta `archived=false` + 重新拉起实例（resume 语义，
    /// 对齐 `session.resume` 查询——get_or_spawn + active seed 更新）。
    pub fn unarchive_session(&self, session_id: &str) -> Result<(), String> {
        let _ = self.write_meta(
            session_id,
            qaqh_session::session_fact_v2::SessionMetadataPatch {
                archived: Some(false),
                ..Default::default()
            },
        );
        self.registry()?
            .get_or_spawn(session_id)
            .map_err(|error| error.to_string())
    }

    /// 彻底删除会话（左侧列表 × 语义）：先关实例（若运行，幂等）再删
    /// 磁盘目录与索引。会话不存在返回 Err（由 daemon 拦截层按幂等处理）。
    pub fn delete_session(
        &self,
        session_id: &str,
        causation_id: Option<&str>,
    ) -> Result<(), String> {
        let _ = self.close_session(session_id, causation_id);
        self.sessions.delete(session_id)
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

    pub fn shutdown(&self) {
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .shutdown_all();
    }

    /// F4: 死 worker 重生（daemon 周期任务调用；内部自带退避与关闭保护）。
    pub fn respawn_dead_agents(&self) {
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .respawn_dead_agents();
    }

    /// True while stopping the daemon would interrupt work or abandon an
    /// interaction waiting for its lease owner. Used by lifecycle takeover so
    /// an updater cannot race a newly-started turn.
    pub fn has_active_work(&self) -> bool {
        self.activity_snapshot().0
    }

    /// 只读活动快照（/activity 观测端点，冻结事故 P0）：逐会话活动状态 +
    /// 是否有活跃工作。单次加锁保证 flag 与列表一致；僵尸会话（如
    /// 2026-09-02 的 running 冻结）可直接从外部探测，不再依赖人肉轮询。
    pub fn activity_snapshot(&self) -> (bool, Vec<qaqh_domain::SessionActivity>) {
        let activities = self
            .registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .activities();
        let has_active_work = activities.iter().any(|activity| {
            matches!(
                activity.state,
                ActivityState::Starting | ActivityState::Working | ActivityState::WaitingUser
            )
        });
        (has_active_work, activities)
    }

    /// Write a session metadata patch (actor first, journal fallback).
    pub(crate) fn write_meta(
        &self,
        session_id: &str,
        patch: qaqh_session::session_fact_v2::SessionMetadataPatch,
    ) -> Result<(), String> {
        let registry = self.registry()?;
        crate::session_meta::patch(Some(&registry), session_id, patch)
    }

    /// Write a session title (actor first, journal fallback).
    pub(crate) fn write_title(
        &self,
        session_id: &str,
        title: &str,
        source: qaqh_session::session_fact_v2::TitleSource,
    ) -> Result<(), String> {
        let registry = self.registry()?;
        crate::session_meta::set_title(Some(&registry), session_id, title, source)
    }

    /// Session metadata read from committed facts.
    pub(crate) fn session_meta(&self, session_id: &str) -> Option<qaqh_types::SessionMeta> {
        crate::session_meta::read(session_id)
    }

    pub(crate) fn registry(&self) -> Result<std::sync::MutexGuard<'_, AgentRegistry>, String> {
        self.registry
            .lock()
            .map_err(|e| format!("registry lock: {e}"))
    }

    /// 构造 Ringing worker 命令信封并转发给 agent（legacy Ui2Agent 帧已拆除）。
    fn send_ringing_cmd(&self, session_id: String, command: RingingCommand) -> QaqhResult<Value> {
        let env = RingingWorkerCommandEnvelope::new(session_id.clone(), command_id(), command);
        self.send_ringing_command(&session_id, &env)
            .map_err(QaqhError::message)?;
        Ok(Value::Null)
    }

    /// Ask the session actor to publish a canonical resource fact after an
    /// off-actor workspace mutation. The RPC has already persisted its file, so
    /// a missing/unreachable worker (idle session, subagent race) only costs
    /// the notification — never fail the mutation for it.
    fn publish_resource_changed_best_effort(&self, session_id: &str, resource_kind: &str) {
        let command = RingingCommand::Control(ControlCommand::PublishResourceChanged {
            resource_kind: resource_kind.to_string(),
        });
        let env = RingingWorkerCommandEnvelope::new(session_id.to_string(), command_id(), command);
        if let Err(error) = self.send_ringing_command(session_id, &env) {
            log::debug!(
                "[todo] resource fact publication skipped for {session_id} (no worker): {error}"
            );
        }
    }

    /// 统一运行状态（2026-10-06 归一裁决）：未加载 → `not_running`（不读投影、
    /// 不做任何合成）；已加载 → 优先 canonical fact 投影（持久可重放，重启后
    /// 语义可重建），投影不可读时退回 live tracker 的六态映射。
    /// 注意此处**有意只对已加载会话**调 `projected_run_status`——hub 首次访问
    /// 一个会话会把整个 fact log 从磁盘重放，`session.list` 对未加载会话逐条
    /// 调用等于全库重放。
    fn session_run_status(
        &self,
        loaded: bool,
        fallback: Option<ActivityState>,
        session_id: &str,
    ) -> qaqh_types::SessionRunStatus {
        if !loaded {
            return qaqh_types::SessionRunStatus::NotRunning;
        }
        if let Some(hub) = self.v2_hub.get() {
            let session_dir = qaqh_types::platform::sessions_dir().join(session_id);
            match hub.projected_run_status(&session_dir, session_id) {
                Ok(status) => return status,
                Err(error) => log::warn!(
                    "[session] run status projection fallback for {session_id}: {error:?}"
                ),
            }
        }
        match fallback {
            Some(ActivityState::Starting | ActivityState::Working) => {
                qaqh_types::SessionRunStatus::Working
            }
            // 投影不可读时无法细分等待类别；loop 仍在回合中途，归 working。
            Some(ActivityState::WaitingUser) => qaqh_types::SessionRunStatus::Working,
            Some(ActivityState::Failed) => qaqh_types::SessionRunStatus::Error,
            Some(ActivityState::Disconnected) => qaqh_types::SessionRunStatus::NotRunning,
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
    fn list_sessions(&self) -> Vec<qaqh_types::SessionListEntry> {
        let manager = &self.sessions;
        let registry = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        let workspaces = qaqh_session::WorkspaceStore::global();
        manager
            .list()
            .into_iter()
            .map(|meta| {
                let loaded = registry.is_running(&meta.session_id);
                let fallback = registry
                    .activities()
                    .into_iter()
                    .find(|activity| activity.session_id == meta.session_id)
                    .map(|activity| activity.state);
                let status = self.session_run_status(loaded, fallback, &meta.session_id);
                let mut workspace_id = workspaces.workspace_of(&meta.session_id);
                // 存量自愈：若该非临时会话带有合法 cwd 且未归属任何工作区，自动纳管归入
                if workspace_id.is_none()
                    && !meta.ephemeral
                    && let Some(cwd) = meta.cwd.as_deref()
                {
                    workspace_id = workspaces.attach_by_cwd(&meta.session_id, cwd);
                }
                qaqh_types::SessionListEntry {
                    meta,
                    status,
                    workspace_id,
                }
            })
            .collect()
    }

    /// 配置写后广播（所有 config.* / profile.* 写路径共用）。
    fn notify_config_changed(&self) {
        // v2：给驻留会话发一条 config_changed 信号（客户端据此重拉 config.load）。
        // 在此之前前端只能靠自建广播 + 15s/焦点轮询兜底。
        if let Some(hub) = self.v2_hub.get() {
            match hub.publish_config_changed() {
                Ok(published) => log::debug!("[config] v2 config_changed → {published} session(s)"),
                Err(error) => log::warn!("[config] v2 config_changed broadcast failed: {error:?}"),
            }
        }
        // worker 侧重载仍走 AgentReloadConfig 广播（改配置要作用到活跃 worker，
        // 含子代理）。客户端侧的「重拉提示」已由上方的 v2 config_changed 承担。
        let failed = match self.registry() {
            Ok(mut registry) => registry
                .broadcast_ringing(&RingingCommand::Control(ControlCommand::AgentReloadConfig)),
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

/// `session.new` / `session.create` 共用的 canonical 基线：`(cwd, model)`。
///
/// cwd 的优先级：**创建请求里显式给出的路径** > 已绑定的 meta.cwd > 默认
/// 会话工作根。显式路径必须已经存在且是目录，非法值直接拒绝，不降级——
/// 创世事实、模型提示、工具上下文和沙箱可写根都用这同一个值。
/// 默认目录由我们创建，与 `load_session_workspace` 注入的目录同源。
/// 模型读不到配置时记 `unknown`（不阻塞建会话）。
pub(crate) fn session_canonical_cwd(
    sessions: &qaqh_session::SessionManager,
    session_id: &str,
    requested: Option<&str>,
) -> Result<(String, String), String> {
    let model = qaqh_config::Config::load()
        .map(|config| config.model)
        .unwrap_or_else(|_| "unknown".to_string());
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
        return Ok((path.to_string_lossy().into_owned(), model));
    }
    let bound = sessions
        .workspace_cwd(session_id)
        .filter(|cwd| !cwd.is_empty() && cwd != ".");
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
    Ok((cwd, model))
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

/// 在 `session.new` 的持久化阶段写入 canonical 基线。
/// Bootstrap and the actor use the same batch store. Drop this temporary owner
/// before spawning the actor so there is only one journal writer at a time.
fn materialize_canonical_session(session_id: &str, cwd: &str, model: &str) -> Result<(), String> {
    let session_dir = qaqh_types::platform::sessions_dir().join(session_id);
    materialize_canonical_session_in(&session_dir, cwd, model, None, false)
}

pub(crate) fn materialize_canonical_session_in(
    session_dir: &std::path::Path,
    cwd: &str,
    model: &str,
    parent_session_id: Option<SessionId>,
    ephemeral: bool,
) -> Result<(), String> {
    let identity =
        CanonicalSessionIdentity::open_or_create(session_dir).map_err(|error| error.to_string())?;
    let session_id = identity.session_id.clone();
    let log_id = identity.log_id.clone();
    let budget = crate::session_agent::assembly::default_context_budget();
    let mut store =
        SessionCommit::open_or_create(session_dir, session_id.clone(), log_id.clone(), budget)
            .map_err(|error| error.to_string())?;
    // Opening verifies business transitions, including SessionCreated at seq 1.
    if store.head().map_err(|error| error.to_string())? > 0 {
        return Ok(());
    }
    let now_ms = system_time_ms();
    let cwd = if std::path::Path::new(cwd).is_absolute() {
        cwd.to_string()
    } else {
        std::env::current_dir()
            .map(|base| base.join(cwd).to_string_lossy().into_owned())
            .unwrap_or_else(|_| cwd.to_string())
    };
    let model = if model.trim().is_empty() {
        "unknown"
    } else {
        model
    };
    let created = SessionFact {
        schema: FactSchema::v2(),
        session_id,
        log_id,
        fact_seq: 0,
        event_id: EventId::new(generate_ulid()),
        ts_ms: now_ms,
        causation_id: None,
        turn_id: None,
        call_id: None,
        interaction_id: None,
        payload: FactPayload::SessionCreated(SessionCreated {
            created_at_ms: now_ms,
            cwd,
            model: model.to_string(),
            parent_session_id,
            schema_caps: Vec::new(),
        }),
    };
    let mut facts = vec![created];
    if ephemeral {
        // 子会话在创世批次里就带上 ephemeral：投影要求创世是第一事实，
        // 之后单独补 patch 也行，但同一批次更省一次提交且没有中间态。
        facts.push(SessionFact {
            schema: FactSchema::v2(),
            session_id: store.session_id().clone(),
            log_id: store.log_id().clone(),
            fact_seq: 0,
            event_id: EventId::new(generate_ulid()),
            ts_ms: now_ms,
            causation_id: None,
            turn_id: None,
            call_id: None,
            interaction_id: None,
            payload: FactPayload::SessionMetadataChanged(SessionMetadataChanged {
                patch: SessionMetadataPatch {
                    ephemeral: Some(true),
                    ..Default::default()
                },
                source: MetadataSource::Api,
                changed_at_ms: now_ms,
            }),
        });
    }
    store
        .commit("session-created", 0, facts)
        .map_err(|error| error.to_string())?;
    drop(store);
    if !ephemeral {
        // 列表缓存从 journal 重建：创世后立刻让会话出现在 `session.list`。
        qaqh_session::meta::refresh_index(session_dir);
    }
    Ok(())
}

fn system_time_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
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
mod canonical_session_materialization_tests {
    use qaqh_session::canonical::{
        BatchJournal, CANONICAL_IDENTITY_FILE, EVENTS_COMMIT_FILE, EVENTS_FILE, WRITER_FENCE_FILE,
    };

    use super::*;

    #[test]
    fn session_creation_materializes_one_canonical_baseline() {
        let dir = tempfile::tempdir().expect("tempdir");
        materialize_canonical_session_in(dir.path(), "/tmp/workspace", "test-model", None, false)
            .expect("materialize");
        assert!(dir.path().join(CANONICAL_IDENTITY_FILE).exists());
        assert!(dir.path().join("storage-format").exists());
        assert!(!dir.path().join(EVENTS_COMMIT_FILE).exists());
        assert!(!dir.path().join(EVENTS_FILE).exists());
        assert!(!dir.path().join(WRITER_FENCE_FILE).exists());

        let identity = CanonicalSessionIdentity::open_or_create(dir.path()).expect("identity");
        let journal = BatchJournal::open(
            dir.path(),
            identity.session_id.clone(),
            identity.log_id.clone(),
        )
        .expect("journal");
        let mut facts = Vec::new();
        journal
            .visit_committed(|batch| {
                facts.extend_from_slice(batch);
                Ok::<_, ()>(())
            })
            .expect("facts");
        assert_eq!(facts.len(), 1);
        let FactPayload::SessionCreated(created) = &facts[0].payload else {
            panic!("first fact must be SessionCreated");
        };
        // Windows 会把 /tmp/workspace 规范化成 "D:/tmp/workspace"（当前盘符
        // 前缀）；这里只断言路径尾部不被改写，盘符差异不属于本测试的关注点。
        assert!(
            created.cwd.ends_with("/tmp/workspace"),
            "cwd tail must survive canonicalization: {}",
            created.cwd
        );
        assert_eq!(created.model, "test-model");
        drop(journal);

        // 幂等：重复调用不得追加第二个 SessionCreated。
        materialize_canonical_session_in(dir.path(), "/tmp/workspace", "test-model", None, false)
            .expect("second materialize");
        let journal =
            BatchJournal::open(dir.path(), identity.session_id, identity.log_id).expect("journal");
        assert_eq!(journal.committed_head(), 1);
    }

    #[test]
    fn session_creation_resumes_an_empty_batch_store() {
        let dir = tempfile::tempdir().expect("tempdir");
        let identity = CanonicalSessionIdentity::open_or_create(dir.path()).expect("identity");
        drop(
            BatchJournal::create(
                dir.path(),
                identity.session_id.clone(),
                identity.log_id.clone(),
            )
            .expect("empty journal"),
        );
        materialize_canonical_session_in(dir.path(), "/tmp/workspace", "test-model", None, false)
            .expect("resume before baseline commit");
        let journal =
            BatchJournal::open(dir.path(), identity.session_id, identity.log_id).expect("journal");
        assert_eq!(journal.committed_head(), 1);
    }

    #[test]
    fn session_creation_does_not_overwrite_legacy_storage() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bytes = b"legacy session data\n";
        std::fs::write(dir.path().join(EVENTS_FILE), bytes).expect("legacy log");
        let error = materialize_canonical_session_in(
            dir.path(),
            "/tmp/workspace",
            "test-model",
            None,
            false,
        )
        .expect_err("legacy stores require explicit migration");
        assert!(error.contains("isolated data root"), "{error}");
        assert_eq!(std::fs::read(dir.path().join(EVENTS_FILE)).unwrap(), bytes);
        assert!(!dir.path().join("storage-format").exists());
    }
}

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

        let sessions = qaqh_session::SessionManager::try_global();
        let outcome = plan_action_against_workspace(sessions.is_some(), &plan_path);

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
    fn plan_action_against_workspace(
        _sessions_ready: bool,
        plan_path: &std::path::Path,
    ) -> Result<(), String> {
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
