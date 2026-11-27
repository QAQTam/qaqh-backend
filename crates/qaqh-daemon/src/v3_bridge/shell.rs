use std::path::{Component, Path, PathBuf};

use qaqh_core_vocab::SessionId;
use qaqh_ringing_v3::{RpcError, ShellRequest, ShellResponse, ShellView};

use super::RealBackend;
use super::bad_request as v3_bad_request;
use crate::v3_server::error::{denied, not_found};
use crate::v3_server::facade::CallContext;
use crate::v3_server::identity::ConnectionIdentity;

pub(crate) async fn handle(
    backend: &RealBackend,
    context: &CallContext,
    request: ShellRequest,
) -> Result<ShellResponse, RpcError> {
    match request {
        ShellRequest::Open {
            session_id,
            cols,
            rows,
            cwd,
            shell,
        } => {
            if session_id.is_none() {
                context.identity.require_owner("Shell::Open")?;
            }
            if let Some(session_id) = &session_id {
                backend.session_for(context, session_id).await?;
            }
            let cwd = resolve_shell_cwd(backend, session_id.as_ref(), cwd).await?;
            let owner = context.identity.device_id.clone();
            let params = qaqh_runtime::ringing::service_methods::ShellOpenParams {
                cols: Some(cols),
                rows: Some(rows),
                cwd,
                shell,
            };
            let service = backend.service.clone();
            let shell_owner = owner.clone();
            let opened =
                RealBackend::blocking(move || service.shell_open(&shell_owner, &params)).await?;
            backend.set_shell_session(owner, opened.id.clone(), session_id);
            Ok(ShellResponse::Opened(view(opened)))
        }
        ShellRequest::Write { shell_id, bytes } => {
            authorize_shell(backend, &context.identity, &shell_id).await?;
            let owner = context.identity.device_id.clone();
            let service = backend.service.clone();
            let id = shell_id.clone();
            let written =
                RealBackend::blocking(move || service.shell_stdin(&owner, &id, &bytes)).await?;
            Ok(ShellResponse::Written {
                bytes_written: written as u64,
            })
        }
        ShellRequest::Resize {
            shell_id,
            cols,
            rows,
        } => {
            authorize_shell(backend, &context.identity, &shell_id).await?;
            let owner = context.identity.device_id.clone();
            let service = backend.service.clone();
            let id = shell_id.clone();
            RealBackend::blocking(move || service.shell_resize(&owner, &id, cols, rows)).await?;
            Ok(ShellResponse::Resized)
        }
        ShellRequest::Close { shell_id } => {
            authorize_shell(backend, &context.identity, &shell_id).await?;
            let owner = context.identity.device_id.clone();
            let service = backend.service.clone();
            let id = shell_id.clone();
            RealBackend::blocking(move || service.shell_close(&owner, &id)).await?;
            backend.remove_shell_session(&shell_id);
            Ok(ShellResponse::Closed)
        }
        ShellRequest::List { session_id } => {
            if let Some(session_id) = &session_id {
                backend.session_for(context, session_id).await?;
            }
            let owner = context.identity.device_id.clone();
            let service = backend.service.clone();
            let shells = RealBackend::blocking(move || service.shell_list(&owner))
                .await?
                .shells;
            let shells = shells
                .into_iter()
                .filter(|shell| match &session_id {
                    Some(session_id) => backend
                        .shell_session(&shell.id)
                        .is_some_and(|(_, bound)| bound == Some(session_id.clone())),
                    None => true,
                })
                .map(view)
                .collect();
            Ok(ShellResponse::Shells(shells))
        }
    }
}

pub(crate) async fn authorize_shell(
    backend: &RealBackend,
    identity: &ConnectionIdentity,
    shell_id: &str,
) -> Result<(), RpcError> {
    let Some((owner, session_id)) = backend.shell_session(shell_id) else {
        return Err(not_found(format!("没有 shell `{shell_id}`")));
    };
    if owner != identity.device_id {
        return Err(denied(format!("设备无权访问 shell `{shell_id}`")));
    }
    match session_id {
        Some(session_id) => backend
            .ensure_session_runtime(&session_id)
            .await?
            .filter(|session| session.can_access(&identity.device))
            .map(|_| ())
            .ok_or_else(|| denied(format!("设备无权访问 shell `{shell_id}`"))),
        None => Ok(()),
    }
}

async fn resolve_shell_cwd(
    backend: &RealBackend,
    session_id: Option<&SessionId>,
    requested: Option<String>,
) -> Result<Option<String>, RpcError> {
    let Some(session_id) = session_id else {
        return Ok(requested);
    };
    let service = backend.service.clone();
    let sessions = RealBackend::blocking(move || service.list_sessions(true)).await?;
    let session = sessions
        .into_iter()
        .find(|entry| entry.meta.session_id == session_id.as_str())
        .ok_or_else(|| not_found(format!("没有会话 `{session_id}`")))?;
    let root = session
        .meta
        .cwd
        .as_ref()
        .map(PathBuf::from)
        .ok_or_else(|| v3_bad_request("session has no workspace for shell open"))?;
    let root = std::fs::canonicalize(&root).map_err(|error| {
        v3_bad_request(format!(
            "canonicalize session workspace {}: {error}",
            root.display()
        ))
    })?;
    let Some(requested) = requested else {
        return Ok(Some(root.to_string_lossy().into_owned()));
    };
    let path = Path::new(&requested);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(denied("shell cwd must be a workspace-relative path"));
    }
    let candidate = qaqh_workspace::permission::resolve_target_path_in(path.to_path_buf(), &root);
    if !qaqh_workspace::permission::path_within_dir(&candidate, &root) {
        return Err(denied("shell cwd escapes the session workspace"));
    }
    Ok(Some(candidate.to_string_lossy().into_owned()))
}

fn view(shell: qaqh_runtime::ringing::service_methods::ShellView) -> ShellView {
    ShellView {
        shell_id: shell.id,
        pid: shell.pid,
        shell: shell.shell,
        cwd: shell.cwd,
        cols: shell.cols,
        rows: shell.rows,
        output_offset: shell.output_offset,
        exited: !shell.alive,
    }
}
