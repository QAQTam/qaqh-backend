//! Production RingingV3 Workspace domain.

use std::path::{Path, PathBuf};

use qaqh_core_vocab::{SessionId, WorkspaceId};
use qaqh_ringing_v3::{RpcError, WorkspaceEntry, WorkspaceRequest, WorkspaceResponse};
use qaqh_runtime::QaqhService;
use qaqh_session::db::{DbError, store, workspace};

use super::bad_request;
use super::{CallContext, RealBackend};

/// Handle one Workspace RPC.
pub(super) async fn handle(
    backend: &RealBackend,
    context: &CallContext,
    request: WorkspaceRequest,
) -> Result<WorkspaceResponse, RpcError> {
    match request {
        WorkspaceRequest::List => {
            context.identity.require_owner("Workspace::List")?;
            let service = backend.service.clone();
            let entries = RealBackend::blocking(move || {
                service
                    .owners()
                    .db()
                    .read_blocking(|conn| Ok::<_, DbError>(workspace::list_entries(conn)?))
                    .map_err(map_db_error)?
                    .into_iter()
                    .map(map_entry)
                    .collect::<Result<Vec<_>, _>>()
            })
            .await?;
            Ok(WorkspaceResponse::List(entries))
        }
        WorkspaceRequest::Create { path } => {
            context.identity.require_owner("Workspace::Create")?;
            let path = path.trim().to_string();
            if path.is_empty() {
                return Err(bad_request("workspace path must not be empty"));
            }
            let service = backend.service.clone();
            let created = RealBackend::blocking(move || {
                service
                    .owners()
                    .db()
                    .write_blocking(move |conn| workspace::create(conn, &path))
                    .map_err(map_db_error)
            })
            .await?;
            Ok(WorkspaceResponse::Created(map_entry(created.entry(false))?))
        }
        WorkspaceRequest::Rename {
            workspace_id,
            title,
        } => {
            context.identity.require_owner("Workspace::Rename")?;
            let title = title.trim().to_string();
            if title.is_empty() {
                return Err(bad_request("workspace title must not be empty"));
            }
            let workspace_id = workspace_id.as_str().to_string();
            let service = backend.service.clone();
            RealBackend::blocking(move || {
                service
                    .owners()
                    .db()
                    .write_blocking(move |conn| workspace::rename(conn, &workspace_id, &title))
                    .map(|_| ())
                    .map_err(map_db_error)
            })
            .await?;
            Ok(WorkspaceResponse::Ack)
        }
        WorkspaceRequest::Delete { workspace_id } => {
            context.identity.require_owner("Workspace::Delete")?;
            let workspace_id = workspace_id.as_str().to_string();
            let service = backend.service.clone();
            RealBackend::blocking(move || {
                service
                    .owners()
                    .db()
                    .write_blocking(move |conn| workspace::delete(conn, &workspace_id))
                    .map(|_| ())
                    .map_err(map_db_error)
            })
            .await?;
            Ok(WorkspaceResponse::Ack)
        }
        WorkspaceRequest::MoveSession {
            session_id,
            workspace_id,
        } => {
            context.identity.require_owner("Workspace::MoveSession")?;
            backend.session_for(context, &session_id).await?;
            let session_id = session_id.as_str().to_string();
            let workspace_id = workspace_id.as_str().to_string();
            let service = backend.service.clone();
            RealBackend::blocking(move || {
                service
                    .owners()
                    .db()
                    .write_blocking(move |conn| {
                        if !store::session_exists(conn, &session_id)? {
                            return Err(DbError::SessionNotFound {
                                session_id: session_id.clone(),
                            });
                        }
                        workspace::move_session(conn, &session_id, &workspace_id)
                    })
                    .map(|_| ())
                    .map_err(map_db_error)
            })
            .await?;
            Ok(WorkspaceResponse::Ack)
        }
    }
}

/// Resolve a session's effective tool workspace, including its Gitx worktree binding.
///
/// This mirrors the runtime's session workspace contract: an explicit cwd is
/// authoritative, a live worktree binding wins, and an unbound session receives
/// its deterministic per-session workspace under the data root.
pub(super) fn effective_session_workspace(
    service: &QaqhService,
    session_id: &str,
) -> Result<PathBuf, String> {
    let owned = session_id.to_string();
    let cwd = service
        .owners()
        .db()
        .read_blocking(move |conn| store::session_row(conn, &owned))
        .map_err(|error| error.to_string())?
        .and_then(|row| row.cwd)
        .filter(|cwd| !cwd.is_empty() && cwd != ".");

    let Some(cwd) = cwd else {
        let path = qaqh_types::platform::default_session_workspace_dir(session_id);
        std::fs::create_dir_all(&path)
            .map_err(|error| format!("create session workspace {}: {error}", path.display()))?;
        return Ok(path);
    };

    let path = PathBuf::from(cwd);
    if !path.is_dir() {
        if !qaqh_types::platform::is_managed_session_workspace(&path) {
            return Err(format!(
                "session workspace does not exist: {}",
                path.display()
            ));
        }
        std::fs::create_dir_all(&path)
            .map_err(|error| format!("create session workspace {}: {error}", path.display()))?;
    }

    match qaqh_gitx::WorktreeManager::new(&path).record_for_session(session_id) {
        Ok(Some(record)) if record.path.is_dir() => Ok(record.path),
        Ok(Some(record)) => Err(format!(
            "session {session_id} is bound to missing worktree {}",
            record.path.display()
        )),
        Ok(None) => Ok(path),
        Err(error) => Err(format!(
            "session {session_id} worktree lookup failed: {error}"
        )),
    }
}

/// Return whether a path is the workspace root itself or one of its descendants.
pub(super) fn path_is_within(path: &Path, root: &Path) -> bool {
    let path = qaqh_types::platform::strip_verbatim(path);
    let root = qaqh_types::platform::strip_verbatim(root);
    path.starts_with(root)
}

fn map_entry(entry: qaqh_ringing_contract::WorkspaceEntry) -> Result<WorkspaceEntry, RpcError> {
    let id = WorkspaceId::new(entry.id)
        .map_err(|error| bad_request(format!("invalid persisted workspace id: {error}")))?;
    let session_ids = entry
        .session_ids
        .into_iter()
        .map(|id| {
            SessionId::new(id)
                .map_err(|error| bad_request(format!("invalid persisted session id: {error}")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(WorkspaceEntry {
        id,
        path: entry.path,
        title: entry.title,
        order: entry.order,
        session_ids,
        missing_dir: entry.missing_dir,
    })
}

fn map_db_error(error: DbError) -> RpcError {
    match error {
        DbError::Workspace(message) => bad_request(message),
        DbError::SessionNotFound { session_id } => {
            bad_request(format!("session `{session_id}` does not exist"))
        }
        other => super::internal(format!("workspace database operation failed: {other}")),
    }
}
