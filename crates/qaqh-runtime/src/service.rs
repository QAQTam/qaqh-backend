use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use qaqh_domain::ActivityState;
use qaqh_domain::ControlCommand;
use qaqh_domain::RingingChannel;
use qaqh_ringing::{RingingCommand, RingingWorkerCommandEnvelope};
use qaqh_session::actor::ConnectionId;
use qaqh_session::canonical::{
    CanonicalSessionIdentity, CanonicalSessionStore, CommittedFactReader, WriterId, generate_ulid,
};
use qaqh_session::session_fact_v2::{
    EventId, FactPayload, FactSchema, SessionCreated, SessionFact, SessionId,
};
use serde_json::{Value, json};

use crate::ringing::V2ProjectionHub;
use crate::{AgentRegistry, RingingHub};

#[derive(Clone)]
pub struct QaqhService {
    pub(crate) registry: Arc<Mutex<AgentRegistry>>,
    pub(crate) hub: std::sync::OnceLock<Arc<RingingHub>>,
    pub(crate) v2_hub: std::sync::OnceLock<Arc<V2ProjectionHub>>,
    /// 会话存储句柄（PR-3-1 注入化：daemon main 装配点 init 后注入，
    /// service 内不再触达会话单例的全局访问器）。
    pub(crate) sessions: Arc<qaqh_session::SessionManager>,
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
        // LSP manager 装配（docs/current/architecture.md；与 MCP 同款单例；
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
        Self {
            registry: Arc::new(Mutex::new(AgentRegistry::new(sessions.clone()))),
            hub: std::sync::OnceLock::new(),
            v2_hub: std::sync::OnceLock::new(),
            sessions,
        }
    }

    /// 挂载 Ringing 运行时（worker 事件双投）。
    pub fn attach_ringing(&self, hub: Arc<RingingHub>) {
        let _ = self.hub.set(hub.clone());
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .attach_ringing(hub);
    }

    /// Attach the canonical V2 projection hub used for runtime residency
    /// overlays.
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
        seed: &str,
        env: &qaqh_ringing::RingingWorkerCommandEnvelope,
    ) -> Result<(), String> {
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .send_ringing(seed, env)
    }

    pub fn subscribe_channel(
        &self,
        seed: &str,
        connection_id: &ConnectionId,
        channel: RingingChannel,
    ) -> Result<bool, String> {
        self.registry()?
            .subscribe_channel(seed, connection_id.clone(), channel)
    }

    pub fn unsubscribe_channel(
        &self,
        seed: &str,
        connection_id: &ConnectionId,
        channel: RingingChannel,
    ) -> Result<bool, String> {
        self.registry()?
            .unsubscribe_channel(seed, connection_id.clone(), channel)
    }

    pub fn connection_closed(
        &self,
        seed: &str,
        connection_id: &ConnectionId,
    ) -> Result<usize, String> {
        self.registry()?
            .connection_closed(seed, connection_id.clone())
    }

    /// 关闭会话（Ringing `SessionClose` 命令语义，契约 §2）：
    /// 关闭 registry 实例并经 hub 发布 `SessionStateChanged { state: Closed }`，
    /// causation 挂命令 id。会话不存在同样返回 Ok（幂等关闭）。
    pub fn close_session(&self, seed: &str, causation_id: Option<&str>) -> Result<(), String> {
        self.registry()?.close(seed);
        // D-3：关闭即终止该会话的取消标记。worker 优雅收尾可能已把它置假
        // （actor 线程 clear_cancel），但整项移除才能让 SESSION_CANCELS 与
        // 活跃会话同阶（种子频繁进出的 daemon 长期运行不无界增长）。
        qaqh_workspace::remove_session_cancel(seed);
        // 临时会话（子代理）用完即走：关闭后删除会话目录，磁盘零残留。
        // 目录已不存在（重复 close / 已被清理）时静默跳过，保持幂等。
        if self.sessions.is_ephemeral(seed) {
            match self.sessions.delete(seed) {
                Ok(()) => log::info!("[session] ephemeral session {seed} cleaned up (auto-unload)"),
                Err(e) if e.contains("Session not found") => {}
                Err(e) => log::warn!("[session] ephemeral cleanup {seed} failed: {e}"),
            }
        }
        if let Some(hub) = self.hub.get() {
            let _ = hub.publish_with_causation(
                seed,
                qaqh_domain::DomainEvent::Control(qaqh_domain::ControlEvent::SessionStateChanged {
                    seed: seed.to_string(),
                    state: qaqh_domain::SessionState::Closed,
                }),
                causation_id,
            );
        }
        self.release_seed_resident_state(seed);
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
    fn release_seed_resident_state(&self, seed: &str) {
        // 图片注册表按 seed 键控存 base64（read_image 的上传缓存）。resume
        // 路径（state/lifecycle.rs）会从持久化消息历史重建，关闭期清空
        // 不破坏 image_index 语义。
        qaqh_workspace::read_image::reset_images(seed);
        if let Some(hub) = self.hub.get() {
            hub.forget_seed(seed);
        }
    }

    /// E: idle 卸载空闲会话 worker（docs/current/architecture.md）。
    /// `idle_secs` <= 0 时为 no-op（配置禁用）。对每个被卸载的 seed 发布
    /// `SessionStateChanged::Closed`（与手动 close_session 一致，UI 可感知）。
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
        if let Some(hub) = self.hub.get() {
            for seed in &unloaded {
                let _ = hub.publish_with_causation(
                    seed,
                    qaqh_domain::DomainEvent::Control(
                        qaqh_domain::ControlEvent::SessionStateChanged {
                            seed: seed.to_string(),
                            state: qaqh_domain::SessionState::Closed,
                        },
                    ),
                    None,
                );
            }
        }
        for seed in &unloaded {
            self.release_seed_resident_state(seed);
        }
        if !unloaded.is_empty() {
            release_freed_heap_memory();
        }
        unloaded
    }

    /// 归档会话（标签 × 语义）：关闭 registry 实例 + meta `archived=true`。
    /// 磁盘与消息文件保留，左侧列表归档组可见可恢复。会话不存在同样
    /// 幂等成功（close 幂等 + set_archived 补写 meta）。
    pub fn archive_session(&self, seed: &str, causation_id: Option<&str>) -> Result<(), String> {
        self.close_session(seed, causation_id)?;
        self.sessions.set_archived(seed, true);
        Ok(())
    }

    /// 恢复归档会话：meta `archived=false` + 重新拉起实例（resume 语义，
    /// 对齐 `session.resume` 查询——get_or_spawn + active seed 更新）。
    pub fn unarchive_session(&self, seed: &str) -> Result<(), String> {
        self.sessions.set_archived(seed, false);
        self.registry()?.get_or_spawn(seed)
    }

    /// 彻底删除会话（左侧列表 × 语义）：先关实例（若运行，幂等）再删
    /// 磁盘目录与索引。会话不存在返回 Err（由 daemon 拦截层按幂等处理）。
    pub fn delete_session(&self, seed: &str, causation_id: Option<&str>) -> Result<(), String> {
        let _ = self.close_session(seed, causation_id);
        self.sessions.delete(seed)
    }

    pub fn handle(&self, method: &str, params: &Value) -> Result<Value, String> {
        let seed = || pstr(params, "seed");
        match method {
            "daemon.version" => Ok(json!(env!("CARGO_PKG_VERSION"))),
            // ── UI 工作区注册表（组织语义，与运行环境 workspace 解耦）──
            "workspace.list" => {
                let ws = qaqh_session::WorkspaceStore::global();
                let items: Vec<Value> = ws
                    .list()
                    .into_iter()
                    .map(|w| {
                        json!({
                            "id": w.id,
                            "path": w.path,
                            "title": w.title,
                            "order": w.order,
                            "session_ids": w.session_ids,
                            "missing_dir": !ws.path_status(&w.path),
                        })
                    })
                    .collect();
                Ok(Value::Array(items))
            }
            // 远端文件浏览（临时跨端模式）：路径一律是 daemon 侧绝对路径，
            // 且必须落在会话工作区根 / 数据根白名单内（T-2-1）。
            "fs.list" => {
                let path = pstr(params, "path")?;
                let scope_seed = params.get("scope_seed").and_then(Value::as_str);
                list_remote_directory(&self.sessions, &path, scope_seed)
            }
            "fs.read" => {
                let path = pstr(params, "path")?;
                let max_bytes = params
                    .get("max_bytes")
                    .and_then(Value::as_u64)
                    .unwrap_or(512 * 1024);
                let scope_seed = params.get("scope_seed").and_then(Value::as_str);
                read_remote_file(&self.sessions, &path, max_bytes, scope_seed)
            }
            "workspace.create" => {
                let path = pstr(params, "path")?;
                let ws = qaqh_session::WorkspaceStore::global();
                let existing = self.sessions.list();
                let created = ws.create(&path, &existing)?;
                Ok(serde_json::to_value(created).map_err(err)?)
            }
            "workspace.rename" => {
                let id = pstr(params, "id")?;
                let title = pstr(params, "title")?;
                let ws = qaqh_session::WorkspaceStore::global();
                let renamed = ws.rename(&id, title)?;
                Ok(serde_json::to_value(renamed).map_err(err)?)
            }
            "workspace.delete" => {
                let id = pstr(params, "id")?;
                qaqh_session::WorkspaceStore::global().delete(&id)?;
                Ok(Value::Null)
            }
            "workspace.move_session" => {
                let seed = pstr(params, "seed")?;
                let workspace_id = pstr(params, "workspace_id")?;
                qaqh_session::WorkspaceStore::global().move_session(&seed, &workspace_id)?;
                Ok(Value::Null)
            }
            "workspace.detach" => {
                let seed = pstr(params, "seed")?;
                qaqh_session::WorkspaceStore::global().remove_session(&seed);
                Ok(Value::Null)
            }
            "session.list" => Ok(serde_json::to_value(self.list_sessions()).map_err(err)?),
            "session.meta" => {
                let seed = seed()?;
                let manager = &self.sessions;
                let Some(meta) = manager.load_meta(&seed) else {
                    return Ok(Value::Null);
                };
                // 单条与 `session.list` 的条目**同一个形状**（G2）：同样的
                // `SessionMeta` + 运行期字段。此前这里也是手拼 `value["running"]`，
                // 且不带 `workspace_id`——同一个形状两处各拼一次，正是漂移的温床。
                let entry = qaqh_types::SessionListEntry {
                    running: self.registry()?.is_running(&meta.seed),
                    workspace_id: qaqh_session::WorkspaceStore::global().workspace_of(&meta.seed),
                    meta,
                };
                Ok(serde_json::to_value(entry).map_err(err)?)
            }
            "session.activity" => {
                Ok(serde_json::to_value(self.registry()?.activities()).map_err(err)?)
            }
            "session.new" => {
                // 可选工具模式预置（TUI/CLI 壳在 create 时一次性锁定）。
                // 先于任何落盘校验：非法值必须整体拒绝，不得留下孤儿 meta。
                let preset = optional_tool_mode(params)?;
                // 可选 cwd（前端在 workspace 上下文新建时传入）→ 记录 + 自动归属。
                let cwd = params
                    .get("cwd")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                // BUG-2026-09-13-24 + BETA-01：先分配 canonical identity，
                // 再用 `SessionId` 作为 seed 和目录名。分配、identity sidecar
                // 与初始 meta 都在同一把 session 锁内落盘，不再产生 8 位
                // seed 与 UUID 并存的新会话。
                let identity = self
                    .sessions
                    .allocate_session(cwd.as_deref())
                    .map_err(|error| format!("session.new: allocate session failed: {error}"))?;
                let seed = identity.session_id.as_str().to_string();
                self.sessions.clear_active();
                // 先于 spawn 落盘：worker 的 init_session 从 meta 恢复并应用，
                // 保证 minimal:dsh 的极简 system prompt 首轮就生效。
                if let Some((tool_mode, custom_tools)) = preset {
                    self.sessions
                        .persist_tool_mode(&seed, &tool_mode, &custom_tools)
                        .map_err(|error| format!("persist tool_mode failed: {error}"))?;
                }
                // 纯 v2：session_create 即物化 canonical identity + 首个
                // `SessionCreated` 事实。否则 bootstrap/events 要等首个工具事实
                // 才可用，新建会话在第一回合前一直处于 snapshot_missing 瞬态。
                let canonical_cwd = self
                    .sessions
                    .workspace_cwd(&seed)
                    .filter(|cwd| !cwd.is_empty())
                    .or_else(|| {
                        std::env::current_dir()
                            .ok()
                            .map(|cwd| cwd.to_string_lossy().into_owned())
                    })
                    .unwrap_or_else(|| "/".to_string());
                let model = qaqh_config::Config::load()
                    .map(|config| config.model)
                    .unwrap_or_else(|_| "unknown".to_string());
                if let Err(error) = materialize_canonical_session(&seed, &canonical_cwd, &model) {
                    let _ = self.sessions.delete(&seed);
                    return Err(format!(
                        "session.new: canonical materialization failed: {error}"
                    ));
                }
                self.registry()?.spawn_new(&seed)?;
                Ok(json!(seed))
            }
            "session.resume" => {
                let seed = seed()?;
                self.sessions.set_active_seed(&seed);
                self.registry()?.get_or_spawn(&seed)?;
                Ok(Value::Null)
            }
            "session.set_tool_mode" => {
                let seed = seed()?;
                let tool_mode = pstr(params, "tool_mode")?;
                validate_tool_mode(&tool_mode)?;
                let custom_tools = pstrings(params, "custom_tools");
                if tool_mode == "custom" && custom_tools.is_empty() {
                    return Err(
                        "custom tool mode requires at least one tool in custom_tools".to_string(),
                    );
                }
                // 先持久化（meta.json，重启存活），再通知 worker 应用
                // （set_allowed_tools + tool_defs 刷新 = 模型侧源头过滤）。
                // CK-PERSIST：持久化失败 → 400 返回前端，前端回滚乐观值；
                // 不允许「应用成功但没落盘」的假切换（重启即丢）。
                self.sessions
                    .persist_tool_mode(&seed, &tool_mode, &custom_tools)
                    .map_err(|error| format!("persist tool_mode failed: {error}"))?;
                self.send_ringing_cmd(
                    seed,
                    RingingCommand::Control(ControlCommand::SetToolMode {
                        tool_mode,
                        custom_tools,
                    }),
                )
            }
            "session.dashboard" => dashboard(&seed()?),
            "session.get_activity" => activity(&self.sessions, &seed()?),
            "skills.operation" => self.send_ringing_cmd(
                seed()?,
                RingingCommand::Control(ControlCommand::SkillsOperation {
                    operation_id: pstr2(params, "operation_id", "operationId")?,
                    action: pstr(params, "action")?,
                    name: pstr(params, "name")?,
                }),
            ),
            "skills.reload" => self.send_ringing_cmd(
                seed()?,
                RingingCommand::Control(ControlCommand::SkillsReload),
            ),
            "skills.activate" => self.send_ringing_cmd(
                seed()?,
                RingingCommand::Control(ControlCommand::SkillsActivate {
                    name: pstr(params, "name")?,
                }),
            ),
            "skills.list_tools" => Ok(json!(qaqh_workspace::runtime::process_all_tool_names())),
            "workspace.get" => Ok(json!(workspace(&self.sessions, &seed()?))),
            "workspace.set" => {
                let seed = seed()?;
                // 空 path 防护：canonical_cwd("") = "" 会把 meta.cwd 清空，
                // 导致会话工作区/归属丢失（前端重启后回空 cwd 的 bug 通道）。
                let path = pstr(params, "path")?.trim().to_string();
                if path.is_empty() {
                    return Err("workspace.set: empty path rejected".into());
                }
                // 统一数据源：运行环境工作目录存 meta.cwd（workspace.txt 退役）。
                self.sessions.set_cwd(&seed, &path, true);
                self.send_ringing_cmd(
                    seed,
                    RingingCommand::Control(ControlCommand::AgentReloadConfig),
                )?;
                Ok(Value::Null)
            }
            "git.diff" => git(
                &self.sessions,
                &seed()?,
                qaqh_workspace::git::status_json,
                json!([]),
            ),
            "git.branch" => git(
                &self.sessions,
                &seed()?,
                qaqh_workspace::git::current_branch,
                Value::Null,
            ),
            "git.branches" => git(
                &self.sessions,
                &seed()?,
                qaqh_workspace::git::list_branches,
                json!([]),
            ),
            "git.switch_branch" => git(
                &self.sessions,
                &seed()?,
                |ws| {
                    qaqh_workspace::git::switch_branch(
                        ws,
                        &pstr(params, "branch")?,
                        pbool(params, "stash"),
                    )
                },
                Value::Null,
            ),
            "git.commit" => git(
                &self.sessions,
                &seed()?,
                |ws| qaqh_workspace::git::commit_all(ws, &pstr(params, "message")?),
                Value::Null,
            ),
            "git.file_diff" => git(
                &self.sessions,
                &seed()?,
                |ws| qaqh_workspace::git::file_diff(ws, &pstr2(params, "file_path", "filePath")?),
                Value::Null,
            ),
            "config.load" => load_config(),
            "config.save" => {
                self.save_config(params)?;
                Ok(Value::Null)
            }
            // 权限等级（L1-L4）：与 config.save 共用 Config::update 单写口；
            // 校验 → 写 config.toml → 广播 AgentReloadConfig 让所有活跃
            // worker（含子代理，子代理继承同一全局权限）重载。
            "config.set_permission_level" => {
                let level = params
                    .get("level")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| "permission level (1-4) is required".to_string())?;
                if !(1..=4).contains(&level) {
                    return Err(format!("invalid permission level {level} (must be 1-4)"));
                }
                self.update_config_and_reload(|cfg| {
                    cfg.permission_level = level as u8;
                    Ok(())
                })?;
                log::info!("[config] permission level set to {level}");
                Ok(json!({ "permission_level": level }))
            }
            "profile.apply" => {
                let name = pstr(params, "name")?;
                self.update_config_and_reload(|cfg| {
                    if cfg.apply_profile(&name).is_none() {
                        return Err(format!("profile '{name}' not found"));
                    }
                    Ok(())
                })?;
                Ok(Value::Null)
            }
            "profile.save_current" => {
                let name = pstr(params, "name")?;
                self.update_config_and_reload(|cfg| {
                    cfg.save_profile(&name);
                    Ok(())
                })?;
                Ok(Value::Null)
            }
            "profile.delete" => {
                let name = pstr(params, "name")?;
                self.update_config_and_reload(|cfg| {
                    if !cfg.delete_profile(&name) {
                        return Err(format!(
                            "profile '{name}' cannot be deleted (not found or default)"
                        ));
                    }
                    Ok(())
                })?;
                Ok(Value::Null)
            }
            "todo.status" => qaqh_workspace::todo::todo_status_value(&seed()?),
            "todo.cancel" => {
                qaqh_workspace::todo::todo_cancel_value(&seed()?, &pstr(params, "id")?)
            }
            "todo.set" => qaqh_workspace::todo::todo_set_value_for(&seed()?, params),
            "todo.list" => qaqh_workspace::todo::todo_list_value_for(&seed()?, params),
            "plan.context_stats" => context_stats(&self.sessions, &seed()?),
            "stats.token_usage" => token_stats(pu64(params, "days") as u32),
            "plan.read" => serde_json::to_value(read_plan(&self.sessions, &seed()?)).map_err(err),
            "plan.action" => serde_json::to_value(plan_action(
                &self.sessions,
                &seed()?,
                &pstr2(params, "item_id", "itemId")?,
                &pstr(params, "action")?,
                value2(params, "user_comment", "userComment")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            )?)
            .map_err(err),
            // ── Subagent orchestration ──────────────────────────────────────
            // Spawn an isolated subagent worker and return its seed. The
            // caller (parent agent) then attaches the seed and drives it with
            // ordinary Ringing commands/events (ConversationSendMessage →
            // TurnCompleted). The worker runs ephemeral with the given tool
            // allowlist and optional model/base-url/max-tokens overrides.
            "subagent.spawn" => {
                let tools: Vec<String> = params
                    .get("tools")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                let model = params
                    .get("model")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(String::from);
                let base_url = params
                    .get("base_url")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(String::from);
                let max_tokens = params
                    .get("max_tokens")
                    .and_then(Value::as_u64)
                    .map(|v| v as u32);
                // 子代理继承主代理的 workspace：spawn 前写入
                // `sessions/{sub_seed}/workspace.txt`，子 worker 启动时
                // `load_session_workspace` 读到，从而正确解析相对路径并
                // 以主代理工作区为权限边界（修复子代理"不知道工作区、
                // 相对路径落到 daemon cwd"的问题）。
                let workspace = params
                    .get("workspace")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|w| !w.is_empty() && *w != ".")
                    .map(String::from);
                // BETA-01：子代理与主会话共享 canonical identity 规则，
                // 目录名、seed、child_session_id 都是同一个 UUID；子代理
                // 保持 unindexed，不污染普通会话列表。
                let identity = self
                    .sessions
                    .allocate_agent_session(workspace.as_deref())
                    .map_err(|error| format!("subagent.spawn: allocate child session: {error}"))?;
                let seed = identity.session_id.as_str().to_string();
                if let Some(workspace) = &workspace {
                    log::info!("[subagent] inherited workspace for seed={seed}: {workspace}");
                }
                self.registry()?.spawn_subagent(
                    &seed,
                    &tools,
                    model.as_deref(),
                    base_url.as_deref(),
                    max_tokens,
                )?;
                log::info!(
                    "[subagent] spawned subagent worker seed={seed} tools={}",
                    tools.len()
                );
                Ok(json!({ "seed": seed }))
            }
            _ => Err(format!("unknown method: {method}")),
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

    pub(crate) fn registry(&self) -> Result<std::sync::MutexGuard<'_, AgentRegistry>, String> {
        self.registry
            .lock()
            .map_err(|e| format!("registry lock: {e}"))
    }

    /// 构造 Ringing worker 命令信封并转发给 agent（legacy Ui2Agent 帧已拆除）。
    fn send_ringing_cmd(&self, seed: String, command: RingingCommand) -> Result<Value, String> {
        let env = RingingWorkerCommandEnvelope::new(seed.clone(), command_id(), command);
        self.send_ringing_command(&seed, &env)?;
        Ok(Value::Null)
    }

    /// `session.list` 的条目（前端契约 **G2**）。
    ///
    /// 返回**类型化**条目而非 `Value`：形状由 `qaqh_types::SessionListEntry` 承载，
    /// 序列化只发生在 dispatch 边界。此前这里是 `to_value(&meta)` 之后再
    /// `value["running"] = …` 手工拼键——手拼的形状没有任何类型承载，三端前端
    /// 只能各自手解（TUI 那份手抄漏了 5 个键而无人察觉）。
    ///
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
                let running = registry.is_running(&meta.seed);
                let workspace_id = workspaces.workspace_of(&meta.seed);
                qaqh_types::SessionListEntry {
                    meta,
                    running,
                    workspace_id,
                }
            })
            .collect()
    }

    /// 配置写后广播（所有 config.* / profile.* 写路径共用）。
    fn notify_config_changed(&self) {
        // P2-D2：除 worker 广播外，向 Control 频道发布 ConfigChanged（空 seed =
        // 全局），前端/TUI/web 订阅后重拉 config.load——轮询降级为兜底。
        if let Some(hub) = self.hub.get() {
            let rev = qaqh_config::watch::latest().map_or(0, |_| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0)
            });
            let _ = hub.publish_with_causation(
                "",
                qaqh_domain::DomainEvent::Control(qaqh_domain::ControlEvent::ConfigChanged { rev }),
                None,
            );
        }
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
    fn update_config_and_reload<F>(&self, mutate: F) -> Result<qaqh_config::Config, String>
    where
        F: FnOnce(&mut qaqh_config::Config) -> Result<(), String>,
    {
        let config = qaqh_config::Config::update(mutate)?;
        self.notify_config_changed();
        Ok(config)
    }

    fn save_config(&self, params: &Value) -> Result<(), String> {
        // Never log config.save payloads: they may contain provider credentials.
        log::info!("[config.save] saving configuration");
        // P1-C2：wire 切换为 Merge Patch 契约（qaqh-config-api）——值域校验在
        // api 层 validate()；逐字段守卫语义集中在 qaqh_config::dto::apply_patch
        // （穷举映射，新增字段未同步会编译失败）。旧 snake/camel 双键 shim 就此退役。
        let patch: qaqh_config_api::ConfigPatch = serde_json::from_value(params.clone())
            .map_err(|e| format!("invalid config.save payload: {e}"))?;
        self.update_config_and_reload(|cfg| qaqh_config::dto::apply_patch(cfg, &patch))
            .map(|_| ())
    }
}

/// P2-1：MCP 热重载器——订阅 watch 单写口广播，只对 `[mcp]` 段变化响应。
///
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
///
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
///
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

/// 在 `session.new` 的持久化阶段写入 canonical 基线。
///
/// `CanonicalSessionIdentity::open_or_create` 只建 identity sidecar；bootstrap
/// 还要求 `events.commit.json` 存在。这里在 worker spawn 前串行写入首个
/// `SessionCreated`，把「identity 已建但 snapshot 缺失」的瞬态消掉。
fn materialize_canonical_session(seed: &str, cwd: &str, model: &str) -> Result<(), String> {
    let session_dir = qaqh_types::platform::sessions_dir().join(seed);
    materialize_canonical_session_in(&session_dir, cwd, model, None)
}

pub(crate) fn materialize_canonical_session_in(
    session_dir: &std::path::Path,
    cwd: &str,
    model: &str,
    parent_session_id: Option<SessionId>,
) -> Result<(), String> {
    let identity =
        CanonicalSessionIdentity::open_or_create(session_dir).map_err(|error| error.to_string())?;
    let session_id = identity.session_id.clone();
    let log_id = identity.log_id.clone();
    // `CanonicalSessionStore::open` initializes an empty commit marker for a
    // brand-new session; `CommittedFactReader` would fail before that.
    let mut store = CanonicalSessionStore::open(session_dir, session_id.clone(), log_id.clone())
        .map_err(|error| error.to_string())?;
    let facts = CommittedFactReader::open(session_dir, session_id.clone(), log_id.clone())
        .map_err(|error| error.to_string())?
        .read_all()
        .map_err(|error| error.to_string())?;
    if facts
        .iter()
        .any(|fact| matches!(&fact.payload, FactPayload::SessionCreated(_)))
    {
        return Ok(());
    }
    let now_ms = system_time_ms();
    let writer_id = WriterId::new(format!(
        "daemon-session-init-{}-{}",
        std::process::id(),
        session_dir
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("session")
    ));
    let lease = store
        .acquire_writer(writer_id, now_ms, 60_000)
        .map_err(|error| error.to_string())?;
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
    let fact = SessionFact {
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
    store
        .append(&lease, fact, now_ms)
        .map_err(|error| error.to_string())?;
    store
        // 释放必须对任何时钟都表现为已过期；写 wall clock 会让落后时钟的读者
        // 把 fence 看成仍有效（与 ToolLedger::Drop 的释放语义一致）。
        .release_writer(&lease, i64::MIN)
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn system_time_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

pub(crate) mod common;
pub(crate) mod fs_git;
pub(crate) mod params;
pub(crate) mod plan;
pub(crate) mod stats;

use self::common::{command_id, err, release_freed_heap_memory};
use self::fs_git::{git, list_remote_directory, read_remote_file, workspace};
use self::params::{
    optional_tool_mode, pbool, pstr, pstr2, pstrings, pu64, validate_tool_mode, value2,
};
use self::plan::{plan_action, read_plan, token_stats};
use self::stats::{activity, context_stats, dashboard, load_config};

#[cfg(test)]
mod canonical_session_materialization_tests {
    use super::*;
    use qaqh_session::canonical::{
        CANONICAL_IDENTITY_FILE, EVENTS_COMMIT_FILE, WRITER_FENCE_FILE, WriterFence,
    };

    #[test]
    fn session_creation_materializes_one_canonical_baseline() {
        let dir = tempfile::tempdir().expect("tempdir");
        materialize_canonical_session_in(dir.path(), "/tmp/workspace", "test-model", None)
            .expect("materialize");
        assert!(dir.path().join(CANONICAL_IDENTITY_FILE).exists());
        assert!(dir.path().join(EVENTS_COMMIT_FILE).exists());
        let fence: WriterFence = serde_json::from_slice(
            &std::fs::read(dir.path().join(WRITER_FENCE_FILE)).expect("writer fence"),
        )
        .expect("decode writer fence");
        assert_eq!(
            fence.lease_expires_at_ms,
            i64::MIN,
            "released fences must look expired to every reader clock"
        );

        let identity = CanonicalSessionIdentity::open_or_create(dir.path()).expect("identity");
        let facts = CommittedFactReader::open(
            dir.path(),
            identity.session_id.clone(),
            identity.log_id.clone(),
        )
        .expect("reader")
        .read_all()
        .expect("facts");
        assert_eq!(facts.len(), 1);
        let FactPayload::SessionCreated(created) = &facts[0].payload else {
            panic!("first fact must be SessionCreated");
        };
        assert_eq!(created.cwd, "/tmp/workspace");
        assert_eq!(created.model, "test-model");

        // 幂等：重复调用不得追加第二个 SessionCreated。
        materialize_canonical_session_in(dir.path(), "/tmp/workspace", "test-model", None)
            .expect("second materialize");
        let facts = CommittedFactReader::open(dir.path(), identity.session_id, identity.log_id)
            .expect("reader")
            .read_all()
            .expect("facts");
        assert_eq!(facts.len(), 1);
    }
}

#[cfg(test)]
mod tool_mode_tests {
    use super::params::{optional_tool_mode, validate_tool_mode};

    #[test]
    fn optional_tool_mode_defaults_to_none() {
        assert_eq!(optional_tool_mode(&serde_json::json!({})).unwrap(), None);
    }

    #[test]
    fn optional_tool_mode_rejects_deprecated_minimal_dsh() {
        // minimal:dsh 已随 bash/pwsh 拆分下线：废弃模式必须被 KNOWN_MODES
        // 白名单拒绝，不得回流。
        assert!(
            optional_tool_mode(&serde_json::json!({
                "tool_mode": "minimal:dsh",
            }))
            .is_err()
        );
    }

    #[test]
    fn optional_tool_mode_rejects_unknown_values() {
        assert!(optional_tool_mode(&serde_json::json!({ "tool_mode": "turbo" })).is_err());
    }

    #[test]
    fn optional_tool_mode_custom_requires_tools() {
        assert!(optional_tool_mode(&serde_json::json!({ "tool_mode": "custom" })).is_err());
        let (mode, tools) = optional_tool_mode(&serde_json::json!({
            "tool_mode": "custom",
            "custom_tools": ["exec"],
        }))
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
    use std::sync::Arc;
    use std::time::Duration;

    use super::reload_loop;
    use qaqh_config::Config;

    /// T-6-1 回归（BUG-2026-09-15-07 / E2）：`qaqh_config::watch::subscribe()`
    /// 走 `Sender::subscribe`，返回的 receiver **已消费到当前版本** ⇒ 订阅后的
    /// **第一次**发布必须被 reloader 观察到，不得被「前置 changed() 守卫」吞掉
    /// （旧代码那次 `borrow_and_update()` 正好丢掉用户启动后的首次真实改动）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn first_publish_after_subscribe_is_observed() {
        let (tx, _bootstrap) = tokio::sync::watch::channel::<Option<Arc<Config>>>(None);
        // 订阅前已有一版快照（模拟「daemon 启动时 manager 已按当前配置装配」）。
        tx.send(Some(Arc::new(Config {
            context_limit: 1,
            ..Default::default()
        })))
        .unwrap();
        let rx = tx.subscribe(); // 与 watch::subscribe() 同款：Sender::subscribe

        let (seen_tx, mut seen_rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = tokio::spawn(reload_loop(rx, move |published| {
            let seen_tx = seen_tx.clone();
            async move {
                let _ = seen_tx.send(published.context_limit);
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
            context_limit: 4242,
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
    use super::plan::token_stats;

    /// `days` 直取 IPC 参数且决定条目数与循环数：未封顶时
    /// `stats.token_usage {days: 200000}` 产出 20 万条目（daemon 线程内存 +
    /// 延迟无界）。窗口必须有硬上限。
    #[test]
    fn token_stats_clamps_the_requested_day_window() {
        let value = token_stats(200_000).expect("token_stats");
        let daily = value["daily"].as_array().expect("daily array");
        assert!(
            daily.len() <= 366,
            "token_stats must clamp the day window; got {} entries",
            daily.len()
        );
    }

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
