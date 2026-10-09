//! Permission admission and single-use authorization proofs.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::tool_api::{
    CancellationToken, SandboxMode, SandboxSpec, ToolCallContext, ToolCallSource,
};

// 子代理沙箱标志（per-actor）：`run_actor` subagent 分支在 actor 线程上设置。
//
// 沙箱语义（方案 B）：子代理**没有用户审批通道**——
// - 文件级操作（Read/Write）且全部路径在 workspace 内 → 自动批准
//   （等价 Level 3 的 workspace 内语义，子代理可以正常干活）；
// - 其余（Exec / Net / 跨 workspace 路径）→ **自动拒绝**（返回
//   `PermissionDenied` 失败，不产生 `ToolPermissionRequested` 弹窗事件，
//   不挂起回合）。
//
// 同时解决"卡死"（L1-L3 下子代理审批无人响应）与"越狱"（子代理
// 跨 workspace / exec / 网络访问）两个问题。
//
// thread-local：主代理与子代理并发时，子代理的沙箱不会误伤主代理的
// 工具准入（`admit` 只在同一 actor 线程上执行）。
thread_local! {
    static SUBAGENT_SANDBOX: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// 启用/关闭子代理沙箱（仅 actor 线程内调用）。
pub fn set_subagent_sandbox(on: bool) {
    SUBAGENT_SANDBOX.with(|slot| slot.set(on));
}

/// 当前线程是否处于子代理沙箱模式。
pub fn is_subagent_sandbox() -> bool {
    SUBAGENT_SANDBOX.with(|slot| slot.get())
}

/// 沙箱内的「跨区」判定。
///
/// 参数路径在 `extract_target_paths` 里已经解析成绝对路径
/// （`permission.rs` 末尾的 `map(resolve_target_path)`，基准是当前会话工作区），
/// 所以这里只剩一件事要处理：**会话没有绑定工作区**（`""` / `"."`）时不做跨区
/// 判定——没有工作区就没有「区外」。否则子代理连自己会话里的普通文件都读不了，
/// 实测报错正是 `reads outside the workspace are host-only`。
///
/// 放宽只覆盖文件读写：调用方仅在文件操作分支使用本函数；exec / 网络 / MCP 的
/// 拒绝不依赖工作区，不受影响。
fn sandbox_within_workspace(
    resources: &[PathBuf],
    workspace_root: &Path,
    workspace_unset: bool,
) -> bool {
    workspace_unset || crate::permission::all_within_workspace(resources, workspace_root)
}

/// Identity of a single tool invocation destined for a handler.
#[derive(Debug, Clone)]
pub struct ToolInvocation {
    pub session_id: String,
    pub call_id: String,
    pub tool_name: String,
    pub args: serde_json::Value,
    /// 能力类别（handler 声明）：权限决策的单一事实源，取代名字表。
    pub category: crate::permission::ToolCategory,
}

/// 授权凭证的签发路径（审计用）：这次调用**为什么被放行**。
///
/// 凭证本身不区分签发路径（单次性由类型保证），但审计账本必须能回答
/// 「自动放行还是用户批准」——决策链因此显式随凭证传递。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantKind {
    /// 策略自动放行（含 MCP D5 快路径与 skip-permissions bypass）。
    Auto,
    /// 用户在审批通道显式批准（一次性凭证）。
    UserApproved,
    /// 子代理沙箱内自动批准（工作区内文件操作）。
    SandboxAuto,
    /// 沙箱文件写强制成立时，只读分类器批准的 exec（ADR 2026-10-09 决策 1/2）。
    SandboxClassified,
}

impl GrantKind {
    /// 审计词汇表（v2 `decision.outcome`）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::UserApproved => "user_approved",
            Self::SandboxAuto => "sandbox_auto",
            Self::SandboxClassified => "sandbox_classified",
        }
    }
}

/// Authorization proof required to dispatch a handler.
///
/// Fields and construction stay private to this crate. External callers can
/// only obtain a proof through [`admit`] or [`PermissionChallenge::approve`].
pub struct AuthorizedToolCall {
    invocation: ToolInvocation,
    resources: Vec<PathBuf>,
    workspace_root: PathBuf,
    grant: GrantKind,
    _sealed: (),
}

impl AuthorizedToolCall {
    pub(crate) fn new(
        invocation: ToolInvocation,
        resources: Vec<PathBuf>,
        workspace_root: PathBuf,
        grant: GrantKind,
    ) -> Self {
        Self {
            invocation,
            resources,
            workspace_root,
            grant,
            _sealed: (),
        }
    }

    /// 凭证签发路径（审计用）。
    pub fn grant(&self) -> GrantKind {
        self.grant
    }

    pub fn session_id(&self) -> &str {
        &self.invocation.session_id
    }

    pub fn call_id(&self) -> &str {
        &self.invocation.call_id
    }

    pub fn tool_name(&self) -> &str {
        &self.invocation.tool_name
    }

    pub fn args(&self) -> &serde_json::Value {
        &self.invocation.args
    }

    pub fn resources(&self) -> &[PathBuf] {
        &self.resources
    }

    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    pub(crate) fn into_parts(self) -> (ToolInvocation, Vec<PathBuf>, PathBuf, GrantKind) {
        (
            self.invocation,
            self.resources,
            self.workspace_root,
            self.grant,
        )
    }
}

/// Result of the admission gate.
pub enum Admission {
    Authorized(AuthorizedToolCall),
    ApprovalRequired(PermissionChallenge),
    Denied(String),
}

/// Immutable snapshot of a call that requires user approval.
///
/// Approval consumes the challenge, making the grant single-use by type.
pub struct PermissionChallenge {
    context: Box<ToolCallContext>,
    tool_name: String,
    normalized_args: serde_json::Value,
    resources: Vec<PathBuf>,
    reason: String,
    category: crate::permission::ToolCategory,
    risk: crate::permission::PermissionRisk,
    consequence: String,
    created_at: Instant,
    _sealed: (),
}

impl PermissionChallenge {
    fn new(
        invocation: ToolInvocation,
        context: ToolCallContext,
        reason: String,
        resources: Vec<PathBuf>,
        category: crate::permission::ToolCategory,
        risk: crate::permission::PermissionRisk,
        consequence: String,
    ) -> Self {
        Self {
            context: Box::new(context),
            tool_name: invocation.tool_name,
            normalized_args: invocation.args,
            resources,
            reason,
            category,
            risk,
            consequence,
            created_at: Instant::now(),
            _sealed: (),
        }
    }

    pub fn session_id(&self) -> &str {
        &self.context.session_id
    }

    pub fn call_id(&self) -> &str {
        &self.context.call_id
    }

    pub fn tool_name(&self) -> &str {
        &self.tool_name
    }

    pub fn normalized_args(&self) -> &serde_json::Value {
        &self.normalized_args
    }

    /// Minimal action description that remote approval UIs can render without
    /// receiving full normalized args (which may contain secrets in `env`).
    pub fn action_summary(&self) -> Option<String> {
        crate::permission::summarize_permission_action(&self.tool_name, &self.normalized_args)
    }

    pub fn resources(&self) -> &[PathBuf] {
        &self.resources
    }

    pub fn workspace_root(&self) -> &Path {
        &self.context.workspace_root
    }

    /// Explicit runtime context captured when the challenge was created.
    pub fn context(&self) -> &ToolCallContext {
        &self.context
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }

    pub fn category(&self) -> &crate::permission::ToolCategory {
        &self.category
    }

    pub fn risk(&self) -> crate::permission::PermissionRisk {
        self.risk
    }

    pub fn consequence(&self) -> &str {
        &self.consequence
    }

    pub fn is_expired(&self, ttl: Duration) -> bool {
        self.created_at.elapsed() > ttl
    }

    pub fn approve(self, approved: bool) -> Result<AuthorizedToolCall, ApprovalError> {
        self.approve_with_ttl(approved, Duration::from_secs(120))
    }

    pub(crate) fn approve_with_ttl(
        self,
        approved: bool,
        ttl: Duration,
    ) -> Result<AuthorizedToolCall, ApprovalError> {
        if !approved {
            return Err(ApprovalError::Rejected);
        }
        if self.is_expired(ttl) {
            return Err(ApprovalError::Expired);
        }
        let context = *self.context;
        let invocation = ToolInvocation {
            session_id: context.session_id.clone(),
            call_id: context.call_id.clone(),
            tool_name: self.tool_name,
            args: self.normalized_args,
            category: self.category,
        };
        Ok(AuthorizedToolCall::new(
            invocation,
            self.resources,
            context.workspace_root,
            GrantKind::UserApproved,
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalError {
    Rejected,
    Expired,
    MissingOrReplayed,
}

fn legacy_tool_call_context(
    invocation: &ToolInvocation,
    permission_level: u8,
    workspace_root: &Path,
) -> ToolCallContext {
    let cancellation = CancellationToken::new();
    if crate::is_cancel() {
        cancellation.cancel();
    }
    ToolCallContext {
        call_id: invocation.call_id.clone(),
        session_id: invocation.session_id.clone(),
        workspace_root: workspace_root.to_path_buf(),
        mode: match crate::runtime::current_mode() {
            1 => crate::tool_api::AgentMode::Plan,
            _ => crate::tool_api::AgentMode::Code,
        },
        permission_level: crate::permission::PermissionLevel::from_u8(permission_level),
        sandbox: if is_subagent_sandbox() {
            SandboxMode::Subagent
        } else {
            SandboxMode::Main
        },
        sandbox_spec: SandboxSpec::workspace_write(workspace_root.to_path_buf()),
        exec_default_shell: None,
        timeout: Duration::ZERO,
        cancellation,
        progress: None,
        source: ToolCallSource::Model,
    }
}

/// Evaluate permission policy and bind the resulting proof to normalized resources.
pub fn admit(
    invocation: ToolInvocation,
    permission_level: u8,
    workspace_root: &Path,
    trusted_dirs: &HashSet<PathBuf>,
) -> Admission {
    let context = legacy_tool_call_context(&invocation, permission_level, workspace_root);
    admit_with_context(invocation, &context, trusted_dirs)
}

/// Evaluate permission policy using an explicit runtime context.
pub fn admit_with_context(
    invocation: ToolInvocation,
    context: &ToolCallContext,
    trusted_dirs: &HashSet<PathBuf>,
) -> Admission {
    // ── MCP 动态工具（D5 + S3，设计 §5.5）──
    //
    // D5（owner 2026-09-07 拍板）：MCP 调用默认放行，不经 PermissionChallenge
    // ——配置声明即信任（D4）；category 照常填写（S3：stdio=Exec / http=Net）
    // 供审计展示。
    //
    // T-8-1（安全审查 P1-1 / O-4）收紧：**Exec/Net 类别不再无条件放行**——
    // read-only / workspace-write 落到下面的 needs_permission 决策并进入审批；
    // skip-permissions 是显式 bypass，继续走 D5 快路径。只读类（Read，如
    // `mcp` resources 聚合）在任何档位都保留 D5 快路径，避免误伤。
    //
    // 子代理沙箱优先于 D5：S3 要求 MCP 工具在子代理上下文一律拒绝
    // （防越狱）——原“沙箱零代码”依赖 needs_permission→AskUser 路径，
    // 而 D5 快路径绕过 needs_permission，故在此显式拦截（仅针对 mcp__ 前缀，
    // 不影响内置工具的沙箱语义）。
    let level = context.permission_level;
    let sandboxed = matches!(context.sandbox, SandboxMode::Subagent);
    if invocation
        .tool_name
        .starts_with(crate::manager::MCP_DYNAMIC_PREFIX)
    {
        if sandboxed {
            return Admission::Denied(format!(
                "subagent sandbox denied '{}': MCP tools are host-only",
                invocation.tool_name
            ));
        }
        let d5_bypass = !matches!(
            invocation.category,
            crate::permission::ToolCategory::Exec | crate::permission::ToolCategory::Net
        ) || level == crate::permission::PermissionLevel::SkipPermissions;
        if d5_bypass {
            let mut resources = crate::permission::extract_target_paths_in(
                &invocation.tool_name,
                &invocation.args,
                context.workspace_root.as_path(),
            );
            resources.sort();
            resources.dedup();
            return Admission::Authorized(AuthorizedToolCall::new(
                invocation,
                resources,
                crate::permission::resolve_target_path(context.workspace_root.clone()),
                GrantKind::Auto,
            ));
        }
    }

    // 会话未绑定工作区（`""` / `"."` ⇒ 无边界）时，沙箱不做跨区判定——
    // 没有工作区就没有「区外」。见 `sandbox_within_workspace`。
    let workspace_unset = matches!(
        context
            .workspace_root
            .components()
            .collect::<Vec<_>>()
            .as_slice(),
        [] | [std::path::Component::CurDir]
    );
    let workspace_root = crate::permission::resolve_target_path(context.workspace_root.clone());
    // ADR 2026-10-09 决策 1/2/5：沙箱文件写强制成立时 exec 走分类器判据——
    // WorkspaceWrite 档仅只读分类自动放行；SandboxRun 档 exec 全部自动
    // （ReadOnly/Unclassified），Risky(deny 形态/网络) 落回常规审批。
    // 分类器只决定摩擦，不是安全边界——误判的兜底是 DACL 与 deny 模式。
    // 会话历史/凭据路径不可借 exec 自动放行外泄，命中即回退常规审批。
    // 审计 F1/P0（analysis-windows-appcontainer-projfs-2026-10-09）：自动放行
    // 不得信任平台级 detect——必须该次 launch 计划确实解析到 sbx 后端
    // （显式 Token/Redirect，或 Auto 已晋升 Redirect）；Auto 未晋升 = 不强制
    // = 不自动放行（fail-safe，与 direct.rs 的实际分派条件一致）。
    let exec_backend_resolved = matches!(
        qaqh_sandbox::sbx_map::resolve_windows_backend(&context.sandbox_spec)
            .ok()
            .flatten(),
        Some(
            qaqh_sandbox::SandboxBackend::WindowsToken
                | qaqh_sandbox::SandboxBackend::WindowsRedirect
        )
    );
    let exec_class = if !sandboxed
        && invocation.tool_name == "exec"
        && context.sandbox_spec.enabled
        && exec_backend_resolved
        && qaqh_sandbox::SandboxCapabilities::detect().filesystem_write_isolation
    {
        Some(crate::permission::classify_exec_args(&invocation.args))
    } else {
        None
    };
    let exec_classified_auto = matches!(
        (&level, &exec_class),
        (
            crate::permission::PermissionLevel::WorkspaceWrite,
            Some(crate::permission::ExecCommandClass::ReadOnly)
        ) | (
            crate::permission::PermissionLevel::SandboxRun,
            Some(crate::permission::ExecCommandClass::ReadOnly),
        ) | (
            crate::permission::PermissionLevel::SandboxRun,
            Some(crate::permission::ExecCommandClass::Unclassified),
        )
    );
    let exec_auto =
        exec_classified_auto && !exec_hits_sensitive_session_paths(&invocation, &workspace_root);
    let decision = if exec_auto {
        crate::permission::PermissionDecision::AutoApprove
    } else {
        crate::permission::needs_permission(
            level,
            &invocation.tool_name,
            &invocation.args,
            &workspace_root,
            trusted_dirs,
            invocation.category,
        )
    };
    match decision {
        crate::permission::PermissionDecision::AutoApprove => {
            let mut resources = crate::permission::extract_target_paths_in(
                &invocation.tool_name,
                &invocation.args,
                context.workspace_root.as_path(),
            );
            resources.sort();
            resources.dedup();
            // S3 兜底（2026-10-05 读自由规则后必读）：needs_permission 对 Read
            // 已无条件放行，子代理沙箱的「跨 workspace 拒读」姿态改在此兑现——
            // 沙箱上下文没有审批通道，跨 workspace 读必须保持自动拒绝（防越狱）。
            if sandboxed
                && invocation.category == crate::permission::ToolCategory::Read
                && !sandbox_within_workspace(&resources, &workspace_root, workspace_unset)
            {
                return Admission::Denied(format!(
                    "subagent sandbox denied '{}': reads outside the workspace are host-only",
                    invocation.tool_name
                ));
            }
            let grant = if exec_auto {
                GrantKind::SandboxClassified
            } else {
                GrantKind::Auto
            };
            Admission::Authorized(AuthorizedToolCall::new(
                invocation,
                resources,
                workspace_root,
                grant,
            ))
        }
        crate::permission::PermissionDecision::AskUser {
            reason,
            paths,
            category,
            risk,
            consequence,
        } => {
            if sandboxed {
                // 子代理沙箱：无审批通道，不产生弹窗事件。
                let file_ops = matches!(
                    category,
                    crate::permission::ToolCategory::Read | crate::permission::ToolCategory::Write
                );
                // 敏感会话文件（历史 / 凭据）必须保持拒绝：沙箱没有审批通道，
                // 被「未设置工作区」放宽顺带放行就等于把数据外泄面直接打开。
                let sensitive = paths
                    .iter()
                    .any(|path| crate::permission::is_sensitive_session_path(path));
                if file_ops
                    && !sensitive
                    && sandbox_within_workspace(&paths, &workspace_root, workspace_unset)
                {
                    // workspace 内文件操作：自动批准（等价 Level 3 语义）。
                    let mut resources = crate::permission::extract_target_paths_in(
                        &invocation.tool_name,
                        &invocation.args,
                        context.workspace_root.as_path(),
                    );
                    resources.sort();
                    resources.dedup();
                    Admission::Authorized(AuthorizedToolCall::new(
                        invocation,
                        resources,
                        workspace_root,
                        GrantKind::SandboxAuto,
                    ))
                } else {
                    // Exec / Net / 跨 workspace：自动拒绝，防止越狱。
                    Admission::Denied(format!(
                        "subagent sandbox denied '{}': {reason}",
                        invocation.tool_name
                    ))
                }
            } else {
                let challenge_context = ToolCallContext {
                    workspace_root: workspace_root.clone(),
                    sandbox_spec: SandboxSpec::workspace_write(workspace_root.clone()),
                    ..context.clone()
                };
                Admission::ApprovalRequired(PermissionChallenge::new(
                    invocation,
                    challenge_context,
                    reason,
                    paths,
                    category,
                    risk,
                    consequence,
                ))
            }
        }
    }
}

// ── Process-global trusted folders + single admission facade (PR-1-1 / B1) ──

static GLOBAL_TRUSTED: std::sync::Mutex<Option<crate::permission::TrustedFolderSet>> =
    std::sync::Mutex::new(None);

fn with_global_trusted<R>(f: impl FnOnce(&mut crate::permission::TrustedFolderSet) -> R) -> R {
    let mut guard = GLOBAL_TRUSTED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let set = guard.get_or_insert_with(|| {
        // 语义与旧 `ToolEngine::new` 的 `TrustedFolderSet::load("")` 逐字一致
        // （进程级共享信任文件）；惰性初始化，装配方零调用负担。
        crate::permission::TrustedFolderSet::load("")
    });
    f(set)
}

/// Runtime trust write (post-approval "remember this folder"). The loop no
/// longer owns a `TrustedFolderSet`; it calls through here.
pub fn trust_folder(dir: &Path) {
    with_global_trusted(|set| set.trust(dir));
}

fn trusted_snapshot() -> HashSet<PathBuf> {
    with_global_trusted(|set| set.set().clone())
}

/// exec 自动放行档的会话敏感路径门禁（审计 2026-10-09 复核）。
///
/// 命令词元与 `cwd` **一律按该次命令的实际落点解析**：给了 `cwd` 就锚 `cwd`，
/// 否则锚会话工作区。旧实现只看含 `/`/`\` 的词元、且按工作区解析，于是
/// `cwd=<sessions 目录>` + `cat messages.jsonl` 这种形态既不解析 cwd、词元也过不了
/// 分隔符过滤——会话历史可在 WorkspaceWrite/SandboxRun 档被静默读出。口径对齐
/// Codex 不变量「`:workspace_roots` 的落点依据是该命令的 cwd，不是 server cwd」。
///
/// 命中只回退到常规审批（多弹一次窗），不影响命令本身能否执行。
fn exec_hits_sensitive_session_paths(invocation: &ToolInvocation, workspace_root: &Path) -> bool {
    let base = invocation
        .args
        .get("cwd")
        .and_then(|value| value.as_str())
        .filter(|cwd| !cwd.trim().is_empty())
        .map(|cwd| crate::permission::resolve_target_path_in(PathBuf::from(cwd), workspace_root))
        .unwrap_or_else(|| workspace_root.to_path_buf());
    if crate::permission::is_sensitive_session_path(&base) {
        return true;
    }
    crate::permission::exec_argument_tokens(&invocation.args)
        .into_iter()
        .map(|token| crate::permission::resolve_target_path_in(PathBuf::from(token), &base))
        .any(|path| crate::permission::is_sensitive_session_path(&path))
}

/// Resolve the effective workspace root the way the loop's engines did
/// (empty/"." falls back to the process cwd).
///
/// BUG-2026-09-12-05：读 TLS 优先的 `current_workspace()` 而非进程全局——
/// admit 发生在 actor 线程（或恢复了 actor scope 的线程），进程全局在多
/// actor daemon 中恒空，直接读全局会把授权基准锚到 daemon 进程 cwd，与
/// 执行线程按会话工作区解析的资源错位（RESOURCE_MISMATCH /
/// WORKSPACE_MISMATCH 拒工具通道）。
fn effective_workspace_root() -> PathBuf {
    let ws = crate::current_workspace();
    if ws.is_empty() || ws == "." {
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
    } else {
        std::path::PathBuf::from(ws)
    }
}

/// Single admission facade (PR-1-1 / B1): the loop passes (session, call,
/// tool, args) and nothing else. Category comes from the handler registry
/// (single source of truth, conservative `Write` fallback), the permission
/// level and workspace root resolve inside this crate, and the trusted
/// folder set is the process-global singleton. The loop performs zero disk
/// reads and owns no authorization state.
pub fn authorize_call(
    session_id: &str,
    call_id: &str,
    tool_name: &str,
    args: &serde_json::Value,
    permission_level: u8,
) -> Admission {
    let invocation = ToolInvocation {
        session_id: session_id.to_string(),
        call_id: call_id.to_string(),
        tool_name: tool_name.to_string(),
        args: args.clone(),
        category: crate::runtime::lookup_category(tool_name)
            .unwrap_or(crate::permission::ToolCategory::Write),
    };
    let ws_root = effective_workspace_root();
    let context = legacy_tool_call_context(&invocation, permission_level, &ws_root);
    admit_with_context(invocation, &context, &trusted_snapshot())
}

/// Single admission facade using the caller's explicit tool context.
///
/// `ToolCallContext` is the source of truth for session/workspace/permission/
/// mode/sandbox. The context's call-specific fields (`timeout`, `progress`) are
/// not consumed by admission; execution resolves them before invoking the
/// handler.
pub fn authorize_call_with_context(
    tool_name: &str,
    args: &serde_json::Value,
    context: &ToolCallContext,
) -> Admission {
    let invocation = ToolInvocation {
        session_id: context.session_id.clone(),
        call_id: context.call_id.clone(),
        tool_name: tool_name.to_string(),
        args: args.clone(),
        category: crate::runtime::lookup_category(tool_name)
            .unwrap_or(crate::permission::ToolCategory::Write),
    };
    admit_with_context(invocation, context, &trusted_snapshot())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::PermissionRisk;

    #[test]
    fn approval_challenge_preserves_backend_risk_and_consequence() {
        // 沙箱标志是 thread-local（`SUBAGENT_SANDBOX`），默认关闭，故本测试可
        // 断言 "level 1 write 必须要求审批"（沙箱下写自动批准）；串行化与其他
        // 运行时状态测试共用 `TEST_RUNTIME_SERIAL`。
        let _serial = crate::TEST_RUNTIME_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let workspace = std::env::temp_dir().join("qaqh-authorization-risk");
        let invocation = ToolInvocation {
            session_id: "seed-a".into(),
            call_id: "call-a".into(),
            tool_name: "write".into(),
            args: serde_json::json!({ "path": workspace.join("src/lib.rs") }),
            category: crate::permission::ToolCategory::Write,
        };

        let Admission::ApprovalRequired(challenge) =
            admit(invocation, 1, &workspace, &HashSet::new())
        else {
            panic!("level 1 write must require approval");
        };

        assert_eq!(challenge.risk(), PermissionRisk::Medium);
        assert_eq!(
            challenge.consequence(),
            "Changes files inside the current workspace."
        );
    }

    #[test]
    fn exec_read_only_auto_approves_only_under_write_enforced_sandbox() {
        let _serial = crate::TEST_RUNTIME_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let workspace = std::env::temp_dir().join("qaqh-authorization-exec-class");
        let invocation = |command: &str, shell: Option<&str>| ToolInvocation {
            session_id: "seed-a".into(),
            call_id: "call-exec".into(),
            tool_name: "exec".into(),
            args: serde_json::json!({
                "command": command,
                "shell": shell,
            }),
            category: crate::permission::ToolCategory::Exec,
        };
        let write_enforced = qaqh_sandbox::SandboxCapabilities::detect().filesystem_write_isolation;
        let ctx_for = |invocation: ToolInvocation, backend: qaqh_sandbox::SandboxBackend| {
            let mut context = legacy_tool_call_context(
                &invocation,
                crate::permission::PermissionLevel::WorkspaceWrite as u8,
                &workspace,
            );
            context.sandbox_spec.backend = backend;
            context
        };
        let asks = |admission: Admission| matches!(admission, Admission::ApprovalRequired(_));

        // WorkspaceWrite 档：只读分类命中 + 后端确实解析到 sbx → 凭证带
        // SandboxClassified；平台无写强制时保持常规审批（fail-closed）。
        let inv = invocation("rg foo src", Some("bash"));
        let admission = admit_with_context(
            inv.clone(),
            &ctx_for(inv, qaqh_sandbox::SandboxBackend::WindowsToken),
            &HashSet::new(),
        );
        if write_enforced {
            let Admission::Authorized(proof) = admission else {
                panic!("classified read-only exec must auto-approve under workspace-write");
            };
            assert_eq!(proof.grant(), GrantKind::SandboxClassified);
        } else {
            assert!(asks(admission));
        }

        // 审计 F1/P0 回归锁：Auto 未晋升 = 实际 plain spawn = 不得自动放行
        let inv = invocation("rg foo src", Some("bash"));
        let admission = admit_with_context(
            inv.clone(),
            &ctx_for(inv, qaqh_sandbox::SandboxBackend::Auto),
            &HashSet::new(),
        );
        assert!(asks(admission));

        // deny 形态（递归删除）任何平台都不得自动放行
        let inv = invocation("rm -rf build", Some("bash"));
        let admission = admit_with_context(
            inv.clone(),
            &ctx_for(inv, qaqh_sandbox::SandboxBackend::WindowsToken),
            &HashSet::new(),
        );
        assert!(asks(admission));

        // pwsh 侧不做文法判定：Unclassified 保持常规审批
        let inv = invocation("Get-ChildItem src", Some("pwsh"));
        let admission = admit_with_context(
            inv.clone(),
            &ctx_for(inv, qaqh_sandbox::SandboxBackend::WindowsToken),
            &HashSet::new(),
        );
        assert!(asks(admission));

        // 命令文本引用会话历史文件（真实平台 sessions 目录）：不自动放行
        let session_log = qaqh_types::platform::sessions_dir().join("messages.jsonl");
        let inv = invocation(&format!("cat {}", session_log.display()), Some("bash"));
        let admission = admit_with_context(
            inv.clone(),
            &ctx_for(inv, qaqh_sandbox::SandboxBackend::WindowsToken),
            &HashSet::new(),
        );
        assert!(asks(admission));
    }

    /// 纯函数锁（与平台能力无关）：门禁必须按**该次命令的 cwd** 解析词元。
    /// 旧实现只看含分隔符的词元、且一律按会话工作区解析，所以
    /// `cat messages.jsonl` + `cwd=<sessions dir>` 两头都不命中。
    #[test]
    fn exec_sensitive_guard_resolves_tokens_against_command_cwd() {
        let workspace = std::env::temp_dir().join("qaqh-exec-sensitive-base");
        let sessions_dir = qaqh_types::platform::sessions_dir();
        let invocation = |command: &str, cwd: Option<String>| ToolInvocation {
            session_id: "seed-a".into(),
            call_id: "call-sensitive".into(),
            tool_name: "exec".into(),
            args: serde_json::json!({ "command": command, "shell": "bash", "cwd": cwd }),
            category: crate::permission::ToolCategory::Exec,
        };
        let sessions_text = sessions_dir.to_string_lossy().into_owned();

        assert!(
            exec_hits_sensitive_session_paths(
                &invocation("cat messages.jsonl", Some(sessions_text.clone())),
                &workspace
            ),
            "cwd 落在会话数据根时，相对文件名必须命中敏感门禁"
        );
        assert!(
            exec_hits_sensitive_session_paths(
                &invocation(&format!("cat {sessions_text}/messages.jsonl"), None),
                &workspace
            ),
            "绝对路径词元照常命中"
        );
        assert!(
            !exec_hits_sensitive_session_paths(
                &invocation("cat messages.jsonl", None),
                &workspace
            ),
            "普通工作区内的相对读不得误伤"
        );
    }

    /// 回归锁（审计 2026-10-09 复核）：exec 的 `cwd` 落在会话数据根内时，
    /// 命令里用**相对文件名**读历史也不得自动放行。旧门禁只扫含分隔符的词元
    /// 并按工作区解析，`cwd=<sessions dir>` + `cat messages.jsonl` 两头都漏。
    #[test]
    fn exec_cwd_in_session_dir_blocks_auto_approve_even_for_relative_reads() {
        let _serial = crate::TEST_RUNTIME_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let workspace = std::env::temp_dir().join("qaqh-authorization-exec-cwd");
        let sessions_dir = qaqh_types::platform::sessions_dir();
        let invocation = ToolInvocation {
            session_id: "seed-a".into(),
            call_id: "call-exec-cwd".into(),
            tool_name: "exec".into(),
            args: serde_json::json!({
                "command": "cat messages.jsonl",
                "shell": "bash",
                "cwd": sessions_dir.to_string_lossy(),
            }),
            category: crate::permission::ToolCategory::Exec,
        };
        let asks = |admission: Admission| matches!(admission, Admission::ApprovalRequired(_));
        for level in [
            crate::permission::PermissionLevel::WorkspaceWrite as u8,
            crate::permission::PermissionLevel::SandboxRun as u8,
        ] {
            let mut context = legacy_tool_call_context(&invocation, level, &workspace);
            context.sandbox_spec.backend = qaqh_sandbox::SandboxBackend::WindowsToken;
            let admission = admit_with_context(invocation.clone(), &context, &HashSet::new());
            assert!(
                asks(admission),
                "level {level}: cwd 指向会话数据根时必须弹审批"
            );
        }
    }

    /// 对照面：同一只读命令落在普通工作区，门禁不得误伤（有写强制时仍
    /// 走自动放行；无写强制的平台保持常规审批）。
    #[test]
    fn exec_cwd_inside_workspace_keeps_classified_auto_approve() {
        let _serial = crate::TEST_RUNTIME_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if !qaqh_sandbox::SandboxCapabilities::detect().filesystem_write_isolation {
            return;
        }
        let workspace = std::env::temp_dir().join("qaqh-authorization-exec-cwd-ok");
        let invocation = ToolInvocation {
            session_id: "seed-a".into(),
            call_id: "call-exec-cwd-ok".into(),
            tool_name: "exec".into(),
            args: serde_json::json!({
                "command": "cat messages.jsonl",
                "shell": "bash",
                "cwd": workspace.join("src").to_string_lossy(),
            }),
            category: crate::permission::ToolCategory::Exec,
        };
        let mut context = legacy_tool_call_context(
            &invocation,
            crate::permission::PermissionLevel::WorkspaceWrite as u8,
            &workspace,
        );
        context.sandbox_spec.backend = qaqh_sandbox::SandboxBackend::WindowsToken;
        let Admission::Authorized(proof) = admit_with_context(invocation, &context, &HashSet::new())
        else {
            panic!("工作区内 cwd 的只读分类 exec 必须自动放行");
        };
        assert_eq!(proof.grant(), GrantKind::SandboxClassified);
    }

    #[test]
    fn sandbox_run_tier_auto_runs_exec_but_keeps_deny_net_and_session_gates() {
        let _serial = crate::TEST_RUNTIME_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let workspace = std::env::temp_dir().join("qaqh-authorization-sandbox-run");
        let invocation = |tool: &str,
                          category: crate::permission::ToolCategory,
                          args: serde_json::Value| ToolInvocation {
            session_id: "seed-a".into(),
            call_id: "call-sandbox-run".into(),
            tool_name: tool.into(),
            args,
            category,
        };
        let write_enforced = qaqh_sandbox::SandboxCapabilities::detect().filesystem_write_isolation;
        let tier = crate::permission::PermissionLevel::SandboxRun as u8;
        let asks = |admission: Admission| matches!(admission, Admission::ApprovalRequired(_));
        let ctx_for = |invocation: ToolInvocation, backend: qaqh_sandbox::SandboxBackend| {
            let mut context = legacy_tool_call_context(&invocation, tier, &workspace);
            context.sandbox_spec.backend = backend;
            context
        };

        // pwsh 无法判定形态 → 沙箱优先自动放行（写强制平台上）；无强制平台回退审批
        let inv = invocation(
            "exec",
            crate::permission::ToolCategory::Exec,
            serde_json::json!({ "command": "Get-ChildItem src", "shell": "pwsh" }),
        );
        let admission = admit_with_context(
            inv.clone(),
            &ctx_for(inv, qaqh_sandbox::SandboxBackend::WindowsToken),
            &HashSet::new(),
        );
        if write_enforced {
            let Admission::Authorized(proof) = admission else {
                panic!("sandbox-run tier must auto-run unclassified exec under enforced sandbox");
            };
            assert_eq!(proof.grant(), GrantKind::SandboxClassified);
        } else {
            assert!(asks(admission));
        }

        // 审计 F1/P0 回归锁：Auto 未晋升 = 实际 plain spawn = 不得自动放行
        let inv = invocation(
            "exec",
            crate::permission::ToolCategory::Exec,
            serde_json::json!({ "command": "Get-ChildItem src", "shell": "pwsh" }),
        );
        let admission = admit_with_context(
            inv.clone(),
            &ctx_for(inv, qaqh_sandbox::SandboxBackend::Auto),
            &HashSet::new(),
        );
        assert!(asks(admission));

        // deny 形态与网络面任何平台都保持审批
        let inv = invocation(
            "exec",
            crate::permission::ToolCategory::Exec,
            serde_json::json!({ "command": "rm -rf build", "shell": "bash" }),
        );
        assert!(asks(admit_with_context(
            inv.clone(),
            &ctx_for(inv, qaqh_sandbox::SandboxBackend::WindowsToken),
            &HashSet::new(),
        )));
        let inv = invocation(
            "exec",
            crate::permission::ToolCategory::Exec,
            serde_json::json!({ "command": "curl http://x.example", "shell": "bash" }),
        );
        assert!(asks(admit_with_context(
            inv.clone(),
            &ctx_for(inv, qaqh_sandbox::SandboxBackend::WindowsToken),
            &HashSet::new(),
        )));
        assert!(asks(admit(
            invocation(
                "web_fetch",
                crate::permission::ToolCategory::Net,
                serde_json::json!({ "url": "http://x.example" }),
            ),
            tier,
            &workspace,
            &HashSet::new(),
        )));

        // 会话历史文件经命令文本引用：保持审批
        let session_log = qaqh_types::platform::sessions_dir().join("messages.jsonl");
        let inv = invocation(
            "exec",
            crate::permission::ToolCategory::Exec,
            serde_json::json!({ "command": format!("cat {}", session_log.display()), "shell": "bash" }),
        );
        assert!(asks(admit_with_context(
            inv.clone(),
            &ctx_for(inv, qaqh_sandbox::SandboxBackend::WindowsToken),
            &HashSet::new(),
        )));

        // 平台无写强制时 pwsh 分支已断言审批；有强制时 above 分支已断言自动
    }

    // ── 子代理沙箱（方案 B）──

    /// 持锁 + 置位沙箱；Drop 时复位。沙箱标志是 thread-local（`SUBAGENT_SANDBOX`
    /// 为 `Cell<bool>`，只作用于本测试线程），串行化与其他运行时状态测试共用
    /// `TEST_RUNTIME_SERIAL`。
    fn sandbox_guard() -> impl Drop {
        struct Guard {
            _serial: std::sync::MutexGuard<'static, ()>,
        }
        impl Drop for Guard {
            fn drop(&mut self) {
                set_subagent_sandbox(false);
            }
        }
        let serial = crate::TEST_RUNTIME_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        set_subagent_sandbox(true);
        Guard { _serial: serial }
    }

    fn invoke(
        tool: &str,
        args: serde_json::Value,
        ws: &std::path::Path,
        category: crate::permission::ToolCategory,
    ) -> Admission {
        admit(
            ToolInvocation {
                session_id: "sub-a".into(),
                call_id: "call-s".into(),
                tool_name: tool.into(),
                args,
                category,
            },
            1, // 即使主代理是 read-only 档，沙箱下 workspace 内文件操作也自动批准
            ws,
            &HashSet::new(),
        )
    }

    #[test]
    fn sandbox_auto_approves_workspace_file_ops_even_at_level_1() {
        let _g = sandbox_guard();
        let ws = std::env::temp_dir().join("qaqh-sandbox-a");
        // workspace 内写：Level 1 本应 AskUser，沙箱下自动批准。
        let admission = invoke(
            "write",
            serde_json::json!({ "path": ws.join("notes.md") }),
            &ws,
            crate::permission::ToolCategory::Write,
        );
        assert!(
            matches!(admission, Admission::Authorized(_)),
            "expected authorized"
        );
        // workspace 内读：同样自动。
        let admission = invoke(
            "read",
            serde_json::json!({ "path": ws.join("notes.md") }),
            &ws,
            crate::permission::ToolCategory::Read,
        );
        assert!(
            matches!(admission, Admission::Authorized(_)),
            "expected authorized"
        );
    }

    #[test]
    fn sandbox_denies_cross_workspace_access() {
        let _g = sandbox_guard();
        let ws = std::env::temp_dir().join("qaqh-sandbox-b");
        let outside = std::env::temp_dir().join("qaqh-sandbox-b-outside");
        let admission = invoke(
            "write",
            serde_json::json!({ "path": outside.join("secret.txt") }),
            &ws,
            crate::permission::ToolCategory::Write,
        );
        match admission {
            Admission::Denied(reason) => {
                assert!(reason.contains("sandbox"), "{reason}");
            }
            _other => panic!("cross-workspace write must be denied, got non-denied"),
        }
        // 2026-10-05 读自由规则后沙箱兜底：needs_permission 对 Read 无条件
        // 放行，沙箱的「跨 workspace 拒读」改在 admit 的 AutoApprove 分支兑现。
        let admission = invoke(
            "read",
            serde_json::json!({ "path": outside.join("secret.txt") }),
            &ws,
            crate::permission::ToolCategory::Read,
        );
        match admission {
            Admission::Denied(reason) => {
                assert!(reason.contains("sandbox"), "{reason}");
            }
            _other => panic!("cross-workspace read must be denied in sandbox, got non-denied"),
        }
    }

    /// 设置本线程的 actor 工作区并 Drop 时清除。
    ///
    /// `extract_target_paths` 用 `current_workspace()` 解析相对路径；生产里由
    /// actor 线程捕获后 install 到工具线程，测试里就地设置以对齐语义。
    fn actor_workspace_guard(ws: &std::path::Path) -> impl Drop {
        struct Guard;
        impl Drop for Guard {
            fn drop(&mut self) {
                crate::clear_actor_context();
            }
        }
        crate::set_actor_context(&ws.to_string_lossy(), "seed-sandbox");
        Guard
    }

    #[test]
    fn sandbox_approves_workspace_relative_paths() {
        // `read` 的路径契约允许工作区相对路径（`qaqh-file-tools/src/file_query.rs:20`
        // "workspace-relative or absolute"），授权侧按同一基准解析
        // （`extract_target_paths` 末尾 `map(resolve_target_path)`）。工作区内的
        // 相对路径必须放行，`..` 逃逸出去仍然拒。
        let _g = sandbox_guard();
        let ws = std::env::temp_dir().join("qaqh-sandbox-rel");
        let _wsg = actor_workspace_guard(&ws);
        for (tool, category) in [
            ("read", crate::permission::ToolCategory::Read),
            ("write", crate::permission::ToolCategory::Write),
        ] {
            let admission = invoke(
                tool,
                serde_json::json!({ "path": "src/main.rs" }),
                &ws,
                category,
            );
            assert!(
                matches!(admission, Admission::Authorized(_)),
                "{tool} with a workspace-relative path must be authorized in sandbox"
            );
        }
        let admission = invoke(
            "read",
            serde_json::json!({ "path": "../qaqh-sandbox-escape.txt" }),
            &ws,
            crate::permission::ToolCategory::Read,
        );
        match admission {
            Admission::Denied(reason) => assert!(reason.contains("sandbox"), "{reason}"),
            _other => panic!("relative path escaping the workspace must stay denied"),
        }
    }

    #[test]
    fn sandbox_allows_file_ops_when_workspace_is_unset() {
        // 会话未绑定工作区（`""` / `"."`）：没有「区外」可言，文件读写放行——
        // 大多数 coding agent 允许不选目录，不设工作区也必须能用。
        // exec / 网络 / MCP 不依赖工作区，仍然一律拒。
        let _g = sandbox_guard();
        let anywhere = std::env::temp_dir().join("qaqh-sandbox-nounset-any.txt");
        for ws in [PathBuf::from(""), PathBuf::from(".")] {
            for (tool, category) in [
                ("read", crate::permission::ToolCategory::Read),
                ("write", crate::permission::ToolCategory::Write),
            ] {
                let admission =
                    invoke(tool, serde_json::json!({ "path": anywhere }), &ws, category);
                assert!(
                    matches!(admission, Admission::Authorized(_)),
                    "{tool} must not be confined when the workspace is unset ({ws:?})"
                );
            }
        }
        for (tool, category) in [
            ("exec", crate::permission::ToolCategory::Exec),
            ("web_fetch", crate::permission::ToolCategory::Net),
        ] {
            let admission = invoke(
                tool,
                serde_json::json!({ "command": "whoami" }),
                &PathBuf::from("."),
                category,
            );
            assert!(
                matches!(admission, Admission::Denied(_)),
                "{tool} must stay denied in sandbox even when the workspace is unset"
            );
        }
    }

    #[test]
    fn sandbox_denies_sensitive_session_files_even_when_workspace_is_unset() {
        // 放宽「未设置工作区」不能顺带打开数据外泄面：会话历史 / 凭据在任何
        // 情况下都不给子代理读（沙箱没有审批通道，只能拒）。
        let _g = sandbox_guard();
        let secret = qaqh_types::platform::sessions_dir()
            .join("seed-other")
            .join("messages.jsonl");
        let admission = invoke(
            "read",
            serde_json::json!({ "path": secret }),
            &PathBuf::from("."),
            crate::permission::ToolCategory::Read,
        );
        match admission {
            Admission::Denied(reason) => assert!(reason.contains("sandbox"), "{reason}"),
            _other => panic!("sensitive session files must stay denied in sandbox"),
        }
    }

    #[test]
    fn main_agent_reads_outside_workspace_auto_approve_at_every_tier() {
        // 2026-10-05 读自由规则：读取完全不受工作区限制——主代理在 read-only /
        // workspace-write 档下读工作区外路径不再弹审批（敏感路径守卫仍在前）。
        let _serial = crate::TEST_RUNTIME_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        set_subagent_sandbox(false);
        let ws = std::env::temp_dir().join("qaqh-read-free-ws");
        let outside = std::env::temp_dir().join("qaqh-read-free-outside");
        for level in [1u8, 2, 3] {
            let admission = admit(
                ToolInvocation {
                    session_id: "seed-readfree".into(),
                    call_id: format!("call-readfree-{level}"),
                    tool_name: "read".into(),
                    args: serde_json::json!({ "path": outside.join("notes.txt") }),
                    category: crate::permission::ToolCategory::Read,
                },
                level,
                &ws,
                &HashSet::new(),
            );
            assert!(
                matches!(admission, Admission::Authorized(_)),
                "level {level} read outside workspace must auto-approve, got non-authorized"
            );
        }
    }

    #[test]
    fn sandbox_denies_exec_and_net() {
        let _g = sandbox_guard();
        let ws = std::env::temp_dir().join("qaqh-sandbox-c");
        for (tool, args) in [
            ("exec", serde_json::json!({ "command": "whoami" })),
            (
                "web_fetch",
                serde_json::json!({ "url": "https://example.com" }),
            ),
        ] {
            let category = if tool == "exec" {
                crate::permission::ToolCategory::Exec
            } else {
                crate::permission::ToolCategory::Net
            };
            let admission = invoke(tool, args, &ws, category);
            assert!(
                matches!(admission, Admission::Denied(_)),
                "{tool} must be denied in sandbox, got non-denied"
            );
        }
    }

    #[test]
    fn mcp_tools_denied_in_subagent_sandbox() {
        // S3：MCP 工具（category Exec/Net）在子代理沙箱一律拒绝——
        // D5 快路径绕过 needs_permission，沙箱拦截在此显式兑现。
        let _g = sandbox_guard();
        let ws = std::env::temp_dir().join("qaqh-sandbox-mcp");
        for tool in ["mcp__demo__echo", "mcp__web-search__search"] {
            let admission = invoke(
                tool,
                serde_json::json!({}),
                &ws,
                crate::permission::ToolCategory::Exec,
            );
            let denied = matches!(
                &admission,
                Admission::Denied(reason) if reason.contains("host-only")
            );
            assert!(
                denied,
                "{tool} must be sandbox-denied (host-only), got a non-denied admission"
            );
        }
    }

    #[test]
    fn mcp_exec_net_require_approval_until_skip_permissions() {
        // T-8-1（安全审查 P1-1 / O-4）：D5 不再对 Exec/Net 类别无条件放行。
        // read-only / workspace-write 必须审批；skip-permissions 是显式
        // bypass，继续走 D5 快路径。
        // 与 sandbox_guard 共用 `TEST_RUNTIME_SERIAL` 串行化。
        let _serial = crate::TEST_RUNTIME_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        set_subagent_sandbox(false);
        let ws = std::env::temp_dir().join("qaqh-mcp-d5");
        for level in [1u8, 2] {
            let admission = admit(
                ToolInvocation {
                    session_id: "seed-d5".into(),
                    call_id: format!("call-d5-{level}"),
                    tool_name: "mcp__demo__echo".into(),
                    args: serde_json::json!({}),
                    category: crate::permission::ToolCategory::Exec,
                },
                level,
                &ws,
                &HashSet::new(),
            );
            assert!(
                matches!(admission, Admission::ApprovalRequired(_)),
                "level {level} Exec MCP call must require approval, got non-approval"
            );
        }
        let admission = admit(
            ToolInvocation {
                session_id: "seed-d5".into(),
                call_id: "call-d5-4".into(),
                tool_name: "mcp__demo__echo".into(),
                args: serde_json::json!({}),
                category: crate::permission::ToolCategory::Exec,
            },
            3,
            &ws,
            &HashSet::new(),
        );
        assert!(
            matches!(admission, Admission::Authorized(_)),
            "skip-permissions explicit bypass must authorize MCP Exec calls"
        );
    }

    #[test]
    fn mcp_read_tools_keep_d5_fast_path() {
        // 只读动态工具（mcp resources 聚合）不得被 T-8-1 收紧误伤。
        let _serial = crate::TEST_RUNTIME_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        set_subagent_sandbox(false);
        let ws = std::env::temp_dir().join("qaqh-mcp-d5-read");
        for level in [1u8, 2, 3] {
            let admission = admit(
                ToolInvocation {
                    session_id: "seed-d5r".into(),
                    call_id: "call-d5r".into(),
                    tool_name: "mcp__demo__resources".into(),
                    args: serde_json::json!({}),
                    category: crate::permission::ToolCategory::Read,
                },
                level,
                &ws,
                &HashSet::new(),
            );
            assert!(
                matches!(admission, Admission::Authorized(_)),
                "level {level} Read MCP call must keep D5 bypass, got non-authorized"
            );
        }
    }

    #[test]
    fn non_sandbox_still_requires_approval() {
        // 未启用沙箱：read-only 档写仍需审批（主代理行为不受影响）。
        // 与 sandbox_guard 共用 `TEST_RUNTIME_SERIAL` 串行化。
        let _serial = crate::TEST_RUNTIME_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        set_subagent_sandbox(false);
        let ws = std::env::temp_dir().join("qaqh-sandbox-d");
        let admission = invoke(
            "write",
            serde_json::json!({ "path": ws.join("notes.md") }),
            &ws,
            crate::permission::ToolCategory::Write,
        );
        assert!(
            matches!(admission, Admission::ApprovalRequired(_)),
            "non-sandbox must keep approval path"
        );
    }
}
