//! Permission admission and single-use authorization proofs.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::tool_api::{CancellationToken, SandboxMode, ToolCallContext, ToolCallSource};

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

/// Identity of a single tool invocation destined for a handler.
#[derive(Debug, Clone)]
pub struct ToolInvocation {
    pub session_id: String,
    pub call_id: String,
    pub tool_name: String,
    pub action: String,
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
    /// 策略自动放行（含 MCP D5 快路径与 Level 4 bypass）。
    Auto,
    /// 用户在审批通道显式批准（一次性凭证）。
    UserApproved,
    /// 子代理沙箱内自动批准（工作区内文件操作）。
    SandboxAuto,
}

impl GrantKind {
    /// 审计词汇表（v2 `decision.outcome`）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::UserApproved => "user_approved",
            Self::SandboxAuto => "sandbox_auto",
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

    pub fn action(&self) -> &str {
        &self.invocation.action
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
        (self.invocation, self.resources, self.workspace_root, self.grant)
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
    context: ToolCallContext,
    tool_name: String,
    action: String,
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
            context,
            tool_name: invocation.tool_name,
            action: invocation.action,
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

    pub fn action(&self) -> &str {
        &self.action
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
        let context = self.context;
        let invocation = ToolInvocation {
            session_id: context.session_id.clone(),
            call_id: context.call_id.clone(),
            tool_name: self.tool_name,
            action: self.action,
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
    // Level 1/2/3 落到下面的 needs_permission 决策并进入审批；Level 4 是
    // 显式 bypass，继续走 D5 快路径。只读类（Read，如 `mcp` resources 聚合）
    // 在任何档位都保留 D5 快路径，避免误伤。
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
        ) || level == crate::permission::PermissionLevel::Unrestricted;
        if d5_bypass {
            let mut resources =
                crate::permission::extract_target_paths(&invocation.tool_name, &invocation.args);
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

    let workspace_root = crate::permission::resolve_target_path(context.workspace_root.clone());
    match crate::permission::needs_permission(
        level,
        &invocation.tool_name,
        &invocation.args,
        &workspace_root,
        trusted_dirs,
        invocation.category,
    ) {
        crate::permission::PermissionDecision::AutoApprove => {
            let mut resources =
                crate::permission::extract_target_paths(&invocation.tool_name, &invocation.args);
            resources.sort();
            resources.dedup();
            Admission::Authorized(AuthorizedToolCall::new(
                invocation,
                resources,
                workspace_root,
                GrantKind::Auto,
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
                if file_ops && crate::permission::all_within_workspace(&paths, &workspace_root) {
                    // workspace 内文件操作：自动批准（等价 Level 3 语义）。
                    let mut resources = crate::permission::extract_target_paths(
                        &invocation.tool_name,
                        &invocation.args,
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
        action: String::new(),
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
        action: String::new(),
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
        // 与 sandbox 测试共享全局 SUBAGENT_SANDBOX：并行时沙箱置位会使本测试的
        // "level 1 write 必须要求审批"断言失败（沙箱下写自动批准）——串行化。
        let _serial = crate::TEST_RUNTIME_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let workspace = std::env::temp_dir().join("qaqh-authorization-risk");
        let invocation = ToolInvocation {
            session_id: "seed-a".into(),
            call_id: "call-a".into(),
            tool_name: "write".into(),
            action: String::new(),
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

    // ── 子代理沙箱（方案 B）──

    /// 持锁 + 置位沙箱；Drop 时复位。全局 AtomicBool 会被并行测试干扰，
    /// 必须经 `TEST_RUNTIME_SERIAL` 串行化（同 crate 其他全局状态测试）。
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
                action: String::new(),
                args,
                category,
            },
            1, // 即使主代理是 MaxLockdown，沙箱下 workspace 内文件操作也自动批准
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
    fn mcp_exec_net_require_approval_until_unrestricted() {
        // T-8-1（安全审查 P1-1 / O-4）：D5 不再对 Exec/Net 类别无条件放行。
        // Level 1/2/3 必须审批；Level 4 是显式 bypass，继续走 D5 快路径。
        // 全局 AtomicBool 需串行（与 sandbox_guard 同锁）。
        let _serial = crate::TEST_RUNTIME_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        set_subagent_sandbox(false);
        let ws = std::env::temp_dir().join("qaqh-mcp-d5");
        for level in [1u8, 2, 3] {
            let admission = admit(
                ToolInvocation {
                    session_id: "seed-d5".into(),
                    call_id: format!("call-d5-{level}"),
                    tool_name: "mcp__demo__echo".into(),
                    action: String::new(),
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
                action: String::new(),
                args: serde_json::json!({}),
                category: crate::permission::ToolCategory::Exec,
            },
            4,
            &ws,
            &HashSet::new(),
        );
        assert!(
            matches!(admission, Admission::Authorized(_)),
            "level 4 explicit bypass must authorize MCP Exec calls"
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
        for level in [1u8, 2, 3, 4] {
            let admission = admit(
                ToolInvocation {
                    session_id: "seed-d5r".into(),
                    call_id: "call-d5r".into(),
                    tool_name: "mcp__demo__resources".into(),
                    action: String::new(),
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
        // 未启用沙箱：Level 1 写仍需审批（主代理行为不受影响）。
        // 全局 AtomicBool 需串行（与 sandbox_guard 同锁）。
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
