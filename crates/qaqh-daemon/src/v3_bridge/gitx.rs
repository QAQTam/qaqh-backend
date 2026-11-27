//! Production RingingV3 Gitx domain.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use qaqh_core_vocab::SessionId;
use qaqh_gitx::{GitxError, WorktreeManager};
use qaqh_ringing_v3::{GitStatus, GitxRequest, GitxResponse, RpcError, WorktreeView};
use qaqh_session::db::{DbError, store, workspace};

use super::bad_request;
use super::{CallContext, RealBackend};

/// Handle one Gitx RPC.
pub(super) async fn handle(
    backend: &RealBackend,
    context: &CallContext,
    request: GitxRequest,
) -> Result<GitxResponse, RpcError> {
    match request {
        GitxRequest::Status { session_id } => {
            backend.session_for(context, &session_id).await?;
            let service = backend.service.clone();
            let session = session_id.as_str().to_string();
            let status = RealBackend::blocking(move || {
                let root = super::workspace::effective_session_workspace(&service, &session)
                    .map_err(bad_request)?;
                WorktreeManager::new(root).status().map_err(map_gitx_error)
            })
            .await?;
            Ok(GitxResponse::Status(GitStatus {
                branch: status.branch.unwrap_or_else(|| {
                    if status.detached {
                        "HEAD".to_string()
                    } else {
                        "unknown".to_string()
                    }
                }),
                dirty: !status.files.is_empty(),
                ahead: status.ahead.unwrap_or(0),
                behind: status.behind.unwrap_or(0),
            }))
        }
        GitxRequest::Diff { session_id, path } => {
            backend.session_for(context, &session_id).await?;
            let service = backend.service.clone();
            let session = session_id.as_str().to_string();
            let diff = RealBackend::blocking(move || {
                let root = super::workspace::effective_session_workspace(&service, &session)
                    .map_err(bad_request)?;
                WorktreeManager::new(root)
                    .diff(path.as_deref())
                    .map_err(map_gitx_error)
            })
            .await?;
            Ok(GitxResponse::Diff(
                backend.content_ref_for_text(&session_id, diff).await?,
            ))
        }
        GitxRequest::Commit {
            session_id,
            message,
        } => {
            context.identity.require_owner("Gitx::Commit")?;
            backend.session_for(context, &session_id).await?;
            if message.trim().is_empty() {
                return Err(bad_request("commit message must not be empty"));
            }
            let service = backend.service.clone();
            let session = session_id.as_str().to_string();
            let commit = RealBackend::blocking(move || {
                let root = super::workspace::effective_session_workspace(&service, &session)
                    .map_err(bad_request)?;
                WorktreeManager::new(root)
                    .commit(&message)
                    .map_err(map_gitx_error)
            })
            .await?;
            Ok(GitxResponse::Committed { commit })
        }
        GitxRequest::Worktrees => {
            context.identity.require_owner("Gitx::Worktrees")?;
            let service = backend.service.clone();
            let worktrees = RealBackend::blocking(move || list_worktrees(&service))
                .await?
                .into_iter()
                .map(map_worktree)
                .collect::<Result<Vec<_>, _>>()?;
            Ok(GitxResponse::Worktrees(worktrees))
        }
    }
}

fn list_worktrees(
    service: &qaqh_runtime::QaqhService,
) -> Result<Vec<qaqh_ringing_contract::WorktreeView>, RpcError> {
    let roots = service
        .owners()
        .db()
        .read_blocking(|conn| {
            let mut roots = BTreeSet::new();
            for workspace in workspace::list(conn)? {
                if !workspace.path.is_empty() {
                    roots.insert(workspace.path);
                }
            }
            for session in store::list_sessions(conn, true, u32::MAX)? {
                if let Some(cwd) = session.cwd.filter(|cwd| !cwd.is_empty() && cwd != ".") {
                    roots.insert(cwd);
                }
            }
            Ok::<_, DbError>(roots.into_iter().map(PathBuf::from).collect::<Vec<_>>())
        })
        .map_err(|error| super::internal(format!("list gitx roots: {error}")))?;

    list_worktrees_in_roots(roots)
}

fn list_worktrees_in_roots(
    roots: Vec<PathBuf>,
) -> Result<Vec<qaqh_ringing_contract::WorktreeView>, RpcError> {
    let mut seen = BTreeSet::new();
    let mut worktrees = Vec::new();
    for root in roots {
        if !root.is_dir() {
            continue;
        }
        let manager = WorktreeManager::new(&root);
        let listed = match manager.list() {
            Ok(listed) => listed,
            Err(GitxError::NotARepository(_)) => continue,
            Err(error) => return Err(map_gitx_error(error)),
        };
        for worktree in listed {
            let key = canonical_key(Path::new(&worktree.path))?;
            if seen.insert(key) {
                worktrees.push(worktree);
            }
        }
    }
    worktrees.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(worktrees)
}

fn canonical_key(path: &Path) -> Result<String, RpcError> {
    std::fs::canonicalize(path)
        .map(|path| path.to_string_lossy().into_owned())
        .map_err(|error| {
            super::internal(format!("canonicalize worktree {}: {error}", path.display()))
        })
}

fn map_worktree(worktree: qaqh_ringing_contract::WorktreeView) -> Result<WorktreeView, RpcError> {
    let session_id = worktree
        .owner_session_id
        .map(|id| {
            SessionId::new(id)
                .map_err(|error| bad_request(format!("invalid worktree session id: {error}")))
        })
        .transpose()?;
    Ok(WorktreeView {
        path: worktree.path,
        branch: worktree.branch,
        session_id,
    })
}

fn map_gitx_error(error: GitxError) -> RpcError {
    match &error {
        GitxError::Io(io)
            if io.kind() == std::io::ErrorKind::InvalidInput
                || io.kind() == std::io::ErrorKind::NotFound =>
        {
            bad_request(error.to_string())
        }
        GitxError::NotARepository(_) | GitxError::GitUnavailable => bad_request(error.to_string()),
        _ => super::internal(format!("gitx operation failed: {error}")),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::process::Command;

    use super::*;

    #[test]
    fn list_worktrees_skips_missing_and_non_git_directories() {
        let repo = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .current_dir(repo.path())
                    .args(args)
                    .status()
                    .unwrap()
                    .success(),
                "git {args:?} failed"
            );
        };
        run(&["init", "-q"]);
        run(&["config", "user.name", "Test"]);
        run(&["config", "user.email", "test@example.com"]);
        std::fs::write(repo.path().join("README.md"), "hi\n").unwrap();
        run(&["add", "README.md"]);
        run(&["commit", "-q", "-m", "init"]);

        WorktreeManager::new(repo.path())
            .create(
                &qaqh_ringing_contract::WorktreeCreateParams {
                    branch_name: "wt-1".to_string(),
                    base_ref: None,
                    attach: false,
                },
                None,
            )
            .unwrap();
        let plain = tempfile::tempdir().unwrap();
        let missing = repo.path().join("deleted");

        let worktrees = list_worktrees_in_roots(vec![
            missing,
            plain.path().to_path_buf(),
            repo.path().to_path_buf(),
        ])
        .unwrap();
        assert_eq!(worktrees.len(), 1);
    }
}
