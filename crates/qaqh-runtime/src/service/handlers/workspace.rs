//! `workspace.*` / `fs.*` 服务面。
//!
//! `git.*`（6 个方法：diff/branch/branches/file_diff/switch_branch/commit）
//! 2026-10-31 整体删除：唯一的消费者是 `qaqh-client` 的 `Endpoint` 枚举，而
//! 桌面端从未接线该面板。删除范围含 client 变体、命令枚举与方法表条目。

use serde_json::{Value, json};

use qaqh_ringing::{CommandTarget, RingingCommand};

use crate::error::{QaqhError, QaqhResult};
use crate::service::QaqhService;
use crate::service::command::WorkspaceCommand;
use crate::service::fs_git::{list_remote_directory, read_remote_file, workspace};

pub(crate) fn handle(service: &QaqhService, command: WorkspaceCommand) -> QaqhResult<Value> {
    match command {
        // UI 工作区注册表保存在会话 SQLite 业务库中。
        WorkspaceCommand::WorkspaceList(_) => {
            let entries = service
                .owners
                .db()
                .read_blocking(qaqh_session::db::workspace::list_entries)
                .map_err(QaqhError::message)?;
            QaqhError::to_value("workspace entries", entries)
        }
        WorkspaceCommand::WorkspaceGet(args) => {
            Ok(json!(workspace(&service.owners, &args.session_id)))
        }
        // 远端文件浏览（临时跨端模式）：路径一律是 daemon 侧绝对路径，
        // 且必须落在会话工作区根 / 数据根白名单内（T-2-1）。
        WorkspaceCommand::FsList(args) => list_remote_directory(
            &service.owners,
            &args.path,
            args.scope_session_id.as_deref(),
        ),
        WorkspaceCommand::FsRead(args) => read_remote_file(
            &service.owners,
            &args.path,
            args.max_bytes.unwrap_or(512 * 1024),
            args.scope_session_id.as_deref(),
        ),
    }
}

pub(crate) fn handle_ringing(
    service: &QaqhService,
    target: &CommandTarget,
    command: &RingingCommand,
) -> QaqhResult<Value> {
    match command {
        RingingCommand::WorkspaceSet { path } => {
            let session_id = session_target(target)?;
            let path = path.trim();
            if path.is_empty() {
                return Err(QaqhError::invalid("workspace.set: empty path rejected"));
            }
            service
                .write_meta(
                    session_id,
                    qaqh_ringing_contract::fact::SessionMetadataPatch {
                        cwd: Some(qaqh_session::grouping::canonical_cwd(std::path::Path::new(
                            path,
                        ))),
                        ..Default::default()
                    },
                )
                .map_err(QaqhError::message)?;
            let session = session_id.to_string();
            let path = path.to_string();
            service
                .owners
                .db()
                .write_blocking(move |conn| {
                    qaqh_session::db::workspace::attach_by_cwd(conn, &session, &path)
                })
                .map_err(QaqhError::message)?;
            service.send_ringing_cmd(session_id.to_string(), RingingCommand::AgentReloadConfig)?;
            Ok(Value::Null)
        }
        RingingCommand::WorkspaceCreate { path } => {
            let path = path.clone();
            let created = service
                .owners
                .db()
                .write_blocking(move |conn| qaqh_session::db::workspace::create(conn, &path))
                .map_err(QaqhError::message)?;
            QaqhError::to_value("workspace", created)
        }
        RingingCommand::WorkspaceRename { title } => {
            let workspace_id = workspace_target(target)?;
            let workspace_id = workspace_id.to_string();
            let title = title.clone();
            let renamed = service
                .owners
                .db()
                .write_blocking(move |conn| {
                    qaqh_session::db::workspace::rename(conn, &workspace_id, &title)
                })
                .map_err(QaqhError::message)?;
            QaqhError::to_value("workspace", renamed)
        }
        RingingCommand::WorkspaceDelete {} => {
            let workspace_id = workspace_target(target)?.to_string();
            service
                .owners
                .db()
                .write_blocking(move |conn| {
                    qaqh_session::db::workspace::delete(conn, &workspace_id)
                })
                .map_err(QaqhError::message)?;
            Ok(Value::Null)
        }
        RingingCommand::WorkspaceMoveSession { workspace_id } => {
            let session_id = session_target(target)?.to_string();
            let workspace_id = workspace_id.clone();
            service
                .owners
                .db()
                .write_blocking(move |conn| {
                    qaqh_session::db::workspace::move_session(conn, &session_id, &workspace_id)
                })
                .map_err(QaqhError::message)?;
            Ok(Value::Null)
        }
        RingingCommand::WorkspaceDetach {} => {
            let session_id = session_target(target)?.to_string();
            service
                .owners
                .db()
                .write_blocking(move |conn| {
                    qaqh_session::db::workspace::remove_session(conn, &session_id)
                })
                .map_err(QaqhError::message)?;
            Ok(Value::Null)
        }
        _ => Err(QaqhError::invalid(format!(
            "{} is not a workspace mutation",
            command.name()
        ))),
    }
}

fn session_target(target: &CommandTarget) -> QaqhResult<&str> {
    match target {
        CommandTarget::Session { session_id } => Ok(session_id),
        _ => Err(QaqhError::invalid("command requires a session target")),
    }
}

fn workspace_target(target: &CommandTarget) -> QaqhResult<&str> {
    match target {
        CommandTarget::Workspace { workspace_id } => Ok(workspace_id),
        _ => Err(QaqhError::invalid("command requires a workspace target")),
    }
}
