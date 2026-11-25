//! `worktree_*` 模型工具面（ADR-0041 切片 2）。
//!
//! 与 goal 工具面（`qaqh-subagent::goal`）同款先例：领域 crate 自带 TypedTool 面 + 宿主 trait，
//! daemon 装配时经 [`install_host`] 注入进程内实现（qaqh-runtime `gitx_host`），
//! 工具 handler 经宿主句柄解析**源工作区**后直达 [`WorktreeManager`]。
//! 未安装宿主即报错，不做降级。
//!
//! 作用域纪律（对齐服务面 `service/handlers/gitx.rs`）：五个工具全部锚定
//! **源工作区**（注册表所在地）。读面走源根是因为注册表只在源工作区——绑定
//! 生效后 `ctx.workspace_root` 落在 worktree，其内没有（也不该有）`.qaqh/`
//! 注册表，从那里 list 只能看到空表；源根视角才是管理面的完整视图。
//!
//! 「会话空闲才可切换」（批准口径）的落点：
//! - 模型工具调用按会话串行执行（session_loop 单循环顺序 await），工具调用
//!   本身就是该会话唯一在飞的工作，结构性满足空闲；
//! - 回合拆除竞态在 [`ensure_switchable`] 拦截（turn 已取消即拒绝）；
//! - 跨会话占用竞争由 registry CAS 仲裁（`OwnerConflict`）；
//! - 切换一律在回合边界生效（`SessionLoop::effective_workspace` 每回合经
//!   resolver 重解析），不引入 dispatch 级重派生。
//!
//! 已知口径：`create(attach=true)` 在工具层先查绑定（一会话至多一 worktree）；
//! 引擎侧 `WorktreeManager::create` 的 owner 登记不做 CAS（v1 已知限制），
//! 服务面 handler 同样在 attach 分支前置检查（两侧口径一致）。

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use qaqh_ringing_contract::{
    GitxWorktreeCreateResult, GitxWorktreeListResult, GitxWorktreeRemoveResult,
    GitxWorktreeUnbindResult, WorktreeBindingView, WorktreeCreateParams,
};
use qaqh_tool_core::tool_api::{
    ToolBody, ToolCallContext, ToolContentBlock, ToolDisplay, ToolError, ToolErrorCode,
    ToolErrorKind, ToolExecutionError, ToolHeader, ToolMeta, ToolProjection, TypedTool,
};
use qaqh_workspace::ToolManager;
use qaqh_workspace::ToolRisk;
use qaqh_workspace::permission::ToolCategory;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::GitxError;
use crate::WorktreeManager;

// ── 宿主（daemon 装配期注入）────────────────────────────────────────────────

/// gitx 工具的进程内宿主：把会话映射到其**源工作区**根。
///
/// qaqh-runtime 侧实现（`gitx_host.rs`）复用 registry 的 `session_cwd` 单一
/// 路径，与服务面写面作用域解析同源。
pub trait GitxHost: Send + Sync {
    /// 会话绑定的源工作区根；`None` = 会话没有可用工作区（工具报
    /// `workspace_unbound`）。
    fn source_workspace(&self, session_id: &str) -> Option<PathBuf>;
}

static HOST: OnceLock<Arc<dyn GitxHost>> = OnceLock::new();

/// 安装进程内 gitx 宿主（幂等，首个生效）。
pub fn install_host(host: Arc<dyn GitxHost>) {
    if HOST.set(host).is_ok() {
        log::info!("[gitx] in-process gitx host installed");
    }
}

/// 已安装的 gitx 宿主；未安装返回 `None`。
#[must_use]
pub fn host() -> Option<Arc<dyn GitxHost>> {
    HOST.get().cloned()
}

// ── 错误映射 ────────────────────────────────────────────────────────────────

/// 结构化工具错误：kind 由词表映射，code 保持 gitx 词表原文。
fn gitx_error(code: &'static str, message: impl Into<String>) -> ToolExecutionError {
    let kind = match code {
        "host_unavailable" | "workspace_unbound" => ToolErrorKind::Unavailable,
        "bind_conflict" | "dirty_worktree" | "switch_busy" => ToolErrorKind::Conflict,
        "not_owner" => ToolErrorKind::PermissionDenied,
        "not_managed" => ToolErrorKind::NotFound,
        "invalid_branch" => ToolErrorKind::InvalidArguments,
        _ => ToolErrorKind::Execution,
    };
    let mut error = ToolError::new(kind, message).with_hint(hint_for(code));
    error.code = ToolErrorCode::parse_or_builtin(code, kind);
    ToolExecutionError::Recoverable(error)
}

/// 恢复动作提示（引擎错误只描述「发生了什么」，动作建议在工具层给出）。
fn hint_for(code: &str) -> &'static str {
    match code {
        "bind_conflict" => {
            "a session holds at most one worktree: call worktree_unbind first, or pick a free worktree"
        }
        "dirty_worktree" => "commit or discard the worktree changes, or retry with force=true",
        "not_owner" => "only the owning session may remove this worktree",
        "not_managed" => "call worktree_list to get valid worktree ids",
        "invalid_branch" => "use a valid git branch name that does not exist yet",
        "not_a_repository" => "the session workspace is not inside a git repository",
        "host_unavailable" => "the daemon must install the gitx host at startup",
        "workspace_unbound" => "attach the session to a project workspace first",
        "switch_busy" => "retry after the current turn finishes",
        _ => "check the workspace git state and retry",
    }
}

/// [`GitxError`] → 工具可恢复错误（词表逐项映射，不吞类型信息）。
fn engine_error(operation: &'static str, error: GitxError) -> ToolExecutionError {
    let (code, message) = match &error {
        GitxError::Git { command, stderr } => (
            "git_failed",
            format!("{operation}: git `{command}` failed: {stderr}"),
        ),
        GitxError::GitUnavailable => (
            "git_unavailable",
            format!("{operation}: git executable not found on PATH"),
        ),
        GitxError::Io(inner) => ("gitx_io", format!("{operation}: io: {inner}")),
        GitxError::Json(inner) => (
            "gitx_registry",
            format!("{operation}: registry json: {inner}"),
        ),
        GitxError::NotARepository(path) => (
            "not_a_repository",
            format!("{operation}: not a git repository: {path}"),
        ),
        GitxError::InvalidBranch(name) => (
            "invalid_branch",
            format!("{operation}: invalid branch name `{name}`"),
        ),
        GitxError::DirtyWorktree(id) => (
            "dirty_worktree",
            format!("{operation}: worktree `{id}` has uncommitted changes"),
        ),
        GitxError::NotManaged(id) => (
            "not_managed",
            format!("{operation}: worktree `{id}` is not managed by this registry"),
        ),
        GitxError::OwnerConflict(id, owner) => (
            "bind_conflict",
            format!("{operation}: worktree `{id}` is already bound to session `{owner}`"),
        ),
        GitxError::NotOwner(id, session) => (
            "not_owner",
            format!("{operation}: session `{session}` does not own worktree `{id}`"),
        ),
    };
    gitx_error(code, message)
}

// ── 内核函数（源工作区锚定，测试直接驱动）───────────────────────────────────

/// 解析调用会话的源工作区（宿主单一路径）。
fn resolve_source(ctx: &ToolCallContext) -> Result<PathBuf, ToolExecutionError> {
    let host = host().ok_or_else(|| {
        gitx_error(
            "host_unavailable",
            "worktree tools require the daemon gitx host (install_host)",
        )
    })?;
    host.source_workspace(&ctx.session_id).ok_or_else(|| {
        gitx_error(
            "workspace_unbound",
            format!(
                "session {} has no bound workspace; worktree tools need a project directory",
                ctx.session_id
            ),
        )
    })
}

/// 切换类工具的会话空闲闸（模块文档「会话空闲才可切换」一节的机制落点）。
fn ensure_switchable(ctx: &ToolCallContext) -> Result<(), ToolExecutionError> {
    if ctx.cancellation.is_cancelled() {
        return Err(gitx_error(
            "switch_busy",
            "turn is being cancelled; the worktree switch was rejected",
        ));
    }
    Ok(())
}

/// `worktree_list` 内核：managed 注册表 + unmanaged 发现式枚举（源根视角）。
pub(crate) fn list_worktrees(
    source: &Path,
) -> Result<GitxWorktreeListResult, ToolExecutionError> {
    let manager = WorktreeManager::new(source);
    let worktrees = manager
        .list()
        .map_err(|error| engine_error("worktree_list", error))?;
    let unmanaged_paths = manager
        .list_unmanaged()
        .map_err(|error| engine_error("worktree_list", error))?
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    Ok(GitxWorktreeListResult {
        worktrees,
        unmanaged_paths,
    })
}

/// `worktree_create` 内核：建 worktree；`attach` 会话时先查绑定再登记 + CAS。
pub(crate) fn create_worktree(
    source: &Path,
    branch_name: &str,
    base_ref: Option<&str>,
    attach_session: Option<&str>,
) -> Result<GitxWorktreeCreateResult, ToolExecutionError> {
    let manager = WorktreeManager::new(source);
    // attach 前置检查：一会话至多一 worktree（引擎 create 的 owner 登记无 CAS）。
    if let Some(session_id) = attach_session
        && manager
            .record_for_session(session_id)
            .map_err(|error| engine_error("worktree_create", error))?
            .is_some()
    {
        return Err(gitx_error(
            "bind_conflict",
            format!(
                "worktree_create: session {session_id} is already bound to a worktree (unbind first)"
            ),
        ));
    }
    let params = WorktreeCreateParams {
        branch_name: branch_name.to_string(),
        base_ref: base_ref.map(str::to_string),
        attach: attach_session.is_some(),
    };
    let worktree = manager
        .create(&params, attach_session)
        .map_err(|error| engine_error("worktree_create", error))?;
    let binding = match attach_session {
        None => None,
        Some(session_id) => Some(binding_view(
            manager
                .bind(&worktree.id, session_id)
                .map_err(|error| engine_error("worktree_create", error))?,
        )),
    };
    Ok(GitxWorktreeCreateResult { worktree, binding })
}

/// `worktree_bind` 内核：先查自身绑定（重复绑定显式报错），再 CAS 占用。
pub(crate) fn bind_worktree(
    source: &Path,
    worktree_id: &str,
    session_id: &str,
) -> Result<WorktreeBindingView, ToolExecutionError> {
    let manager = WorktreeManager::new(source);
    if let Some(existing) = manager
        .record_for_session(session_id)
        .map_err(|error| engine_error("worktree_bind", error))?
    {
        return Err(gitx_error(
            "bind_conflict",
            format!(
                "worktree_bind: session {session_id} is already bound to {} (unbind first)",
                existing.path.display()
            ),
        ));
    }
    manager
        .bind(worktree_id, session_id)
        .map_err(|error| engine_error("worktree_bind", error))
        .map(binding_view)
}

/// `worktree_unbind` 内核：无绑定时幂等成功（`unbound = false`）。
pub(crate) fn unbind_worktree(
    source: &Path,
    session_id: &str,
) -> Result<GitxWorktreeUnbindResult, ToolExecutionError> {
    let manager = WorktreeManager::new(source);
    let Some(record) = manager
        .record_for_session(session_id)
        .map_err(|error| engine_error("worktree_unbind", error))?
    else {
        return Ok(GitxWorktreeUnbindResult { unbound: false });
    };
    manager
        .unbind(&record.id, session_id)
        .map_err(|error| engine_error("worktree_unbind", error))?;
    Ok(GitxWorktreeUnbindResult { unbound: true })
}

/// `worktree_remove` 内核：owner 校验 + dirty 保护由引擎负责。
pub(crate) fn remove_worktree(
    source: &Path,
    worktree_id: &str,
    force: bool,
    requestor_session: Option<&str>,
) -> Result<GitxWorktreeRemoveResult, ToolExecutionError> {
    WorktreeManager::new(source)
        .remove(worktree_id, force, requestor_session)
        .map_err(|error| engine_error("worktree_remove", error))?;
    Ok(GitxWorktreeRemoveResult { removed: true })
}

/// 引擎绑定 → 契约读模型（与服务面 handler 同款）。
fn binding_view(bound: crate::WorktreeBinding) -> WorktreeBindingView {
    WorktreeBindingView {
        worktree_root: bound.worktree_root.to_string_lossy().into_owned(),
        source_root: bound.source_root.to_string_lossy().into_owned(),
        branch: bound.branch,
        head_sha: bound.head_sha,
        owner_session_id: bound.owner_session_id,
    }
}

// ── 工具 I/O 类型 ───────────────────────────────────────────────────────────

/// 结果包装：`result` 即 `gitx.*` 契约 JSON（`qaqh.ringing.gitx/v1` 形状）。
#[derive(Debug, Serialize, JsonSchema)]
pub struct WorktreeListToolOutput {
    pub result: GitxWorktreeListResult,
}

impl ToolProjection for WorktreeListToolOutput {
    fn model_blocks(&self) -> Vec<ToolContentBlock> {
        vec![ToolContentBlock::Text {
            text: serde_json::to_string(&self.result).unwrap_or_default(),
        }]
    }

    fn display(&self, _args: &serde_json::Value) -> ToolDisplay {
        let summary = format!(
            "{} managed worktrees, {} unmanaged",
            self.result.worktrees.len(),
            self.result.unmanaged_paths.len()
        );
        ToolDisplay::new(
            ToolHeader::Other {
                label: "worktrees".to_string(),
            },
            ToolBody::Text {
                text: summary.clone(),
                truncated: false,
            },
        )
        .with_summary(summary)
    }
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct WorktreeCreateToolOutput {
    pub result: GitxWorktreeCreateResult,
}

impl ToolProjection for WorktreeCreateToolOutput {
    fn model_blocks(&self) -> Vec<ToolContentBlock> {
        vec![ToolContentBlock::Text {
            text: serde_json::to_string(&self.result).unwrap_or_default(),
        }]
    }

    fn display(&self, _args: &serde_json::Value) -> ToolDisplay {
        let summary = format!(
            "{} → {}",
            self.result.worktree.branch, self.result.worktree.path
        );
        ToolDisplay::new(
            ToolHeader::Path {
                path: self.result.worktree.path.clone(),
                op: qaqh_workspace::tool_api::PathOp::Write,
            },
            ToolBody::Text {
                text: summary.clone(),
                truncated: false,
            },
        )
        .with_summary(summary)
    }
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct WorktreeBindToolOutput {
    pub binding: WorktreeBindingView,
}

impl ToolProjection for WorktreeBindToolOutput {
    fn model_blocks(&self) -> Vec<ToolContentBlock> {
        vec![ToolContentBlock::Text {
            text: serde_json::to_string(&self.binding).unwrap_or_default(),
        }]
    }

    fn display(&self, _args: &serde_json::Value) -> ToolDisplay {
        let summary = format!("{} @ {}", self.binding.branch, self.binding.worktree_root);
        ToolDisplay::new(
            ToolHeader::Other {
                label: "worktree-bind".to_string(),
            },
            ToolBody::Text {
                text: summary.clone(),
                truncated: false,
            },
        )
        .with_summary(summary)
    }
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct WorktreeUnbindToolOutput {
    pub result: GitxWorktreeUnbindResult,
}

impl ToolProjection for WorktreeUnbindToolOutput {
    fn model_blocks(&self) -> Vec<ToolContentBlock> {
        vec![ToolContentBlock::Text {
            text: serde_json::to_string(&self.result).unwrap_or_default(),
        }]
    }

    fn display(&self, _args: &serde_json::Value) -> ToolDisplay {
        let summary = if self.result.unbound {
            "binding released".to_string()
        } else {
            "session had no worktree binding".to_string()
        };
        ToolDisplay::new(
            ToolHeader::Other {
                label: "worktree-unbind".to_string(),
            },
            ToolBody::Text {
                text: summary.clone(),
                truncated: false,
            },
        )
        .with_summary(summary)
    }
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct WorktreeRemoveToolOutput {
    pub result: GitxWorktreeRemoveResult,
}

impl ToolProjection for WorktreeRemoveToolOutput {
    fn model_blocks(&self) -> Vec<ToolContentBlock> {
        vec![ToolContentBlock::Text {
            text: serde_json::to_string(&self.result).unwrap_or_default(),
        }]
    }

    fn display(&self, _args: &serde_json::Value) -> ToolDisplay {
        let summary = "worktree removed".to_string();
        ToolDisplay::new(
            ToolHeader::Other {
                label: "worktree-remove".to_string(),
            },
            ToolBody::Text {
                text: summary.clone(),
                truncated: false,
            },
        )
        .with_summary(summary)
    }
}

// ── Args ────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorktreeListArgs {}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorktreeCreateArgs {
    /// New branch name to create and check out (must not exist yet).
    branch_name: String,
    /// Base commit-ish for the new branch (default: HEAD).
    #[serde(default)]
    base_ref: Option<String>,
    /// Bind THIS session to the new worktree immediately (default false).
    #[serde(default)]
    attach: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorktreeBindArgs {
    /// Worktree id from worktree_list.
    worktree_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorktreeUnbindArgs {}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorktreeRemoveArgs {
    /// Worktree id from worktree_list.
    worktree_id: String,
    /// Remove even when the worktree has uncommitted changes (default false).
    #[serde(default)]
    force: bool,
}

// ── 工具实现 ────────────────────────────────────────────────────────────────

/// 列出源工作区的全部 worktree（管理面完整视图）。
pub struct WorktreeListTool;

impl TypedTool for WorktreeListTool {
    type Args = WorktreeListArgs;
    type Output = WorktreeListToolOutput;

    fn meta(&self) -> ToolMeta {
        ToolMeta::new(
            "worktree_list",
            "List git worktrees of this session's source workspace: managed entries (id, branch, \
             path, owning session, missing-dir state) plus unmanaged linked worktrees. Call this \
             before create/bind/remove to discover worktree ids and see which session owns what.",
            ToolCategory::Read,
            ToolRisk::ReadOnly,
            Duration::from_secs(30),
        )
    }

    fn run(
        &self,
        ctx: &ToolCallContext,
        _args: Self::Args,
    ) -> Result<Self::Output, ToolExecutionError> {
        let source = resolve_source(ctx)?;
        Ok(WorktreeListToolOutput {
            result: list_worktrees(&source)?,
        })
    }
}

/// 建 worktree（attach 语义：会话级绑定，不是模型的选择而是显式动作）。
pub struct WorktreeCreateTool;

impl TypedTool for WorktreeCreateTool {
    type Args = WorktreeCreateArgs;
    type Output = WorktreeCreateToolOutput;

    fn meta(&self) -> ToolMeta {
        ToolMeta::new(
            "worktree_create",
            "Create a git worktree: new branch `branch_name` (must not exist) checked out from \
             `base_ref` (default HEAD) into the source workspace's .qaqh/worktree pool. Prefer \
             worktrees for parallel or high-risk work (long builds, speculative refactors) to \
             keep the main checkout clean. attach=true binds THIS session immediately so file \
             operations run in the worktree from the next turn; attach=false only creates it \
             for later assignment.",
            ToolCategory::Write,
            ToolRisk::Write,
            Duration::from_secs(120),
        )
    }

    fn run(
        &self,
        ctx: &ToolCallContext,
        args: Self::Args,
    ) -> Result<Self::Output, ToolExecutionError> {
        let attach_session = args.attach.then_some(ctx.session_id.as_str());
        if attach_session.is_some() {
            ensure_switchable(ctx)?;
        }
        let source = resolve_source(ctx)?;
        Ok(WorktreeCreateToolOutput {
            result: create_worktree(
                &source,
                &args.branch_name,
                args.base_ref.as_deref(),
                attach_session,
            )?,
        })
    }
}

/// 绑定当前会话到 managed worktree（会话级属性，registry CAS 唯一仲裁点）。
pub struct WorktreeBindTool;

impl TypedTool for WorktreeBindTool {
    type Args = WorktreeBindArgs;
    type Output = WorktreeBindToolOutput;

    fn meta(&self) -> ToolMeta {
        ToolMeta::new(
            "worktree_bind",
            "Bind THIS session to a managed worktree (id from worktree_list). From the next \
             turn, file operations and exec run inside the worktree while .qaqh artifacts stay \
             anchored to the source workspace. A session holds at most one worktree binding - \
             call worktree_unbind first to switch. The switch only lands at turn boundaries; \
             running exec sessions keep their own working directory.",
            ToolCategory::Write,
            ToolRisk::Write,
            Duration::from_secs(30),
        )
    }

    fn run(
        &self,
        ctx: &ToolCallContext,
        args: Self::Args,
    ) -> Result<Self::Output, ToolExecutionError> {
        ensure_switchable(ctx)?;
        let source = resolve_source(ctx)?;
        Ok(WorktreeBindToolOutput {
            binding: bind_worktree(&source, &args.worktree_id, &ctx.session_id)?,
        })
    }
}

/// 释放当前会话的绑定（幂等）。
pub struct WorktreeUnbindTool;

impl TypedTool for WorktreeUnbindTool {
    type Args = WorktreeUnbindArgs;
    type Output = WorktreeUnbindToolOutput;

    fn meta(&self) -> ToolMeta {
        ToolMeta::new(
            "worktree_unbind",
            "Release this session's worktree binding (idempotent: unbound=false when nothing \
             was bound). From the next turn, file operations run in the source workspace again. \
             The worktree keeps its content; use worktree_remove to delete it.",
            ToolCategory::Write,
            ToolRisk::Write,
            Duration::from_secs(30),
        )
    }

    fn run(
        &self,
        ctx: &ToolCallContext,
        _args: Self::Args,
    ) -> Result<Self::Output, ToolExecutionError> {
        ensure_switchable(ctx)?;
        let source = resolve_source(ctx)?;
        Ok(WorktreeUnbindToolOutput {
            result: unbind_worktree(&source, &ctx.session_id)?,
        })
    }
}

/// 删除 managed worktree（owner 校验 + dirty 保护）。
pub struct WorktreeRemoveTool;

impl TypedTool for WorktreeRemoveTool {
    type Args = WorktreeRemoveArgs;
    type Output = WorktreeRemoveToolOutput;

    fn meta(&self) -> ToolMeta {
        ToolMeta::new(
            "worktree_remove",
            "Delete a managed worktree (id from worktree_list). Refused when the worktree has \
             uncommitted changes unless force=true; a session-requested removal requires owning \
             the worktree. A session bound to the removed worktree falls back to the source \
             workspace on its next turn.",
            ToolCategory::Write,
            ToolRisk::Destructive,
            Duration::from_secs(120),
        )
    }

    fn run(
        &self,
        ctx: &ToolCallContext,
        args: Self::Args,
    ) -> Result<Self::Output, ToolExecutionError> {
        ensure_switchable(ctx)?;
        let source = resolve_source(ctx)?;
        Ok(WorktreeRemoveToolOutput {
            result: remove_worktree(&source, &args.worktree_id, args.force, Some(&ctx.session_id))?,
        })
    }
}

/// 注册 worktree 工具面（`SESSION/ROOT_TOOL_REGISTRARS` 消费）。
pub fn register(mgr: &mut ToolManager) {
    mgr.register_typed(WorktreeListTool);
    mgr.register_typed(WorktreeCreateTool);
    mgr.register_typed(WorktreeBindTool);
    mgr.register_typed(WorktreeUnbindTool);
    mgr.register_typed(WorktreeRemoveTool);
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    /// 建一个带单次提交的真实 git 仓库（与 lib.rs 测试同一前提：根 `.gitignore`
    /// 含 `.qaqh/`，模拟 qaqh 托管仓）。
    fn init_repo() -> TempDir {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        std::fs::write(root.join(".gitignore"), ".qaqh/\n").unwrap();
        for args in [
            ["init", "-b", "main"],
            ["config", "user.name", "qaqh-test"],
            ["config", "user.email", "qaqh-test@example.com"],
        ] {
            let status = Command::new("git").current_dir(root).args(args).status().unwrap();
            assert!(status.success());
        }
        std::fs::write(root.join("README.md"), "hello\n").unwrap();
        let status = Command::new("git")
            .current_dir(root)
            .args(["add", "."])
            .status()
            .unwrap();
        assert!(status.success());
        let status = Command::new("git")
            .current_dir(root)
            .args(["commit", "-m", "init"])
            .status()
            .unwrap();
        assert!(status.success());
        dir
    }

    /// 最小宿主：`ses-empty` 映射为 None（覆盖 workspace_unbound 分支），其余
    /// 会话映射到固定路径（切换类工具在宿主解析前就被空闲闸拦截，路径不会被
    /// 实际触碰）。OnceLock 进程级单宿主：两个 ctx 级测试共用这一个安装。
    struct TestHost;

    impl GitxHost for TestHost {
        fn source_workspace(&self, session_id: &str) -> Option<PathBuf> {
            if session_id == "ses-empty" {
                None
            } else {
                Some(PathBuf::from("D:/qaqh-gitx-test/nonexistent-root"))
            }
        }
    }

    fn ensure_test_host() {
        if host().is_none() {
            install_host(Arc::new(TestHost));
        }
    }

    fn detail_of(error: ToolExecutionError) -> String {
        match error {
            ToolExecutionError::Recoverable(inner) => inner.detail,
            ToolExecutionError::Fatal(_) => "fatal".to_string(),
        }
    }

    fn model_ctx(session_id: &str, cancellation: qaqh_workspace::tool_api::CancellationToken) -> ToolCallContext {
        ToolCallContext {
            call_id: "call_1".to_owned(),
            session_id: session_id.to_owned(),
            workspace_root: PathBuf::from("D:/qaqh-gitx-test/nonexistent-root"),
            permission_level: qaqh_workspace::PermissionLevel::WorkspaceWrite,
            sandbox: qaqh_workspace::tool_api::SandboxMode::Main,
            sandbox_spec: qaqh_workspace::tool_api::SandboxSpec::disabled(),
            exec_default_shell: None,
            exec_max_output_chars: None,
            timeout: Duration::from_secs(30),
            cancellation,
            progress: None,
            exec_progress: None,
            exec_host: None,
            official_web: None,
            source: qaqh_workspace::tool_api::ToolCallSource::Model,
        }
    }

    #[test]
    fn registry_exposes_the_five_worktree_tools() {
        let mut mgr = ToolManager::new();
        register(&mut mgr);
        let defs = mgr.all_defs();
        let names: Vec<String> = defs.iter().map(|def| def.function.name.clone()).collect();
        for name in [
            "worktree_list",
            "worktree_create",
            "worktree_bind",
            "worktree_unbind",
            "worktree_remove",
        ] {
            assert!(names.contains(&name.to_string()), "{name} must register");
            assert!(
                mgr.lookup(name).is_some(),
                "{name} must be callable, not just advertised"
            );
        }
        // schema 形态闸：输入必须是 object 且无组合键泄漏。
        for def in &defs {
            let schema = &def.function.parameters;
            assert_eq!(schema["type"], "object", "{}: object schema", def.function.name);
            let serialized = schema.to_string();
            for key in ["oneOf", "anyOf", "allOf", "$ref", "$defs"] {
                assert!(
                    !serialized.contains(key),
                    "{}: composition key {key} leaked",
                    def.function.name
                );
            }
        }
    }

    #[test]
    fn create_list_bind_conflict_unbind_round_trip() {
        let repo = init_repo();
        let source = repo.path();

        // create（不 attach）→ list 可见且空闲。
        let created = create_worktree(source, "feature/auth", None, None).unwrap();
        assert!(created.binding.is_none());
        let listed = list_worktrees(source).unwrap();
        assert_eq!(listed.worktrees.len(), 1);
        assert!(listed.worktrees[0].owner_session_id.is_none());
        assert!(listed.unmanaged_paths.is_empty());

        // bind → 绑定视图；重复 bind 显式报错（先 unbind）。
        let binding =
            bind_worktree(source, &created.worktree.id, "ses-a").unwrap();
        assert_eq!(binding.branch, "feature/auth");
        assert_eq!(binding.owner_session_id, "ses-a");
        assert!(matches!(
            bind_worktree(source, &created.worktree.id, "ses-b"),
            Err(ToolExecutionError::Recoverable(error)) if error.detail.contains("already bound")
        ));
        // ses-a 已绑定 → 对其他 worktree 的 bind 同样拒绝。
        let second = create_worktree(source, "feature/second", None, None).unwrap();
        assert!(matches!(
            bind_worktree(source, &second.worktree.id, "ses-a"),
            Err(ToolExecutionError::Recoverable(error)) if error.detail.contains("unbind first")
        ));

        // unbind 幂等；无绑定会话 unbound=false。
        let unbound = unbind_worktree(source, "ses-a").unwrap();
        assert!(unbound.unbound);
        let unbound_again = unbind_worktree(source, "ses-a").unwrap();
        assert!(!unbound_again.unbound);
    }

    #[test]
    fn create_attach_is_rejected_while_session_is_bound() {
        let repo = init_repo();
        let source = repo.path();

        let first = create_worktree(source, "feature/first", None, Some("ses-1")).unwrap();
        assert!(first.binding.is_some(), "attach=true 必须回传绑定视图");
        // 会话已绑定 → attach=true 的第二次 create 必须拒绝（双绑漏洞的工具层封口）。
        assert!(matches!(
            create_worktree(source, "feature/second", None, Some("ses-1")),
            Err(ToolExecutionError::Recoverable(error)) if error.detail.contains("already bound")
        ));
        let listed = list_worktrees(source).unwrap();
        assert_eq!(listed.worktrees.len(), 1, "拒绝的 create 不得留下半成品");
    }

    #[test]
    fn remove_enforces_owner_and_dirty_semantics() {
        let repo = init_repo();
        let source = repo.path();

        let created = create_worktree(source, "feature/temp", None, Some("ses-1")).unwrap();
        // 非 owner 删除被拒。
        assert!(matches!(
            remove_worktree(source, &created.worktree.id, false, Some("ses-2")),
            Err(ToolExecutionError::Recoverable(error)) if error.detail.contains("does not own")
        ));
        // dirty 未 force 被拒。
        std::fs::write(
            Path::new(&created.worktree.path).join("wip.txt"),
            "wip\n",
        )
        .unwrap();
        assert!(matches!(
            remove_worktree(source, &created.worktree.id, false, Some("ses-1")),
            Err(ToolExecutionError::Recoverable(error)) if error.detail.contains("uncommitted")
        ));
        // force + owner → 删除成功。
        let removed = remove_worktree(source, &created.worktree.id, true, Some("ses-1")).unwrap();
        assert!(removed.removed);
        assert!(!Path::new(&created.worktree.path).exists());
    }

    #[test]
    fn tools_fail_closed_without_host_or_workspace() {
        ensure_test_host();
        let ctx = model_ctx("ses-empty", qaqh_workspace::tool_api::CancellationToken::new());
        let error = WorktreeListTool
            .run(&ctx, WorktreeListArgs {})
            .expect_err("empty host mapping must fail");
        assert!(detail_of(error).contains("no bound workspace"));
    }

    #[test]
    fn switch_is_rejected_when_turn_is_cancelled() {
        ensure_test_host();
        let token = qaqh_workspace::tool_api::CancellationToken::new();
        token.cancel();
        let ctx = model_ctx("ses-x", token);
        // 取消中的回合：bind/unbind/create(attach)/remove 全部拒绝（空闲闸在宿主解析之前）。
        for (name, error) in [
            (
                "bind",
                WorktreeBindTool
                    .run(&ctx, WorktreeBindArgs { worktree_id: "w".to_string() })
                    .expect_err("cancelled turn must reject bind"),
            ),
            (
                "unbind",
                WorktreeUnbindTool
                    .run(&ctx, WorktreeUnbindArgs {})
                    .expect_err("cancelled turn must reject unbind"),
            ),
            (
                "create",
                WorktreeCreateTool
                    .run(&ctx, WorktreeCreateArgs {
                        branch_name: "feature/x".to_string(),
                        base_ref: None,
                        attach: true,
                    })
                    .expect_err("cancelled turn must reject attach"),
            ),
            (
                "remove",
                WorktreeRemoveTool
                    .run(&ctx, WorktreeRemoveArgs { worktree_id: "w".to_string(), force: false })
                    .expect_err("cancelled turn must reject remove"),
            ),
        ] {
            let detail = detail_of(error);
            assert!(detail.contains("cancelled"), "{name}: {detail}");
        }
    }
}
