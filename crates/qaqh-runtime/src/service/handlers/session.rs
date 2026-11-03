//! `session.*` / `skills.*` / `subagent.*` 服务面。

use serde_json::{Value, json};

use qaqh_domain::ControlCommand;
use qaqh_ringing::RingingCommand;

use crate::error::{QaqhError, QaqhResult};
use crate::service::QaqhService;
use qaqh_session::session_fact_v2::{SessionMetadataPatch, TitleSource};

use crate::service::command::SessionCommand;
use crate::service::params::optional_tool_mode;
use crate::service::stats::{activity, dashboard};
use crate::service::session_canonical_cwd;

pub(crate) fn handle(service: &QaqhService, command: SessionCommand) -> QaqhResult<Value> {
    match command {
        SessionCommand::SessionList(_) => {
            QaqhError::to_value("session list", service.list_sessions())
        }
        SessionCommand::SessionMeta(args) => {
            let session_id = args.session_id;
            let Some(meta) = service.session_meta(&session_id) else {
                return Ok(Value::Null);
            };
            // 单条与 `session.list` 的条目**同一个形状**（G2）：同样的
            // `SessionMeta` + 运行期字段。此前这里也是手拼 `value["running"]`，
            // 且不带 `workspace_id`——同一个形状两处各拼一次，正是漂移的温床。
            let registry = service.registry().map_err(QaqhError::message)?;
            let loaded = registry.is_running(&meta.session_id);
            let fallback = registry
                .activities()
                .into_iter()
                .find(|activity| activity.session_id == meta.session_id)
                .map(|activity| activity.state);
            let entry = qaqh_types::SessionListEntry {
                status: service.session_run_status(loaded, fallback, &meta.session_id),
                workspace_id: qaqh_session::WorkspaceStore::global().workspace_of(&meta.session_id),
                meta,
            };
            QaqhError::to_value("session meta", entry)
        }
        SessionCommand::SessionActivity(_) => {
            let activities = service.registry().map_err(QaqhError::message)?.activities();
            QaqhError::to_value("session activity", activities)
        }
        SessionCommand::SessionGetActivity(args) => {
            activity(&service.owners, &args.session_id).map_err(QaqhError::message)
        }
        SessionCommand::SessionDashboard(args) => {
            dashboard(&args.session_id).map_err(QaqhError::message)
        }
        SessionCommand::SessionNew(args) => create_new(service, args),
        SessionCommand::SessionCreate(args) => {
            let session_id = service
                .sessions
                .allocate_session()
                .map_err(|error| {
                    QaqhError::context("session.create: allocate session failed", error)
                })?;
            service.sessions.clear_active();
            create_business_row(service, &session_id, CreateRowInput {
                cwd: args.cwd.as_deref(),
                title: args.title.as_deref().map(str::trim).filter(|t| !t.is_empty()),
                profile: args.profile.as_deref(),
                effort: args.effort.as_deref(),
                tool_mode: None,
                custom_tools: None,
                model_selection: None,
            })
            .inspect_err(|_| {
                let _ = service.sessions.delete(&session_id);
            })
            .map_err(QaqhError::message)?;
            service
                .registry()
                .map_err(QaqhError::message)?
                .spawn_new(&session_id)
                .map_err(QaqhError::message)?;
            Ok(json!({ "session_id": session_id }))
        }
        SessionCommand::SessionResume(args) => {
            service.sessions.set_active_session(&args.session_id);
            service
                .registry()
                .map_err(QaqhError::message)?
                .get_or_spawn(&args.session_id)
                .map_err(QaqhError::message)?;
            Ok(Value::Null)
        }
        SessionCommand::SessionSetToolMode(args) => {
            let tool_mode = args.tool_mode;
            crate::service::params::validate_tool_mode(&tool_mode).map_err(QaqhError::message)?;
            let custom_tools = args.custom_tools.unwrap_or_default();
            if tool_mode == "custom" && custom_tools.is_empty() {
                return Err(QaqhError::invalid(
                    "custom tool mode requires at least one tool in custom_tools",
                ));
            }
            // 先持久化（meta.json，重启存活），再通知 worker 应用
            // （set_allowed_tools + tool_defs 刷新 = 模型侧源头过滤）。
            // CK-PERSIST：持久化失败 → 400 返回前端，前端回滚乐观值；
            // 不允许「应用成功但没落盘」的假切换（重启即丢）。
            service
                .write_meta(
                    &args.session_id,
                    SessionMetadataPatch {
                        tool_mode: Some(tool_mode.clone()),
                        custom_tools: Some(custom_tools.clone()),
                        ..Default::default()
                    },
                )
                .map_err(|error| QaqhError::context("persist tool_mode failed", error))?;
            service.send_ringing_cmd(
                args.session_id,
                RingingCommand::Control(ControlCommand::SetToolMode {
                    tool_mode,
                    custom_tools,
                }),
            )
        }
        SessionCommand::SkillsOperation(args) => service.send_ringing_cmd(
            args.session_id,
            RingingCommand::Control(ControlCommand::SkillsOperation {
                operation_id: args.operation_id,
                action: args.action,
                name: args.name,
            }),
        ),
        SessionCommand::SkillsReload(args) => service.send_ringing_cmd(
            args.session_id,
            RingingCommand::Control(ControlCommand::SkillsReload),
        ),
        SessionCommand::SkillsActivate(args) => service.send_ringing_cmd(
            args.session_id,
            RingingCommand::Control(ControlCommand::SkillsActivate { name: args.name }),
        ),
        SessionCommand::SkillsListTools(_) => {
            Ok(json!(qaqh_workspace::runtime::process_all_tool_names()))
        }
        SessionCommand::SkillsCatalog(args) => {
            let workspace = match args.cwd {
                Some(cwd) if !cwd.is_empty() => std::path::PathBuf::from(cwd),
                _ => std::env::current_dir().map_err(|error| {
                    QaqhError::message(format!("skills.catalog: resolve cwd: {error}"))
                })?,
            };
            let catalog = qaqh_skills::discover(&workspace);
            let skills = catalog
                .skills
                .iter()
                .map(|skill| crate::ringing::service_methods::SkillCatalogEntry {
                    name: skill.name.clone(),
                    description: skill.description.clone(),
                    scope: match skill.scope {
                        qaqh_skills::SkillScope::Project => "project",
                        qaqh_skills::SkillScope::User => "user",
                    }
                    .to_string(),
                    origin: match skill.origin {
                        qaqh_skills::SkillOrigin::Project => "project",
                        qaqh_skills::SkillOrigin::User => "user",
                        qaqh_skills::SkillOrigin::Bundled => "bundled",
                    }
                    .to_string(),
                    path: skill.path.to_string_lossy().into_owned(),
                })
                .collect();
            let diagnostics = catalog
                .diagnostics
                .iter()
                .map(
                    |diagnostic| crate::ringing::service_methods::SkillCatalogDiagnostic {
                        severity: match diagnostic.severity {
                            qaqh_skills::DiagnosticSeverity::Warning => "warning",
                            qaqh_skills::DiagnosticSeverity::Error => "error",
                        }
                        .to_string(),
                        path: diagnostic.path.to_string_lossy().into_owned(),
                        message: diagnostic.message.clone(),
                    },
                )
                .collect();
            serde_json::to_value(crate::ringing::service_methods::SkillsCatalogResult {
                skills,
                diagnostics,
            })
            .map_err(QaqhError::message)
        }
        // 只改本会话的 profile（不动全局 active_profile，也不广播）。落
        // meta.json 后定向重载该会话——`AgentReloadConfig` 走
        // `send_ringing_cmd` 而不是 registry 广播，所以别的会话不受影响。
        // `name` 传空串 = 清除该会话的选择，回到跟随全局 `active_profile`。
        SessionCommand::SessionSetProfile(args) => {
            let name = args.name;
            if name.is_empty() {
                service
                    .write_meta(
                        &args.session_id,
                        SessionMetadataPatch {
                            profile: Some(String::new()),
                            ..Default::default()
                        },
                    )
                    .map_err(QaqhError::message)?;
            } else {
                let known = qaqh_config::watch::authoritative()
                    .is_some_and(|cfg| cfg.profiles.contains_key(&name));
                if !known {
                    return Err(QaqhError::not_found(format!("profile '{name}' not found")));
                }
                service
                    .write_meta(
                        &args.session_id,
                        SessionMetadataPatch {
                            profile: Some(name.clone()),
                            ..Default::default()
                        },
                    )
                    .map_err(QaqhError::message)?;
            }
            service.send_ringing_cmd(
                args.session_id,
                RingingCommand::Control(ControlCommand::AgentReloadConfig),
            )?;
            Ok(Value::Null)
        }
        // 只改本会话的 effort（思考强度），不碰全局配置。
        // 落 meta.json 后定向重载该会话（AgentReloadConfig）。
        SessionCommand::SessionSetEffort(args) => {
            let effort = args.effort;
            service
                .write_meta(
                    &args.session_id,
                    SessionMetadataPatch {
                        effort: Some(if effort.is_empty() {
                            String::new()
                        } else {
                            effort.clone()
                        }),
                        ..Default::default()
                    },
                )
                .map_err(QaqhError::message)?;
            service.send_ringing_cmd(
                args.session_id,
                RingingCommand::Control(ControlCommand::AgentReloadConfig),
            )?;
            Ok(Value::Null)
        }
        // 用户重命名会话标题（方案B）：落 meta.title 并以 TitleSource::User
        // 广播 TitleChanged —— 客户端经 v2 Meta SSE 即时回灌，session.list
        // 每轮读盘自然可见。auto-title 的冻结守卫（title.is_some()）保证
        // 首回合后不再重复生成。
        SessionCommand::SessionRename(args) => {
            let title = args.title.trim().to_string();
            if title.is_empty() {
                return Err(QaqhError::invalid("title must be a non-empty string"));
            }
            let session_id = args.session_id;
            service
                .write_title(&session_id, &title, TitleSource::User)
                .map_err(QaqhError::message)?;
            // 标题即业务库列;客户端经 GET state / session.list 取新值。
            Ok(Value::Null)
        }
        // ── Subagent orchestration ─────────────────────────────────────
        // Spawn an isolated subagent worker and return its seed. The caller
        // (parent agent) then attaches the seed and drives it with ordinary
        // Ringing commands/events.
        SessionCommand::SubagentSpawn(args) => spawn_subagent(service, args),
    }
}

/// `session.new`：分配 canonical identity → 落 worker 前置状态 → 物化首条事实 → spawn。
pub(crate) fn create_new(
    service: &QaqhService,
    args: crate::service::command::SessionNewArgs,
) -> QaqhResult<Value> {
    // 可选工具模式预置（TUI/CLI 壳在 create 时一次性锁定）。
    // 先于任何落盘校验：非法值必须整体拒绝，不得留下孤儿 meta。
    let custom_tools = args.custom_tools.unwrap_or_default();
    let preset =
        optional_tool_mode(args.tool_mode.as_deref(), &custom_tools).map_err(QaqhError::message)?;
    // BUG-2026-09-13-24 + BETA-01：先分配会话 id，再以它建业务行。
    // 业务行一次性落齐 cwd/model/tool_mode/model_selection/title——spawn 与
    // 首回合直接读行，不再有 meta.json 中间态。
    let session_id = service
        .sessions
        .allocate_session()
        .map_err(|error| QaqhError::context("session.new: allocate session failed", error))?;
    service.sessions.clear_active();
    let (tool_mode, custom_tools) = preset
        .map(|(tool_mode, custom_tools)| (Some(tool_mode), Some(custom_tools)))
        .unwrap_or((None, None));
    create_business_row(service, &session_id, CreateRowInput {
        cwd: args.cwd.as_deref(),
        title: args.title.as_deref().map(str::trim).filter(|t| !t.is_empty()),
        profile: None,
        effort: None,
        tool_mode,
        custom_tools,
        model_selection: args.model_selection.clone(),
    })
    .inspect_err(|_| {
        let _ = service.sessions.delete(&session_id);
    })
    .map_err(QaqhError::message)?;
    service
        .registry()
        .map_err(QaqhError::message)?
        .spawn_new(&session_id)
        .map_err(QaqhError::message)?;
    Ok(json!(session_id))
}

impl QaqhService {
    /// 统一命令面（§8.7 typed adapter）：`session.create`。
    ///
    /// 强类型入参，直连 [`create_new`] 业务实现，不走
    /// `handle(method, Value)` 字符串 decoder。
    pub fn create_session_unified(
        &self,
        cwd: Option<String>,
        title: Option<String>,
        model_selection: Option<qaqh_types::ModelSelection>,
        tool_mode: Option<String>,
        custom_tools: Option<Vec<String>>,
    ) -> QaqhResult<Value> {
        create_new(
            self,
            crate::service::command::SessionNewArgs {
                cwd,
                tool_mode,
                custom_tools,
                title,
                model_selection,
            },
        )
    }
}

/// 业务行创建输入:cwd/model 由 `session_canonical_cwd` 解析,其余字段直通。
struct CreateRowInput<'a> {
    cwd: Option<&'a str>,
    title: Option<&'a str>,
    profile: Option<&'a str>,
    effort: Option<&'a str>,
    tool_mode: Option<String>,
    custom_tools: Option<Vec<String>>,
    model_selection: Option<qaqh_types::ModelSelection>,
}

/// 会话创建的第一站:一次性落 SQLite 业务行(cwd/model/profile/effort/
/// tool_mode/model_selection/title)。行存在即 spawn 可读,无 meta.json 中间态。
fn create_business_row(
    service: &QaqhService,
    session_id: &str,
    input: CreateRowInput<'_>,
) -> Result<(), String> {
    let (canonical_cwd, model) = session_canonical_cwd(&service.owners, session_id, input.cwd)?;
    let row = qaqh_session::db::store::NewSession {
        session_id: session_id.to_string(),
        parent_session_id: None,
        title: input.title.map(str::to_string).unwrap_or_default(),
        title_source: input.title.map(|_| "user".to_string()),
        cwd: Some(canonical_cwd),
        model: Some(model),
        effort: input.effort.filter(|e| !e.is_empty()).map(str::to_string),
        profile: input.profile.filter(|p| !p.is_empty()).map(str::to_string),
        mode: None,
        tool_mode: input.tool_mode,
        model_selection: input.model_selection,
        custom_tools: input.custom_tools,
        skills: None,
        frozen_annotation: None,
        ephemeral: false,
    };
    let error_session_id = session_id.to_string();
    service
        .owners
        .db()
        .write_blocking(move |conn| qaqh_session::db::store::create_session(conn, &row))
        .map(|_| ())
        .map_err(|error| format!("create business row for {error_session_id}: {error}"))
}

fn spawn_subagent(
    service: &QaqhService,
    args: crate::service::command::SubagentSpawnArgs,
) -> QaqhResult<Value> {
    let tools = args.tools.unwrap_or_default();
    let model = args.model.filter(|model| !model.is_empty());
    let base_url = args.base_url.filter(|base_url| !base_url.is_empty());
    let max_tokens = args.max_tokens;
    // 子代理继承主代理的 workspace：spawn 前把 workspace 作为 cwd 落进子会话
    // meta（`allocate_agent_session(workspace)` → `create_new_session(..., cwd, ...)`），
    // 子 worker 启动时经 `load_session_workspace` 读 meta.cwd，从而正确解析
    // 相对路径并以主代理工作区为权限边界。
    let workspace = args
        .workspace
        .map(|workspace| workspace.trim().to_string())
        .filter(|workspace| !workspace.is_empty() && workspace != ".");
    // BETA-01：子代理与主会话共享 id 规则；cwd 由 spawn_subagent_in_workspace
    // 落进业务行（显式 workspace 优先，其次继承父会话 cwd）。
    let session_id = service
        .sessions
        .allocate_session()
        .map_err(|error| QaqhError::context("subagent.spawn: allocate child session", error))?;
    if let Some(workspace) = &workspace {
        log::info!("[subagent] inherited workspace for seed={session_id}: {workspace}");
    }
    service
        .registry()
        .map_err(QaqhError::message)?
        .spawn_subagent_in_workspace(
            &session_id,
            &tools,
            model.as_deref(),
            base_url.as_deref(),
            max_tokens,
            workspace.as_deref(),
        )
        .map_err(QaqhError::message)?;
    log::info!(
        "[subagent] spawned subagent worker seed={session_id} tools={}",
        tools.len()
    );
    Ok(json!({ "session_id": session_id }))
}
