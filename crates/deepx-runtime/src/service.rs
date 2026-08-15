use std::io::BufRead;
use std::sync::{Arc, Mutex};

use deepx_domain::{
    ControlCommand, ConversationCommand, ConversationMode, ImageBlock, ToolCommand,
};
use deepx_proto::SessionActivityState;
use deepx_ringing::{RingingCommand, RingingWorkerCommandEnvelope};
use serde_json::{Value, json};

use crate::{AgentRegistry, RingingHub};

/// workspace serve 当前运行状态（daemon 内存态，供前端展示诊断依据）。
#[derive(Debug, Clone, Default)]
pub struct WorkspaceRuntimeState {
    /// 配置的模式（config.toml [workspace] mode）。
    pub configured_mode: String,
    /// 实际运行模式（WSL 降级后为 local）。
    pub active_mode: String,
    /// serve endpoint（空 = 未启用）。
    pub endpoint: String,
    /// 每次 workspace 守护重启后单调递增；不含任何凭据。
    pub generation: u64,
}

#[derive(Clone)]
pub struct DeepxService {
    registry: Arc<Mutex<AgentRegistry>>,
    hub: std::sync::OnceLock<Arc<RingingHub>>,
    workspace_state: Arc<Mutex<WorkspaceRuntimeState>>,
}

impl DeepxService {
    pub fn init() -> Self {
        let _config = deepx_config::Config::load().unwrap_or_default();
        deepx_session::SessionManager::init(deepx_types::platform::data_dir());
        Self {
            registry: Arc::new(Mutex::new(AgentRegistry::new())),
            hub: std::sync::OnceLock::new(),
            workspace_state: Arc::new(Mutex::new(WorkspaceRuntimeState::default())),
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

    /// 转发 Ringing 命令到 agent worker（wire 判别后由 worker reader 解析）。
    pub fn send_ringing_command(
        &self,
        seed: &str,
        env: &deepx_ringing::RingingWorkerCommandEnvelope,
    ) -> Result<(), String> {
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .send_ringing(seed, env)
    }

    /// 关闭会话（Ringing `SessionClose` 命令语义，契约 §2）：
    /// 关闭 registry 实例并经 hub 发布 `SessionStateChanged { state: Closed }`，
    /// causation 挂命令 id。会话不存在同样返回 Ok（幂等关闭）。
    pub fn close_session(&self, seed: &str, causation_id: Option<&str>) -> Result<(), String> {
        self.registry()?.close(seed);
        // 临时会话（子代理）用完即走：关闭后删除会话目录，磁盘零残留。
        // 目录已不存在（重复 close / 已被清理）时静默跳过，保持幂等。
        if deepx_session::SessionManager::global().is_ephemeral(seed) {
            match deepx_session::SessionManager::global().delete(seed) {
                Ok(()) => log::info!("[session] ephemeral session {seed} cleaned up (auto-unload)"),
                Err(e) if e.contains("Session not found") => {}
                Err(e) => log::warn!("[session] ephemeral cleanup {seed} failed: {e}"),
            }
        }
        if let Some(hub) = self.hub.get() {
            let _ = hub.publish_with_causation(
                seed,
                deepx_domain::DomainEvent::Control(
                    deepx_domain::ControlEvent::SessionStateChanged {
                        seed: seed.to_string(),
                        state: deepx_domain::SessionState::Closed,
                    },
                ),
                causation_id,
            );
        }
        Ok(())
    }

    /// 归档会话（标签 × 语义）：关闭 registry 实例 + meta `archived=true`。
    /// 磁盘与消息文件保留，左侧列表归档组可见可恢复。会话不存在同样
    /// 幂等成功（close 幂等 + set_archived 补写 meta）。
    pub fn archive_session(&self, seed: &str, causation_id: Option<&str>) -> Result<(), String> {
        self.close_session(seed, causation_id)?;
        deepx_session::SessionManager::global().set_archived(seed, true);
        Ok(())
    }

    /// 恢复归档会话：meta `archived=false` + 重新拉起实例（resume 语义，
    /// 对齐 `session.resume` 查询——get_or_spawn + active seed 更新）。
    pub fn unarchive_session(&self, seed: &str) -> Result<(), String> {
        deepx_session::SessionManager::global().set_archived(seed, false);
        self.registry()?.get_or_spawn(seed)
    }

    /// 彻底删除会话（左侧列表 × 语义）：先关实例（若运行，幂等）再删
    /// 磁盘目录与索引。会话不存在返回 Err（由 daemon 拦截层按幂等处理）。
    pub fn delete_session(&self, seed: &str, causation_id: Option<&str>) -> Result<(), String> {
        let _ = self.close_session(seed, causation_id);
        deepx_session::SessionManager::global().delete(seed)
    }

    pub fn session_scoped(method: &str) -> bool {
        matches!(
            method.split('.').next(),
            Some("session" | "interaction" | "workspace" | "git" | "plan" | "skills" | "todo")
        ) && !matches!(
            method,
            "session.list" | "session.activity" | "session.new" | "skills.list_tools"
        )
    }

    pub fn handle(&self, method: &str, params: &Value) -> Result<Value, String> {
        let seed = || pstr(params, "seed");
        match method {
            "daemon.version" => Ok(json!(env!("CARGO_PKG_VERSION"))),
            "workspace.set_mode" => {
                let mode = pstr(params, "mode")?;
                let supported = if cfg!(target_os = "windows") {
                    matches!(mode.as_str(), "local" | "wsl")
                } else {
                    // Linux 原生系统：工具本来就在 Linux 环境，无 WSL 选项。
                    mode == "local"
                };
                if !supported {
                    return Err(format!(
                        "invalid workspace mode '{mode}' (supported: {})",
                        if cfg!(target_os = "windows") {
                            "local | wsl"
                        } else {
                            "local"
                        }
                    ));
                }
                let mut config =
                    deepx_config::Config::load().map_err(|e| format!("load config: {e}"))?;
                config.workspace.mode = mode.clone();
                config.save().map_err(|e| format!("save config: {e}"))?;
                log::info!("[workspace] mode switched to {mode} (restart required)");
                Ok(json!({
                    "mode": mode,
                    "restart_required": true,
                }))
            }
            "workspace.status" => {
                let state = self
                    .workspace_state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                Ok(json!({
                    "configured_mode": state.configured_mode,
                    "active_mode": state.active_mode,
                    "endpoint": state.endpoint,
                    "generation": state.generation,
                }))
            }
            // ── UI 工作区注册表（组织语义，与运行环境 workspace 解耦）──
            "workspace.list" => {
                let ws = deepx_session::WorkspaceStore::global();
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
            "workspace.create" => {
                let path = pstr(params, "path")?;
                let ws = deepx_session::WorkspaceStore::global();
                let existing = deepx_session::SessionManager::global().list();
                let created = ws.create(&path, &existing)?;
                Ok(serde_json::to_value(created).map_err(err)?)
            }
            "workspace.rename" => {
                let id = pstr(params, "id")?;
                let title = pstr(params, "title")?;
                let ws = deepx_session::WorkspaceStore::global();
                let renamed = ws.rename(&id, title)?;
                Ok(serde_json::to_value(renamed).map_err(err)?)
            }
            "workspace.delete" => {
                let id = pstr(params, "id")?;
                deepx_session::WorkspaceStore::global().delete(&id)?;
                Ok(Value::Null)
            }
            "workspace.move_session" => {
                let seed = pstr(params, "seed")?;
                let workspace_id = pstr(params, "workspace_id")?;
                deepx_session::WorkspaceStore::global()
                    .move_session(&seed, &workspace_id)?;
                Ok(Value::Null)
            }
            "workspace.detach" => {
                let seed = pstr(params, "seed")?;
                deepx_session::WorkspaceStore::global().remove_session(&seed);
                Ok(Value::Null)
            }
            "workspace.diagnose" => Ok(crate::workspace_supervisor::diagnose_wsl().map_err(err)?),
            "workspace.install_wsl" => {
                let repo_root = params
                    .get("repo_root")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                Ok(crate::workspace_supervisor::install_wsl(repo_root.as_deref()).map_err(err)?)
            }
            "session.list" => Ok(Value::Array(self.list_sessions())),
            "session.meta" => {
                let seed = seed()?;
                let manager = deepx_session::SessionManager::global();
                let Some(meta) = manager.load_meta(&seed) else {
                    return Ok(Value::Null);
                };
                let mut value = serde_json::to_value(&meta).map_err(err)?;
                value["running"] = json!(self.registry()?.is_running(&meta.seed));
                Ok(value)
            }
            "session.activity" => {
                Ok(serde_json::to_value(self.registry()?.activities()).map_err(err)?)
            }
            "session.new" => {
                let seed = deepx_session::SessionManager::generate_seed();
                deepx_session::SessionManager::global().clear_active();
                // 可选 cwd（前端在 workspace 上下文新建时传入）→ 记录 + 自动归属。
                let cwd = params
                    .get("cwd")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                deepx_session::SessionManager::global()
                    .persist_new_session_with_cwd(&seed, cwd.as_deref());
                self.registry()?.spawn_new(&seed)?;
                Ok(json!(seed))
            }
            "session.resume" => {
                let seed = seed()?;
                deepx_session::SessionManager::global().set_active_seed(&seed);
                self.registry()?.get_or_spawn(&seed)?;
                Ok(Value::Null)
            }
            "session.send_message" => {
                let seed = seed()?;
                let text = pstr(params, "text")?;
                let files = pstrings(params, "files");
                let text = with_file_previews(text, &files);
                let images: Vec<ImageBlock> = params
                    .get("images")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|img| {
                                Some(ImageBlock {
                                    mime_type: img.get("mimeType")?.as_str()?.to_string(),
                                    data: img.get("data")?.as_str()?.to_string(),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                self.send_user_input(seed, text, images)
            }
            "session.set_mode" => self.send_ringing_cmd(
                seed()?,
                RingingCommand::Conversation(ConversationCommand::ConversationSetMode {
                    mode: parse_conversation_mode(&pstr(params, "mode")?)?,
                }),
            ),
            "session.set_tool_mode" => {
                let seed = seed()?;
                let tool_mode = pstr(params, "tool_mode")?;
                if !matches!(tool_mode.as_str(), "standard" | "minimal" | "custom") {
                    return Err(format!(
                        "invalid tool_mode '{tool_mode}' (expected standard | minimal | custom)"
                    ));
                }
                let custom_tools = pstrings(params, "custom_tools");
                if tool_mode == "custom" && custom_tools.is_empty() {
                    return Err(
                        "custom tool mode requires at least one tool in custom_tools".to_string(),
                    );
                }
                // 先持久化（meta.json，重启存活），再通知 worker 应用
                // （set_allowed_tools + tool_defs 刷新 = 模型侧源头过滤）。
                deepx_session::SessionManager::global()
                    .persist_tool_mode(&seed, &tool_mode, &custom_tools);
                self.send_ringing_cmd(
                    seed,
                    RingingCommand::Control(ControlCommand::SetToolMode {
                        tool_mode,
                        custom_tools,
                    }),
                )
            }
            "session.cancel" => self.send_ringing_cmd(
                seed()?,
                RingingCommand::Conversation(ConversationCommand::ConversationCancel {
                    turn_id: None,
                }),
            ),
            "session.compact" => self.compact_idle_session(seed()?),
            "session.undo_turn" => self.send_ringing_cmd(
                seed()?,
                RingingCommand::Conversation(ConversationCommand::ConversationUndoTurn {
                    turn_id: pstr2(params, "turn_id", "turnId")?,
                }),
            ),
            "session.load_more_turns" => self.send_ringing_cmd(
                seed()?,
                RingingCommand::Conversation(ConversationCommand::ConversationLoadMore {
                    before_turn_id: pstr2(params, "before_turn_id", "beforeTurnId")?,
                    count: 20,
                }),
            ),
            "session.close" => {
                self.registry()?.close(&seed()?);
                Ok(Value::Null)
            }
            "session.delete" => {
                let seed = seed()?;
                self.registry()?.close(&seed);
                deepx_session::SessionManager::global().delete(&seed)?;
                Ok(Value::Null)
            }
            "session.dashboard" => dashboard(&seed()?),
            "session.get_activity" => activity(&seed()?),
            "interaction.permission" => self.send_ringing_cmd(
                seed()?,
                RingingCommand::Tool(ToolCommand::ToolPermissionRespond {
                    tool_call_id: pstr2(params, "tool_call_id", "toolCallId")?,
                    approved: pbool(params, "approved"),
                    trust_folder: pbool2(params, "trust_folder", "trustFolder"),
                }),
            ),
            "interaction.ask_response" => self.send_ringing_cmd(
                seed()?,
                RingingCommand::Control(ControlCommand::InteractionAskRespond {
                    interaction_id: pstr2(params, "ask_id", "askId")?,
                    answers: serde_json::from_value(
                        params.get("answers").cloned().unwrap_or_else(|| json!([])),
                    )
                    .map_err(err)?,
                }),
            ),
            "interaction.ask_dismiss" => self.send_ringing_cmd(
                seed()?,
                RingingCommand::Control(ControlCommand::InteractionAskDismiss {
                    interaction_id: pstr2(params, "ask_id", "askId")?,
                }),
            ),
            "interaction.plan_review" => self.send_ringing_cmd(
                seed()?,
                RingingCommand::Control(ControlCommand::PlanReviewRespond {
                    interaction_id: pstr2(params, "call_id", "callId")?,
                    approved: pbool(params, "approved"),
                    message: params
                        .get("message")
                        .and_then(Value::as_str)
                        .filter(|m| !m.is_empty())
                        .map(str::to_string),
                    autonomous: pbool(params, "autonomous"),
                }),
            ),
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
            "skills.list_tools" => Ok(json!(deepx_workspace::runtime::all_tool_names())),
            "workspace.get" => Ok(json!(workspace(&seed()?))),
            "workspace.set" => {
                let seed = seed()?;
                // 统一数据源：运行环境工作目录存 meta.cwd（workspace.txt 退役）。
                deepx_session::SessionManager::global().set_cwd(
                    &seed,
                    pstr(params, "path")?.trim(),
                    true,
                );
                self.send_ringing_cmd(
                    seed,
                    RingingCommand::Control(ControlCommand::AgentReloadConfig),
                )?;
                Ok(Value::Null)
            }
            "git.diff" => git(
                &seed()?,
                |ws| deepx_workspace::git::status_json(ws),
                json!([]),
            ),
            "git.branch" => git(
                &seed()?,
                |ws| deepx_workspace::git::current_branch(ws),
                Value::Null,
            ),
            "git.branches" => git(
                &seed()?,
                |ws| deepx_workspace::git::list_branches(ws),
                json!([]),
            ),
            "git.switch_branch" => git(
                &seed()?,
                |ws| {
                    deepx_workspace::git::switch_branch(
                        ws,
                        &pstr(params, "branch")?,
                        pbool(params, "stash"),
                    )
                },
                Value::Null,
            ),
            "git.commit" => git(
                &seed()?,
                |ws| deepx_workspace::git::commit_all(ws, &pstr(params, "message")?),
                Value::Null,
            ),
            "git.file_diff" => git(
                &seed()?,
                |ws| deepx_workspace::git::file_diff(ws, &pstr2(params, "file_path", "filePath")?),
                Value::Null,
            ),
            "config.load" => load_config(),
            "config.save" => {
                self.save_config(params)?;
                Ok(Value::Null)
            }
            // 权限等级（L1-L4）：前端独立通道（不参与 config.save 全量提交）。
            // 此前 daemon 从未实现该 action —— 前端能选但配置永远是默认 4
            // (Unrestricted)，agent 永远不会受限。这里补上：校验 → 写
            // config.toml → 广播 AgentReloadConfig 让所有活跃 worker
            // （含子代理，子代理继承同一全局权限）重载。
            "config.set_permission_level" => {
                let level = params
                    .get("level")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| "permission level (1-4) is required".to_string())?;
                if !(1..=4).contains(&level) {
                    return Err(format!("invalid permission level {level} (must be 1-4)"));
                }
                let mut cfg = deepx_config::Config::load().unwrap_or_default();
                cfg.permission_level = level as u8;
                cfg.save().map_err(|e| format!("save config: {e}"))?;
                let failed = self.registry()?.broadcast_ringing(
                    &RingingCommand::Control(ControlCommand::AgentReloadConfig),
                );
                if !failed.is_empty() {
                    log::warn!(
                        "[config] permission reload broadcast failed for: {}",
                        failed.join("; ")
                    );
                }
                log::info!("[config] permission level set to {level}");
                Ok(json!({ "permission_level": level }))
            }
            "profile.apply" => {
                let name = pstr(params, "name")?;
                let mut cfg = deepx_config::Config::load().map_err(err)?;
                if cfg.apply_profile(&name).is_none() {
                    return Err(format!("profile '{name}' not found"));
                }
                Ok(Value::Null)
            }
            "profile.save_current" => {
                let name = pstr(params, "name")?;
                let mut cfg = deepx_config::Config::load().map_err(err)?;
                cfg.save_profile(&name);
                Ok(Value::Null)
            }
            "profile.delete" => {
                let name = pstr(params, "name")?;
                let mut cfg = deepx_config::Config::load().map_err(err)?;
                if !cfg.delete_profile(&name) {
                    return Err(format!(
                        "profile '{name}' cannot be deleted (not found or default)"
                    ));
                }
                Ok(Value::Null)
            }
            "todo.status" => parse_json_string(deepx_workspace::todo::todo_status_json(&seed()?)?),
            "todo.cancel" => parse_json_string(deepx_workspace::todo::todo_cancel_json(
                &seed()?,
                &pstr(params, "id")?,
            )?),
            "plan.context_stats" => context_stats(&seed()?),
            "stats.token_usage" => token_stats(pu64(params, "days") as u32),
            "plan.read" => read_plan(&seed()?),
            "plan.action" => {
                plan_action(
                    &seed()?,
                    &pstr2(params, "item_id", "itemId")?,
                    &pstr(params, "action")?,
                    value2(params, "user_comment", "userComment")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                )?;
                Ok(Value::Null)
            }
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
                let seed = deepx_session::SessionManager::generate_seed();
                if let Some(workspace) = &workspace {
                    // 子代理继承主代理的 workspace：写入 meta.cwd（统一数据源），
                    // 子 worker 启动时 `load_session_workspace` 读到，从而正确解析
                    // 相对路径并以主代理工作区为权限边界。
                    deepx_session::SessionManager::global().set_cwd(&seed, workspace, false);
                    log::info!(
                        "[subagent] inherited workspace for seed={seed}: {workspace}"
                    );
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
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .activities()
            .iter()
            .any(|activity| {
                matches!(
                    activity.state,
                    SessionActivityState::Starting
                        | SessionActivityState::Working
                        | SessionActivityState::WaitingUser
                )
            })
    }

    fn registry(&self) -> Result<std::sync::MutexGuard<'_, AgentRegistry>, String> {
        self.registry
            .lock()
            .map_err(|e| format!("registry lock: {e}"))
    }

    /// 注入 workspace serve 连接信息与运行模式（worker spawn 时写入 env）。
    /// `mode` ∈ {"local", "wsl"}：local 工具执行保持进程内，wsl 才启用远程 HTTP。
    pub fn attach_workspace(&self, endpoint: String, token: String, mode: &str) {
        if let Ok(mut registry) = self.registry() {
            registry.attach_workspace(endpoint, token, mode);
        }
    }

    /// 记录 workspace serve 实际运行状态（server.rs 启动 supervisor 后调用）。
    pub fn attach_workspace_state(&self, state: WorkspaceRuntimeState) {
        *self
            .workspace_state
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = state;
    }

    /// 构造 Ringing worker 命令信封并转发给 agent（legacy Ui2Agent 帧已拆除）。
    fn send_ringing_cmd(&self, seed: String, command: RingingCommand) -> Result<Value, String> {
        let env = RingingWorkerCommandEnvelope::new(seed.clone(), command_id(), command);
        self.send_ringing_command(&seed, &env)?;
        Ok(Value::Null)
    }

    fn send_user_input(
        &self,
        seed: String,
        text: String,
        images: Vec<ImageBlock>,
    ) -> Result<Value, String> {
        let mut registry = self.registry()?;
        // An inactive persisted session has no activity entry yet. Spawn it
        // first so the Starting -> Working reservation below also covers the
        // initialization Ready / first UserInput race.
        registry.get_or_spawn(&seed)?;
        let current = registry.activity(&seed);
        // Preserve the existing ability to queue follow-ups while a real turn
        // is running. A Working state without turn_id is different: it is an
        // atomic pre-TurnStart or manual-compact transaction, so another input
        // cannot safely be admitted yet.
        if current.as_ref().is_some_and(|activity| {
            activity.state == deepx_proto::SessionActivityState::Working
                && activity.turn_id.is_none()
        }) {
            return Err("session message is blocked by an active context transaction".into());
        }

        // Starting/Idle must be reserved before writing the frame. Reserving
        // afterwards can race a very short turn that already emitted Done and
        // incorrectly move the activity back from Idle to Working.
        let reservation = match current.as_ref().map(|activity| activity.state) {
            Some(
                deepx_proto::SessionActivityState::Starting
                | deepx_proto::SessionActivityState::Idle,
            ) => registry.reserve_for_input(&seed),
            _ => None,
        };
        if matches!(
            current.as_ref().map(|activity| activity.state),
            Some(
                deepx_proto::SessionActivityState::Starting
                    | deepx_proto::SessionActivityState::Idle
            )
        ) && reservation.is_none()
        {
            return Err("session activity changed before message admission".into());
        }
        if let Some((activity, _)) = reservation.as_ref() {
            crate::activity::publish_activity(self.hub.get().map(|v| &**v), activity);
        }
        let env = RingingWorkerCommandEnvelope::new(
            seed.clone(),
            command_id(),
            RingingCommand::Conversation(ConversationCommand::ConversationSendMessage {
                text,
                images,
                attachments: None,
                as_system: false,
            }),
        );
        if let Err(error) = registry.send_ringing(&seed, &env) {
            if let Some((activity, previous)) = reservation
                && let Some(rollback) =
                    registry.rollback_input_reservation(&seed, activity.seq, previous)
            {
                crate::activity::publish_activity(self.hub.get().map(|v| &**v), &rollback);
            }
            return Err(error);
        }
        Ok(Value::Null)
    }

    fn compact_idle_session(&self, seed: String) -> Result<Value, String> {
        let mut registry = self.registry()?;
        let reservation = registry.reserve_idle(&seed).ok_or_else(|| {
            let state = registry
                .activity(&seed)
                .map(|activity| format!("{:?}", activity.state))
                .unwrap_or_else(|| "unknown".into());
            format!("session compact requires an idle session; current state: {state}")
        })?;
        let reservation_seq = reservation.seq;
        crate::activity::publish_activity(self.hub.get().map(|v| &**v), &reservation);

        let env = RingingWorkerCommandEnvelope::new(
            seed.clone(),
            command_id(),
            RingingCommand::Conversation(ConversationCommand::ConversationCompact {
                turn_id: None,
            }),
        );
        if let Err(error) = registry.send_ringing(&seed, &env) {
            if let Some(rollback) = registry.rollback_idle_reservation(&seed, reservation_seq) {
                crate::activity::publish_activity(self.hub.get().map(|v| &**v), &rollback);
            }
            return Err(error);
        }
        Ok(Value::Null)
    }

    fn list_sessions(&self) -> Vec<Value> {
        let manager = deepx_session::SessionManager::global();
        let registry = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        let workspaces = deepx_session::WorkspaceStore::global();
        manager
            .list()
            .into_iter()
            .map(|meta| {
                let mut value = serde_json::to_value(&meta).unwrap_or_default();
                value["running"] = json!(registry.is_running(&meta.seed));
                value["workspace_id"] = workspaces
                    .workspace_of(&meta.seed)
                    .map(Value::String)
                    .unwrap_or(Value::Null);
                value
            })
            .collect()
    }

    fn save_config(&self, params: &Value) -> Result<(), String> {
        // Never log config.save payloads: they may contain provider credentials.
        log::info!("[config.save] saving configuration");
        let mut cfg = deepx_config::Config::load().unwrap_or_default();
        // 审计 P0-1：apiKey 空串 = 显式删除凭据（update_string 忽略空串，
        // 这里单独处理；"****" 是 load_config 的掩码占位符，跳过保持现值）。
        if let Some(value) = value2(params, "api_key", "apiKey").and_then(Value::as_str) {
            if value != "****" {
                cfg.api_key = value.to_string();
            }
        }
        update_string(&mut cfg.model, params, "model", "model");
        update_string(&mut cfg.base_url, params, "base_url", "baseUrl");
        update_string(&mut cfg.provider_id, params, "provider_id", "providerId");
        update_string(&mut cfg.endpoint, params, "endpoint", "endpoint");
        update_string(
            &mut cfg.reasoning_effort,
            params,
            "reasoning_effort",
            "reasoningEffort",
        );
        update_u32(&mut cfg.max_tokens, params, "max_tokens", "maxTokens");
        update_u32(
            &mut cfg.context_limit,
            params,
            "context_limit",
            "contextLimit",
        );
        if let Some(lang) = value2(params, "lang", "lang")
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
        {
            cfg.lang = Some(lang.to_string());
        }
        // ── UI 字体（空字符串 = 恢复系统默认；掩码守卫同 update_string）──
        if let Some(value) = value2(params, "font_family", "fontFamily").and_then(Value::as_str) {
            if value != "****" {
                cfg.font_family = value.to_string();
            }
        }
        update_string(
            &mut cfg.subagent.model,
            params,
            "subagent_model",
            "subagentModel",
        );
        update_string(
            &mut cfg.subagent.base_url,
            params,
            "subagent_base_url",
            "subagentBaseUrl",
        );
        update_string(
            &mut cfg.subagent.api_key,
            params,
            "subagent_api_key",
            "subagentApiKey",
        );
        update_u32(
            &mut cfg.subagent.max_tokens,
            params,
            "subagent_max_tokens",
            "subagentMaxTokens",
        );
        if let Some(value) = value2(params, "subagent_timeout_secs", "subagentTimeoutSecs")
            .and_then(Value::as_u64)
            .filter(|v| *v > 0)
        {
            cfg.subagent.timeout_secs = value;
        }
        // 允许空数组保存：配置语义中 `default_tools = []` 表示"全部工具可用"，
        // 用户在前端取消所有勾选时应能表达该状态（此前空数组被过滤、无法保存）。
        if let Some(values) = value2(params, "subagent_default_tools", "subagentDefaultTools")
            .and_then(Value::as_array)
        {
            cfg.subagent.default_tools = values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect();
        }
        if let Some(path) =
            value2(params, "tokenizer_path", "tokenizerPath").and_then(Value::as_str)
        {
            cfg.tokenizer_path = (!path.is_empty()).then(|| path.to_string());
        }
        // ── Multimodal (vision) config ──
        if let Some(enabled) =
            value2(params, "multimodal_enabled", "multimodalEnabled").and_then(Value::as_bool)
        {
            cfg.multimodal.enabled = enabled;
        }
        update_string(
            &mut cfg.multimodal.provider_type,
            params,
            "multimodal_provider_type",
            "multimodalProviderType",
        );
        update_string(
            &mut cfg.multimodal.provider_id,
            params,
            "multimodal_provider_id",
            "multimodalProviderId",
        );
        update_string(
            &mut cfg.multimodal.api_key,
            params,
            "multimodal_api_key",
            "multimodalApiKey",
        );
        update_string(
            &mut cfg.multimodal.base_url,
            params,
            "multimodal_base_url",
            "multimodalBaseUrl",
        );
        update_string(
            &mut cfg.multimodal.model,
            params,
            "multimodal_model",
            "multimodalModel",
        );
        update_u32(
            &mut cfg.multimodal.max_tokens,
            params,
            "multimodal_max_tokens",
            "multimodalMaxTokens",
        );
        if let Some(threshold) =
            value2(params, "auto_compact_threshold", "autoCompactThreshold").and_then(Value::as_f64)
        {
            cfg.auto_compact_threshold = threshold;
        }
        if let Some(enabled) =
            value2(params, "compliance_enabled", "complianceEnabled").and_then(Value::as_bool)
        {
            cfg.compliance_enabled = enabled;
        }
        cfg.save()?;
        self.registry()?
            .send_ringing_all(RingingCommand::Control(ControlCommand::AgentReloadConfig));
        Ok(())
    }
}

fn value2<'a>(params: &'a Value, snake: &str, camel: &str) -> Option<&'a Value> {
    params.get(snake).or_else(|| params.get(camel))
}
fn pstr(params: &Value, key: &str) -> Result<String, String> {
    params
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("missing string parameter: {key}"))
}
fn pstr2(params: &Value, snake: &str, camel: &str) -> Result<String, String> {
    value2(params, snake, camel)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("missing string parameter: {snake}"))
}
fn pbool(params: &Value, key: &str) -> bool {
    params.get(key).and_then(Value::as_bool).unwrap_or(false)
}
fn pbool2(params: &Value, snake: &str, camel: &str) -> bool {
    value2(params, snake, camel)
        .and_then(Value::as_bool)
        .unwrap_or(false)
}
fn pu64(params: &Value, key: &str) -> u64 {
    params.get(key).and_then(Value::as_u64).unwrap_or_default()
}
fn pstrings(params: &Value, key: &str) -> Vec<String> {
    params
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}
fn err(error: impl std::fmt::Display) -> String {
    error.to_string()
}
fn parse_json_string(value: String) -> Result<Value, String> {
    serde_json::from_str(&value).map_err(err)
}

fn update_string(target: &mut String, params: &Value, snake: &str, camel: &str) {
    if let Some(value) = value2(params, snake, camel)
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
    {
        // Guard: skip the masked placeholder used by load_config
        if value == "****" {
            log::info!("[update_string] skipping masked placeholder '****' for field '{snake}'");
            return;
        }
        *target = value.to_string();
    }
}
fn update_u32(target: &mut u32, params: &Value, snake: &str, camel: &str) {
    if let Some(value) = value2(params, snake, camel)
        .and_then(Value::as_u64)
        .filter(|v| *v > 0)
    {
        *target = value as u32;
    }
}

fn workspace(seed: &str) -> String {
    if seed.is_empty() {
        return String::new();
    }
    // 统一数据源：meta.cwd（workspace.txt 退役，读取侧惰性迁移）。
    deepx_session::workspace::session_workspace_cwd(seed).unwrap_or_default()
}

fn git<F>(seed: &str, operation: F, empty: Value) -> Result<Value, String>
where
    F: FnOnce(&str) -> Result<String, String>,
{
    let workspace = workspace(seed);
    if workspace.is_empty() {
        return Ok(empty);
    }
    let value = operation(&workspace)?;
    serde_json::from_str(&value).or_else(|_| Ok(json!(value)))
}

fn with_file_previews(text: String, files: &[String]) -> String {
    if files.is_empty() {
        return text;
    }
    let mut parts = vec!["[Files]".to_string()];
    for path in files {
        let preview = std::fs::read_to_string(path)
            .map(|value| {
                value
                    .lines()
                    .take(10)
                    .collect::<Vec<_>>()
                    .join("\n")
                    .chars()
                    .take(1000)
                    .collect()
            })
            .unwrap_or_else(|e| format!("[ERROR: {e}]"));
        parts.push(format!("\n{path}:\n{preview}"));
    }
    parts.push(format!("\n\n[Message]\n{text}"));
    parts.join("")
}

fn dashboard(seed: &str) -> Result<Value, String> {
    let dir = deepx_types::platform::sessions_dir().join(seed);
    let tasks: Vec<Value> = deepx_workspace::todo::todo_status_json(seed)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| {
            v.get("items")?.as_array().map(|arr| {
                arr.iter()
                    .map(|item| {
                        json!({
                            "id": item["id"],
                            "subject": item["title"],
                            "description": item["description"],
                            "status": item["status"],
                            "evidence": item["evidence"],
                        })
                    })
                    .collect()
            })
        })
        .unwrap_or_default();
    let mut edits = std::fs::File::open(dir.join("code_stats.jsonl"))
        .ok()
        .into_iter()
        .flat_map(|file| std::io::BufReader::new(file).lines().map_while(Result::ok))
        .filter_map(|line| {
            serde_json::from_str::<Value>(&line)
                .ok()?
                .get("file")?
                .as_str()
                .map(str::to_string)
        })
        .collect::<Vec<_>>();
    edits.reverse();
    edits.dedup();
    edits.truncate(10);
    Ok(json!({"tasks":tasks,"recent_edits":edits}))
}

fn activity(seed: &str) -> Result<Value, String> {
    let (_, messages) = deepx_session::SessionManager::global()
        .load(seed)
        .ok_or_else(|| "session not found".to_string())?;
    let mut tools = std::collections::HashMap::new();
    for message in &messages {
        if message.role == "assistant" {
            for block in &message.content {
                if let deepx_types::ContentBlock::ToolUse { id, name, input } = block {
                    tools.insert(id.clone(), (name.clone(), input.to_string()));
                }
            }
        }
    }
    let mut result = Vec::new();
    for message in &messages {
        if message.role == "tool" {
            for block in &message.content {
                if let deepx_types::ContentBlock::ToolResult {
                    tool_use_id,
                    result: tool_result,
                } = block
                {
                    let (name, args) = tools.get(tool_use_id).cloned().unwrap_or_default();
                    result.push(json!({"tool_name":name,"summary":tool_result.summary,"status":serde_json::to_value(tool_result.status).unwrap_or_default(),"time":message.msg_id.map(|v|v.to_string()).unwrap_or_default(),"args":args}));
                }
            }
        }
    }
    result.reverse();
    Ok(Value::Array(result))
}

fn load_config() -> Result<Value, String> {
    let cfg = deepx_config::Config::load().map_err(err)?;
    let providers = deepx_config::registry::all_providers().into_iter().map(|provider| json!({"id":provider.id,"display":provider.display,"endpoints":provider.endpoints.into_iter().map(|endpoint|json!({"id":endpoint.id,"display":endpoint.display,"protocol":endpoint.protocol,"base_url":endpoint.base_url,"default_model":endpoint.default_model,"models":endpoint.models,"stateful":endpoint.stateful,"beta":endpoint.beta})).collect::<Vec<_>>() })).collect::<Vec<_>>();
    // profile 名称列表（前端 profile 管理 UI 用；不含敏感字段）。
    let profile_names: Vec<String> = cfg.profiles.keys().cloned().collect();
    Ok(
        json!({"api_key":if cfg.api_key.is_empty(){""}else{"****"},"api_key_set":!cfg.api_key.is_empty(),"model":cfg.model,"base_url":cfg.base_url,"provider_id":cfg.provider_id,"endpoint":cfg.endpoint,"max_tokens":cfg.max_tokens,"context_limit":cfg.context_limit,"reasoning_effort":cfg.reasoning_effort,"auto_compact_threshold":cfg.auto_compact_threshold,"permission_level":cfg.permission_level,"lang":cfg.lang,"font_family":cfg.font_family,"active_profile":cfg.active_profile,"profiles":profile_names,"compliance_enabled":cfg.compliance_enabled,"providers":providers,"subagent":{"model":cfg.subagent.model,"base_url":cfg.subagent.base_url,"api_key":if cfg.subagent.api_key.is_empty(){""}else{"****"},"api_key_set":!cfg.subagent.api_key.is_empty(),"max_tokens":cfg.subagent.max_tokens,"timeout_secs":cfg.subagent.timeout_secs,"default_tools":cfg.subagent.default_tools},"multimodal":{"enabled":cfg.multimodal.enabled,"provider_type":cfg.multimodal.provider_type,"provider_id":cfg.multimodal.provider_id,"api_key":if cfg.multimodal.api_key.is_empty(){""}else{"****"},"api_key_set":!cfg.multimodal.api_key.is_empty(),"base_url":cfg.multimodal.base_url,"model":cfg.multimodal.model,"max_tokens":cfg.multimodal.max_tokens},"workspace":{"mode":cfg.workspace.mode},"tokenizer_path":cfg.tokenizer_path}),
    )
}

fn context_stats(seed: &str) -> Result<Value, String> {
    // 统一数据源：meta.json 的 context_stats 字段（原独立文件退役）。
    // 旧 context_stats.json 为可再生缓存，忽略不迁移。
    if let Some(meta) = deepx_session::SessionManager::global().load_meta(seed) {
        if let Some(stats) = meta.context_stats {
            return Ok(stats);
        }
    }
    Ok(
        json!({"messages":0,"chat_text":0,"thinking":0,"tool_calls":0,"tool_results":0,"tools_schema":0,"system_prompt":0,"thinking_blocks":0,"tool_call_blocks":0}),
    )
}

fn token_stats(days: u32) -> Result<Value, String> {
    use std::collections::BTreeMap;
    let days = days.max(1);
    let cutoff = days_before_today(days);
    let mut daily: BTreeMap<String, Value> = BTreeMap::new();
    if let Ok(file) =
        std::fs::File::open(deepx_types::platform::data_dir().join("token_stats.jsonl"))
    {
        for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
            let Ok(entry) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let date = entry["date"].as_str().unwrap_or_default().to_string();
            if date < cutoff {
                continue;
            }
            let day=daily.entry(date).or_insert_with(||json!({"prompt_tokens":0,"completion_tokens":0,"cache_hit":0,"cache_miss":0,"calls":0}));
            for key in [
                "prompt_tokens",
                "completion_tokens",
                "cache_hit",
                "cache_miss",
            ] {
                day[key] = json!(day[key].as_u64().unwrap_or(0) + entry[key].as_u64().unwrap_or(0));
            }
            day["calls"] = json!(day["calls"].as_u64().unwrap_or(0) + 1);
        }
    }
    let mut values = Vec::new();
    let mut prompt = 0;
    let mut completion = 0;
    let mut hit = 0;
    let mut miss = 0;
    let mut calls = 0;
    for offset in (0..days).rev() {
        let date = days_before_today(offset);
        let entry=daily.get(&date).cloned().unwrap_or_else(||json!({"prompt_tokens":0,"completion_tokens":0,"cache_hit":0,"cache_miss":0,"calls":0}));
        prompt += entry["prompt_tokens"].as_u64().unwrap_or(0);
        completion += entry["completion_tokens"].as_u64().unwrap_or(0);
        hit += entry["cache_hit"].as_u64().unwrap_or(0);
        miss += entry["cache_miss"].as_u64().unwrap_or(0);
        calls += entry["calls"].as_u64().unwrap_or(0);
        values.push(json!({"date":date,"prompt_tokens":entry["prompt_tokens"],"completion_tokens":entry["completion_tokens"],"cache_hit":entry["cache_hit"],"cache_miss":entry["cache_miss"],"calls":entry["calls"]}));
    }
    let pct = if hit + miss > 0 {
        (hit as f64 / (hit + miss) as f64 * 1000.0).round() / 10.0
    } else {
        0.0
    };
    Ok(
        json!({"daily":values,"totals":{"prompt_tokens":prompt,"completion_tokens":completion,"calls":calls,"cache_hit_pct":pct}}),
    )
}
fn days_before_today(days: u32) -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .saturating_sub(days as u64 * 86400);
    let (y, m, d) = deepx_types::platform::civil_from_days((seconds / 86400) as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

fn deepx_dir(seed: &str) -> std::path::PathBuf {
    let workspace = workspace(seed);
    if workspace.is_empty() || workspace == "." {
        deepx_types::platform::data_dir().join("workspace")
    } else {
        std::path::Path::new(&workspace).join(".deepx")
    }
}
fn read_plan(seed: &str) -> Result<Value, String> {
    let content = match std::fs::read_to_string(deepx_dir(seed).join("PLAN.md")) {
        Ok(value) => value,
        Err(_) => return Ok(json!([])),
    };
    let items = content
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if !line.starts_with("- [") {
                return None;
            }
            let end = line.find(']')?;
            let status = line.get(3..end)?.trim();
            let rest = line.get(end + 1..)?.trim();
            let (id, title) = rest.split_once(": ")?;
            Some(json!({"id":id,"title":title,"status":status,"comment":"","actions":[]}))
        })
        .collect();
    Ok(Value::Array(items))
}
fn plan_action(seed: &str, item_id: &str, action: &str, comment: &str) -> Result<(), String> {
    let path = deepx_dir(seed).join("PLAN.md");
    let content = std::fs::read_to_string(&path).map_err(err)?;
    let mut found = false;
    let output = content
        .lines()
        .filter_map(|line| {
            if !found && line.trim().starts_with("- [") && line.contains(&format!(" {item_id}: ")) {
                found = true;
                if action == "delete" {
                    return None;
                }
                let end = line.find(']')?;
                let base = format!("- [ ]{}", &line[end + 1..]);
                return Some(match action {
                    "approve" => base.replacen("- [ ]", "- [✓]", 1),
                    "reject" => {
                        let value = base.replacen("- [ ]", "- [-]", 1);
                        if comment.is_empty() {
                            value
                        } else {
                            format!("{value} | {comment}")
                        }
                    }
                    "ask" => base.replacen("- [ ]", "- [?]", 1),
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
    std::fs::write(path, output).map_err(err)
}

#[allow(dead_code)]
#[cfg(test)]
mod control_scope_tests {
    use super::DeepxService;

    #[test]
    fn global_activity_snapshot_does_not_require_a_session_lease() {
        assert!(!DeepxService::session_scoped("session.activity"));
        assert!(!DeepxService::session_scoped("session.list"));
        assert!(DeepxService::session_scoped("session.get_activity"));
        assert!(DeepxService::session_scoped("session.send_message"));
    }
}

fn command_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("svc-{nanos:x}")
}

fn parse_conversation_mode(mode: &str) -> Result<ConversationMode, String> {
    match mode {
        "normal" | "" => Ok(ConversationMode::Normal),
        "plan" => Ok(ConversationMode::Plan),
        "code" => Ok(ConversationMode::Code),
        other => Err(format!(
            "invalid conversation mode '{other}' (supported: normal | plan | code)"
        )),
    }
}
