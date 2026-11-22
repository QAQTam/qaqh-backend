//! `gitx.*` 服务面（ADR-0041）：git 快照与 worktree 隔离命令。
//!
//! 作用域约定：
//! - **读面**（snapshot / worktree_list）经 [`ensure_session_workspace`] 解析——
//!   绑定生效的会话动态落到 worktree 根，前端视图与模型工具视图同源；
//! - **写面**（create / remove / bind / unbind）经 `session_cwd` 解析——注册表只
//!   存在于**源工作区**（`{cwd}/.qaqh/worktree/registry.json`），绕开"已绑定
//!   会话再解析出 worktree 路径"的自指陷阱；
//! - 绑定不写 `sessions.cwd`：`.qaqh` 工件继续锚定源工作区（双锚点，ADR-0041
//!   决策 3），workspace 的切换是 registry spawn 的派生 fallback。

use std::path::{Path, PathBuf};

use serde_json::Value;

use qaqh_ringing_contract::{
    GitxWorktreeCreateResult, GitxWorktreeListResult, GitxWorktreeRemoveResult,
    GitxWorktreeUnbindResult, WorktreeBindingView, WorktreeCreateParams,
};

use crate::error::{QaqhError, QaqhResult};
use crate::registry::spawn::{ensure_session_workspace, session_cwd};
use crate::service::QaqhService;
use crate::service::command::GitxCommand;

pub(crate) fn handle(service: &QaqhService, command: GitxCommand) -> QaqhResult<Value> {
    match command {
        GitxCommand::GitxSnapshot(args) => {
            let root = scoped_workspace(service, args.scope_session_id.as_deref())?;
            let snapshot = qaqh_gitx::WorktreeManager::new(root)
                .snapshot()
                .map_err(QaqhError::message)?;
            QaqhError::to_value("git snapshot", snapshot)
        }
        GitxCommand::GitxStatus(args) => {
            let root = scoped_workspace(service, args.scope_session_id.as_deref())?;
            let status = qaqh_gitx::WorktreeManager::new(root)
                .status()
                .map_err(QaqhError::message)?;
            QaqhError::to_value("git status", status)
        }
        GitxCommand::GitxWorktreeList(args) => {
            let root = scoped_workspace(service, args.scope_session_id.as_deref())?;
            let manager = qaqh_gitx::WorktreeManager::new(root);
            let worktrees = manager.list().map_err(QaqhError::message)?;
            let unmanaged = manager.list_unmanaged().map_err(QaqhError::message)?;
            QaqhError::to_value(
                "worktree list",
                GitxWorktreeListResult {
                    worktrees,
                    unmanaged_paths: unmanaged
                        .iter()
                        .map(|path| path.to_string_lossy().into_owned())
                        .collect(),
                },
            )
        }
        GitxCommand::GitxWorktreeCreate(args) => {
            let source = scoped_workspace(service, args.scope_session_id.as_deref())?;
            let attach_session = match (args.attach, args.scope_session_id.as_deref()) {
                // attach 必须指名会话：绑定是会话级属性，daemon cwd 无主可绑。
                (true, Some(session_id)) if !session_id.is_empty() => Some(session_id),
                (true, _) => {
                    return Err(QaqhError::invalid(
                        "gitx.worktree_create: attach=true requires scope_session_id",
                    ));
                }
                (false, _) => None,
            };
            let manager = qaqh_gitx::WorktreeManager::new(&source);
            // attach 前置检查：一会话至多一 worktree（引擎 create 的 owner 登记
            // 无 CAS，绕过本检查会造成双绑；与模型工具 tools.rs 同一口径）。
            if let Some(session_id) = attach_session
                && manager
                    .record_for_session(session_id)
                    .map_err(QaqhError::message)?
                    .is_some()
            {
                return Err(QaqhError::invalid(format!(
                    "gitx.worktree_create: session {session_id} is already bound to a worktree (unbind first)"
                )));
            }
            let params = WorktreeCreateParams {
                branch_name: args.branch_name,
                base_ref: args.base_ref,
                attach: args.attach,
            };
            let worktree = manager
                .create(&params, attach_session)
                .map_err(QaqhError::message)?;
            let binding = match attach_session {
                None => None,
                Some(session_id) => {
                    let bound = manager
                        .bind(&worktree.id, session_id)
                        .map_err(QaqhError::message)?;
                    Some(binding_view(bound))
                }
            };
            QaqhError::to_value(
                "worktree create",
                GitxWorktreeCreateResult { worktree, binding },
            )
        }
        GitxCommand::GitxWorktreeRemove(args) => {
            let source = scoped_workspace(service, args.scope_session_id.as_deref())?;
            qaqh_gitx::WorktreeManager::new(&source)
                .remove(
                    &args.worktree_id,
                    args.force,
                    args.scope_session_id.as_deref(),
                )
                .map_err(QaqhError::message)?;
            QaqhError::to_value(
                "worktree remove",
                GitxWorktreeRemoveResult { removed: true },
            )
        }
        GitxCommand::GitxWorktreeBind(args) => {
            let source = source_workspace(service, &args.session_id)?;
            let manager = qaqh_gitx::WorktreeManager::new(&source);
            // 已绑定的会话必须先 unbind：一个 worktree 同时至多一个会话，一个
            // 会话同时至多一个 worktree（registry CAS 是唯一仲裁点）。
            if let Some(existing) = manager
                .record_for_session(&args.session_id)
                .map_err(QaqhError::message)?
            {
                return Err(QaqhError::invalid(format!(
                    "gitx.worktree_bind: session {} already bound to {} (unbind first)",
                    args.session_id,
                    existing.path.display()
                )));
            }
            let bound = manager
                .bind(&args.worktree_id, &args.session_id)
                .map_err(QaqhError::message)?;
            QaqhError::to_value("worktree binding", binding_view(bound))
        }
        GitxCommand::GitxWorktreeUnbind(args) => {
            let source = source_workspace(service, &args.session_id)?;
            let manager = qaqh_gitx::WorktreeManager::new(&source);
            let Some(record) = manager
                .record_for_session(&args.session_id)
                .map_err(QaqhError::message)?
            else {
                // 幂等：会话本无绑定，照常成功。
                return QaqhError::to_value(
                    "worktree unbind",
                    GitxWorktreeUnbindResult { unbound: false },
                );
            };
            manager
                .unbind(&record.id, &args.session_id)
                .map_err(QaqhError::message)?;
            QaqhError::to_value("worktree unbind", GitxWorktreeUnbindResult { unbound: true })
        }
    }
}

/// 读面作用域解析：会话 → 其生效工作区（含 worktree fallback）；否则 daemon cwd。
fn scoped_workspace(
    service: &QaqhService,
    scope_session_id: Option<&str>,
) -> QaqhResult<PathBuf> {
    match scope_session_id.filter(|session_id| !session_id.is_empty()) {
        Some(session_id) => {
            ensure_session_workspace(&service.owners, session_id).map_err(QaqhError::message)
        }
        None => std::env::current_dir()
            .map_err(|error| QaqhError::message(format!("gitx: resolve cwd: {error}"))),
    }
}

/// 写面作用域解析：必须指名会话，且取**源工作区**（注册表所在地）。
fn source_workspace(service: &QaqhService, session_id: &str) -> QaqhResult<PathBuf> {
    let cwd = session_cwd(&service.owners, session_id)
        .filter(|cwd| !cwd.is_empty() && Path::new(cwd).is_dir())
        .ok_or_else(|| {
            QaqhError::message(format!(
                "gitx: session {session_id} has no bound workspace"
            ))
        })?;
    Ok(PathBuf::from(cwd))
}

fn binding_view(bound: qaqh_gitx::WorktreeBinding) -> WorktreeBindingView {
    WorktreeBindingView {
        worktree_root: bound.worktree_root.to_string_lossy().into_owned(),
        source_root: bound.source_root.to_string_lossy().into_owned(),
        branch: bound.branch,
        head_sha: bound.head_sha,
        owner_session_id: bound.owner_session_id,
    }
}
