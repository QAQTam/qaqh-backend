//! `session.*` / `skills.*` / `subagent.*` 服务面。

use serde_json::{Value, json};

use qaqh_ringing::{RingingCommand, SessionCreateResult};

use crate::error::{QaqhError, QaqhResult};
use crate::service::QaqhService;

use crate::service::command::SessionCommand;
use crate::service::params::optional_tool_mode;
use crate::service::session_canonical_cwd;
use crate::service::stats::{activity, dashboard};

pub(crate) fn handle(service: &QaqhService, command: SessionCommand) -> QaqhResult<Value> {
    match command {
        SessionCommand::SessionList(_) => {
            QaqhError::to_value("session list", service.list_sessions()?)
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
                workspace_id: service
                    .owners
                    .db()
                    .read_blocking(|conn| {
                        qaqh_session::db::workspace::workspace_of(conn, &meta.session_id)
                    })
                    .map_err(QaqhError::message)?,
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
            // B08 权威 DTO:typed 结构直出,序列化即 wire 形状。
            dashboard(&args.session_id)
                .and_then(|dashboard| {
                    serde_json::to_value(dashboard).map_err(|error| error.to_string())
                })
                .map_err(QaqhError::message)
        }
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
        } // ── Subagent orchestration ─────────────────────────────────────
          // Spawn an isolated subagent worker and return its seed. The caller
          // (parent agent) then attaches the seed and drives it with ordinary
          // Ringing commands/events.
    }
}

struct CreateSessionInput {
    cwd: Option<String>,
    title: Option<String>,
    profile: Option<String>,
    effort: Option<String>,
    tool_mode: Option<String>,
    custom_tools: Option<Vec<String>>,
    model_selection: Option<qaqh_types::ModelSelection>,
}

fn create_session(
    service: &QaqhService,
    input: CreateSessionInput,
) -> QaqhResult<SessionCreateResult> {
    let custom_tools = input.custom_tools.unwrap_or_default();
    let preset = optional_tool_mode(input.tool_mode.as_deref(), &custom_tools)
        .map_err(QaqhError::message)?;
    let session_id = service
        .sessions
        .allocate_session()
        .map_err(|error| QaqhError::context("session.create: allocate session failed", error))?;
    service.sessions.clear_active();
    let (tool_mode, custom_tools) = preset
        .map(|(tool_mode, custom_tools)| (Some(tool_mode), Some(custom_tools)))
        .unwrap_or((None, None));
    create_business_row(
        service,
        &session_id,
        CreateRowInput {
            cwd: input.cwd.as_deref(),
            title: input
                .title
                .as_deref()
                .map(str::trim)
                .filter(|t| !t.is_empty()),
            profile: input.profile.as_deref(),
            effort: input.effort.as_deref(),
            tool_mode,
            custom_tools,
            model_selection: input.model_selection,
        },
    )
    .inspect_err(|_| {
        let _ = service.sessions.delete(&session_id);
    })
    .map_err(QaqhError::message)?;
    service
        .registry()
        .map_err(QaqhError::message)?
        .spawn_new(&session_id)
        .map_err(QaqhError::message)?;
    Ok(SessionCreateResult { session_id })
}

impl QaqhService {
    /// RingingV2 `session.create` 命令入口。
    ///
    /// 强类型入参，直连 [`create_session`] 业务实现。
    pub fn create_session_command(
        &self,
        cwd: Option<String>,
        title: Option<String>,
        profile: Option<String>,
        effort: Option<String>,
        model_selection: Option<qaqh_types::ModelSelection>,
        tool_mode: Option<String>,
        custom_tools: Option<Vec<String>>,
    ) -> QaqhResult<SessionCreateResult> {
        create_session(
            self,
            CreateSessionInput {
                cwd,
                title,
                profile,
                effort,
                tool_mode,
                custom_tools,
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

pub(crate) fn handle_ringing(service: &QaqhService, command: &RingingCommand) -> QaqhResult<Value> {
    let RingingCommand::SubagentSpawn {
        tools,
        model,
        base_url,
        max_tokens,
        workspace,
    } = command
    else {
        return Err(QaqhError::invalid(format!(
            "{} is not a subagent mutation",
            command.name()
        )));
    };
    spawn_subagent(
        service,
        tools,
        model.as_deref(),
        base_url.as_deref(),
        *max_tokens,
        workspace.as_deref(),
    )
}

fn spawn_subagent(
    service: &QaqhService,
    tools: &[String],
    model: Option<&str>,
    base_url: Option<&str>,
    max_tokens: Option<u32>,
    workspace: Option<&str>,
) -> QaqhResult<Value> {
    let model = model.filter(|model| !model.is_empty());
    let base_url = base_url.filter(|base_url| !base_url.is_empty());
    // 子代理继承主代理的 workspace：spawn 前把 workspace 作为 cwd 落进子会话
    // meta（`allocate_agent_session(workspace)` → `create_new_session(..., cwd, ...)`），
    // 子 worker 启动时经 `load_session_workspace` 读 meta.cwd，从而正确解析
    // 相对路径并以主代理工作区为权限边界。
    let workspace = workspace
        .map(str::trim)
        .filter(|workspace| !workspace.is_empty() && *workspace != ".");
    // BETA-01：子代理与主会话共享 id 规则；cwd 由 spawn_subagent_in_workspace
    // 落进业务行（显式 workspace 优先，其次继承父会话 cwd）。
    let session_id = service
        .sessions
        .allocate_session()
        .map_err(|error| QaqhError::context("subagent.spawn: allocate child session", error))?;
    if let Some(workspace) = workspace {
        log::info!("[subagent] inherited workspace for seed={session_id}: {workspace}");
    }
    service
        .registry()
        .map_err(QaqhError::message)?
        .spawn_subagent_in_workspace(&session_id, tools, model, base_url, max_tokens, workspace)
        .map_err(QaqhError::message)?;
    log::info!(
        "[subagent] spawned subagent worker seed={session_id} tools={}",
        tools.len()
    );
    Ok(json!({ "session_id": session_id }))
}
