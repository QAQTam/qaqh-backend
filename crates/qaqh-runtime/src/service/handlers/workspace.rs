//! `workspace.*` / `fs.*` 服务面。
//!
//! `git.*`（6 个方法：diff/branch/branches/file_diff/switch_branch/commit）
//! 2026-10-31 整体删除：唯一的消费者是 `qaqh-client` 的 `Endpoint` 枚举，而
//! 桌面端从未接线该面板。删除范围含 client 变体、命令枚举与方法表条目。

use serde_json::{Value, json};

use qaqh_domain::ControlCommand;
use qaqh_ringing::RingingCommand;

use crate::error::{QaqhError, QaqhResult};
use crate::service::QaqhService;
use crate::service::command::WorkspaceCommand;
use crate::service::fs_git::{list_remote_directory, read_remote_file, workspace};

pub(crate) fn handle(service: &QaqhService, command: WorkspaceCommand) -> QaqhResult<Value> {
    match command {
        // UI 工作区注册表（组织语义，与运行环境 workspace 解耦）。
        // 一手 DTO：条目形状由 WorkspaceStore::list_entries 唯一定义。
        WorkspaceCommand::WorkspaceList(_) => {
            let entries = qaqh_session::WorkspaceStore::global().list_entries();
            QaqhError::to_value("workspace entries", entries)
        }
        WorkspaceCommand::WorkspaceGet(args) => {
            Ok(json!(workspace(&service.owners, &args.session_id)))
        }
        WorkspaceCommand::WorkspaceSet(args) => {
            // 空 path 防护：canonical_cwd("") = "" 会把 meta.cwd 清空，
            // 导致会话工作区/归属丢失（前端重启后回空 cwd 的 bug 通道）。
            let path = args.path.trim().to_string();
            if path.is_empty() {
                return Err(QaqhError::invalid("workspace.set: empty path rejected"));
            }
            // 统一数据源：运行环境工作目录是 journal 里的 `cwd`。
            service
                .write_meta(
                    &args.session_id,
                    qaqh_session::session_fact_v2::SessionMetadataPatch {
                        cwd: Some(qaqh_session::grouping::canonical_cwd(std::path::Path::new(
                            &path,
                        ))),
                        ..Default::default()
                    },
                )
                .map_err(QaqhError::message)?;
            qaqh_session::WorkspaceStore::global().attach_by_cwd(&args.session_id, &path);
            service.send_ringing_cmd(
                args.session_id,
                RingingCommand::Control(ControlCommand::AgentReloadConfig),
            )?;
            Ok(Value::Null)
        }
        WorkspaceCommand::WorkspaceCreate(args) => {
            let store = qaqh_session::WorkspaceStore::global();
            let existing: Vec<(String, Option<String>)> = service
                .owners
                .db()
                .read_blocking(|conn| qaqh_session::db::store::list_sessions(conn, false, 256))
                .unwrap_or_default()
                .into_iter()
                .map(|row| (row.session_id, row.cwd))
                .collect();
            let created = store
                .create(&args.path, &existing)
                .map_err(QaqhError::message)?;
            QaqhError::to_value("workspace", created)
        }
        WorkspaceCommand::WorkspaceRename(args) => {
            let renamed = qaqh_session::WorkspaceStore::global()
                .rename(&args.id, args.title)
                .map_err(QaqhError::message)?;
            QaqhError::to_value("workspace", renamed)
        }
        WorkspaceCommand::WorkspaceDelete(args) => {
            qaqh_session::WorkspaceStore::global()
                .delete(&args.id)
                .map_err(QaqhError::message)?;
            Ok(Value::Null)
        }
        WorkspaceCommand::WorkspaceMoveSession(args) => {
            qaqh_session::WorkspaceStore::global()
                .move_session(&args.session_id, &args.workspace_id)
                .map_err(QaqhError::message)?;
            Ok(Value::Null)
        }
        WorkspaceCommand::WorkspaceDetach(args) => {
            qaqh_session::WorkspaceStore::global().remove_session(&args.session_id);
            Ok(Value::Null)
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
