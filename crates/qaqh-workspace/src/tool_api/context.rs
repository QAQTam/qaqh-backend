//! 显式执行上下文（base spec §6.3 + 09-19 补充稿 §4.5）。
//!
//! 目标：调用身份 / 工作区 / 取消 / 进度全部**显式传递**，禁止线程局部
//! 隐式状态（迁移期 legacy `ToolCallCtx` 兼容字段保留，新 API 不得依赖）。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::progress::ProgressSink;
use crate::permission::PermissionLevel;
pub use qaqh_policy::SandboxSpec;

/// Sandbox policy effective for one tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SandboxMode {
    /// Main session actor: normal permission/admission flow.
    #[default]
    Main,
    /// Subagent actor: no interactive approval channel; workspace file
    /// operations may auto-approve, while exec/net/cross-workspace calls are
    /// denied.
    Subagent,
}

/// 调用来源（09-19 补充稿 §4.2）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolCallSource {
    /// 模型在对话回合内发起。
    Model,
    /// 用户经 UI 直接调用（快捷工具）。
    User,
    /// 子代理/编排发起。
    Agent {
        /// 父调用 id（若可追踪）。
        parent_call_id: Option<String>,
    },
}

/// 代理运行模式（v1 本地镜像）。
///
/// domain 的 `ConversationMode` 因 R-4 约束（workspace → domain 仅限
/// Dashboard* 类型）不可引入；映射由 runtime 适配层负责。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentMode {
    /// 编码模式（默认）。
    #[default]
    Code,
    /// 计划模式（写类工具受限）。
    Plan,
}

/// 取消信号（v1 同步执行器形态：`Arc<AtomicBool>` 只读句柄）。
///
/// 未来引入 async 执行器时可替换为框架级 token（保留同名替换点，
/// 见 base spec §6.4 备注）。
#[derive(Debug, Clone, Default)]
pub struct CancellationToken {
    inner: Arc<AtomicBool>,
}

impl CancellationToken {
    /// 新建未取消的 token。
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a token sharing an existing cancellation flag.
    ///
    /// This is the bridge used when a runtime-owned cancellation tree must be
    /// projected into an explicit [`ToolCallContext`].
    pub fn from_shared_flag(flag: Arc<AtomicBool>) -> Self {
        Self { inner: flag }
    }

    /// 置位取消（宿主调用；工具侧只读）。
    pub fn cancel(&self) {
        self.inner.store(true, Ordering::SeqCst);
    }

    /// 是否已取消（工具在 IO 边界/循环批次处轮询）。
    pub fn is_cancelled(&self) -> bool {
        self.inner.load(Ordering::SeqCst)
    }

    /// 与 legacy `ToolCallCtx.cancel` 共享同一信号的句柄（桥接专用，不对外）。
    pub(crate) fn shared_flag(&self) -> Arc<AtomicBool> {
        self.inner.clone()
    }
}

/// 显式工具调用上下文（base spec §6.3）。
#[derive(Debug, Clone)]
pub struct ToolCallContext {
    /// 调用 id（对应模型 tool_call_id）。
    pub call_id: String,
    /// 会话 seed。
    pub session_id: String,
    /// 工作区根（执行器强制注入；工具不得读线程局部）。
    pub workspace_root: PathBuf,
    /// 运行模式。
    pub mode: AgentMode,
    /// 生效权限档位。
    pub permission_level: PermissionLevel,
    /// 生效沙箱模式。
    pub sandbox: SandboxMode,
    /// Canonical sandbox policy for this call.
    ///
    /// This is resolved before execution and carried explicitly so runtime
    /// intent hashing and the exec handler consume the same policy value.
    pub sandbox_spec: SandboxSpec,
    /// Host-configured default shell for the exec tool. `None` = platform
    /// priority auto-detection; an explicit exec `shell` argument still wins.
    pub exec_default_shell: Option<String>,
    /// 生效超时（调用方显式值覆盖 descriptor 默认值；构造后定稿）。
    pub timeout: Duration,
    /// 取消信号（只读）。
    pub cancellation: CancellationToken,
    /// 进度出口（`capabilities.streaming == true` 且宿主启用时非空）。
    pub progress: Option<ProgressSink>,
    /// 调用来源。
    pub source: ToolCallSource,
}

impl ToolCallContext {
    /// Canonical sandbox policy for this call.
    pub fn sandbox_spec(&self) -> &SandboxSpec {
        &self.sandbox_spec
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_token_roundtrip() {
        let token = CancellationToken::new();
        assert!(!token.is_cancelled());
        let cloned = token.clone();
        token.cancel();
        assert!(cloned.is_cancelled(), "克隆句柄共享同一信号");
    }

    #[test]
    fn context_is_constructible_with_explicit_fields() {
        let ctx = ToolCallContext {
            call_id: "call_1".to_owned(),
            session_id: "d9a1a320".to_owned(),
            workspace_root: PathBuf::from("/tmp/ws"),
            mode: AgentMode::Code,
            permission_level: PermissionLevel::ReadOnly,
            sandbox: SandboxMode::Main,
            sandbox_spec: SandboxSpec::workspace_write(PathBuf::from("/tmp/ws")),
            exec_default_shell: None,
            timeout: Duration::from_secs(30),
            cancellation: CancellationToken::new(),
            progress: None,
            source: ToolCallSource::Model,
        };
        assert_eq!(ctx.mode, AgentMode::Code);
        assert_eq!(ctx.source, ToolCallSource::Model);
        assert_eq!(
            ctx.sandbox_spec().writable_roots,
            vec![PathBuf::from("/tmp/ws")]
        );
    }

    #[test]
    fn explicit_sandbox_spec_is_not_derived_from_workspace_root() {
        let ctx = ToolCallContext {
            call_id: "call_1".to_owned(),
            session_id: "d9a1a320".to_owned(),
            workspace_root: PathBuf::from("/tmp/ws"),
            mode: AgentMode::Code,
            permission_level: PermissionLevel::ReadOnly,
            sandbox: SandboxMode::Main,
            sandbox_spec: SandboxSpec::disabled(),
            exec_default_shell: None,
            timeout: Duration::from_secs(30),
            cancellation: CancellationToken::new(),
            progress: None,
            source: ToolCallSource::Model,
        };

        assert!(!ctx.sandbox_spec().enabled);
        assert!(ctx.sandbox_spec().writable_roots.is_empty());
    }
}
