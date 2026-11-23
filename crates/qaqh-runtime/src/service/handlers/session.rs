//! `session.*` / `skills.*` / `subagent.*` 服务面。

use serde_json::{Value, json};

use qaqh_ringing::SessionCreateResult;

use crate::error::{QaqhError, QaqhResult};
use crate::service::QaqhService;

use crate::service::command::SessionCommand;
use crate::service::params::optional_tool_mode;
use crate::service::session_canonical_cwd;
use crate::service::stats::{activity, dashboard};

pub(crate) fn handle(service: &QaqhService, command: SessionCommand) -> QaqhResult<Value> {
    match command {
        SessionCommand::SessionList(args) => QaqhError::to_value(
            "session list",
            service.list_sessions(args.include_archived)?,
        ),
        SessionCommand::SessionMeta(args) => {
            let session_id = args.session_id;
            let Some(meta) = service.session_meta(&session_id) else {
                return Ok(Value::Null);
            };
            // 单条与 `session.list` 的条目**同一个形状**（G2）：同样的
            // `SessionMeta` + 运行期字段。此前这里也是手拼 `value["running"]`，
            // 且不带 `workspace_id`——同一个形状两处各拼一次，正是漂移的温床。
            // P1-0b（L-01.2 同律）：`registry` 锁只圈住这一次内存查找，guard
            // 出表达式即释放；`session_run_status` 的 DB 兜底与下面
            // `workspace_of` 的 `read_blocking` 都在锁外。旧实现的 guard
            // 一路活到 `read_blocking` 之后，把单条查询也变成了持锁 IO。
            let (loaded, fallback) = {
                let registry = service.registry().map_err(QaqhError::message)?;
                let loaded = registry.is_running(&meta.session_id);
                let fallback = registry
                    .activity(&meta.session_id)
                    .map(|activity| activity.state);
                (loaded, fallback)
            };
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
            dashboard(&service.owners, &args.session_id)
                .and_then(|dashboard| {
                    serde_json::to_value(dashboard).map_err(|error| error.to_string())
                })
                .map_err(QaqhError::message)
        }
        SessionCommand::ExecSessionList(args) => {
            let session_id = args.session_id;
            let rows = service
                .owners
                .db()
                .read_blocking(move |conn| {
                    qaqh_session::db::store::exec_sessions_for_bootstrap(conn, &session_id)
                })
                .map_err(QaqhError::message)?;
            let sessions = rows
                .into_iter()
                .map(qaqh_session::db::view::exec_session_view)
                .collect::<Result<Vec<_>, _>>()
                .map_err(QaqhError::message)?;
            QaqhError::to_value(
                "exec session list",
                crate::ringing::service_methods::ExecSessionListResult { sessions },
            )
        }
        SessionCommand::ExecSessionOutput(args) => {
            // ringing 层只校验 params.session_id 的 lease 归属;exec 会话与该
            // 会话的绑定在这里判定——别人的会话拿不到别人的 exec 日志。
            let exec_session_id = args.exec_session_id.clone();
            let row = service
                .owners
                .db()
                .read_blocking(move |conn| {
                    qaqh_session::db::store::exec_session_by_id(conn, &exec_session_id)
                })
                .map_err(QaqhError::message)?
                .ok_or_else(|| {
                    QaqhError::message(format!("exec session not found: {}", args.exec_session_id))
                })?;
            if row.session_id != args.session_id {
                return Err(QaqhError::Forbidden {
                    message: format!(
                        "exec session {} does not belong to session {}",
                        args.exec_session_id, args.session_id
                    ),
                });
            }
            let max_bytes = args
                .max_bytes
                .unwrap_or(crate::ringing::service_methods::EXEC_OUTPUT_DEFAULT_MAX_BYTES)
                .min(crate::ringing::service_methods::EXEC_OUTPUT_MAX_BYTES);
            if max_bytes == 0 {
                return Err(QaqhError::message("max_bytes must be positive"));
            }
            let (next_offset, text) = qaqh_session::owner::read_exec_output_log_range(
                service.owners.db(),
                &args.session_id,
                &args.exec_session_id,
                row.next_offset,
                args.from_offset,
                max_bytes,
            )
            .map_err(QaqhError::message)?;
            QaqhError::to_value(
                "exec session output",
                crate::ringing::service_methods::ExecSessionOutputResult {
                    exec_session_id: args.exec_session_id,
                    next_offset,
                    output_complete: row.output_complete,
                    text,
                },
            )
        }
        SessionCommand::SessionTurn(args) => {
            let turn_id = args.turn_id;
            let missing_turn_id = turn_id.clone();
            let snapshot = service
                .owners
                .db()
                .read_blocking(move |conn| {
                    qaqh_session::db::view::load_session_turn(conn, &args.session_id, &turn_id)
                })
                .map_err(QaqhError::message)?;
            let Some(snapshot) = snapshot else {
                return Err(QaqhError::message(format!(
                    "session turn not found: {missing_turn_id}"
                )));
            };
            QaqhError::to_value("session turn", snapshot)
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
    tool_mode: Option<String>,
    custom_tools: Option<Vec<String>>,
    model_selection: Option<qaqh_types::ModelSelection>,
    /// ADR-0015 切片F：handoff fork 血缘(客户端 fork 时传 from_session_id)。
    handoff_from: Option<String>,
}

fn create_session(
    service: &QaqhService,
    input: CreateSessionInput,
) -> QaqhResult<SessionCreateResult> {
    let catalog =
        qaqh_profile::ProfilesStore::load_or_create(&qaqh_profile::default_providers_path())
            .map_err(QaqhError::message)?;
    let model_selection = input
        .model_selection
        .or_else(|| catalog.document().default_model_selection.clone())
        .ok_or_else(|| QaqhError::invalid("请先在电脑端设置默认服务商和模型"))?;
    qaqh_profile::resolve_model_selection(catalog.document(), &model_selection)
        .map_err(QaqhError::message)?;
    let custom_tools = input.custom_tools.unwrap_or_default();
    let preset = optional_tool_mode(input.tool_mode.as_deref(), &custom_tools)
        .map_err(QaqhError::message)?;
    let session_id = service.owners().allocate_session();
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
            tool_mode,
            custom_tools,
            model_selection,
            handoff_from: input.handoff_from.as_deref(),
        },
    )
    .inspect_err(|_| {
        let _ = service.owners().delete_session_resources(&session_id);
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
        model_selection: Option<qaqh_types::ModelSelection>,
        tool_mode: Option<String>,
        custom_tools: Option<Vec<String>>,
        handoff_from: Option<String>,
    ) -> QaqhResult<SessionCreateResult> {
        create_session(
            self,
            CreateSessionInput {
                cwd,
                title,
                tool_mode,
                custom_tools,
                model_selection,
                handoff_from,
            },
        )
    }
}

/// 业务行创建输入，model_selection 已由模型目录校验。
struct CreateRowInput<'a> {
    cwd: Option<&'a str>,
    title: Option<&'a str>,
    tool_mode: Option<String>,
    custom_tools: Option<Vec<String>>,
    model_selection: qaqh_types::ModelSelection,
    handoff_from: Option<&'a str>,
}

/// 会话创建的第一站:一次性落 SQLite 业务行(cwd/model/tool_mode/
/// model_selection/title)。行存在即 spawn 可读,无 meta.json 中间态。
fn create_business_row(
    service: &QaqhService,
    session_id: &str,
    input: CreateRowInput<'_>,
) -> Result<(), String> {
    let canonical_cwd = session_canonical_cwd(&service.owners, session_id, input.cwd)?;
    let row = qaqh_session::db::store::NewSession {
        session_id: session_id.to_string(),
        parent_session_id: None,
        title: input.title.map(str::to_string).unwrap_or_default(),
        title_source: input.title.map(|_| "user".to_string()),
        cwd: Some(canonical_cwd),
        model: Some(input.model_selection.model_id.clone()),
        tool_mode: input.tool_mode,
        model_selection: Some(input.model_selection),
        custom_tools: input.custom_tools,
        frozen_annotation: None,
        ephemeral: false,
    };
    let error_session_id = session_id.to_string();
    let handoff_from = input.handoff_from.map(str::to_string);
    service
        .owners
        .db()
        .write_blocking(move |conn| {
            qaqh_session::db::store::create_session(conn, &row)?;
            // ADR-0015 切片F：handoff 血缘与建行同事务——要么都有,要么都没有。
            if let Some(from) = handoff_from {
                qaqh_session::db::store::set_handoff_from(conn, &row.session_id, &from)?;
            }
            Ok::<_, qaqh_session::db::DbError>(())
        })
        .map_err(|error| format!("create business row for {error_session_id}: {error}"))
}

