//! 显式执行上下文（base spec §6.3 + 09-19 补充稿 §4.5）。
//!
//! 目标：调用身份 / 工作区 / 取消 / 进度全部**显式传递**，禁止线程局部
//! 隐式状态。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::task::{Context as TaskContext, Poll, Waker};
use std::time::Duration;

use super::official_web::OfficialWebBackend;
use super::progress::ProgressSink;
use crate::permission::PermissionLevel;
pub use qaqh_policy::SandboxSpec;
use qaqh_types::{ExecSessionId, ToolCallOutcome};

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

/// Validated command invocation passed to the session-owned execution host.
#[derive(Debug, Clone)]
pub struct ExecCommandRequest {
    pub command: String,
    pub argv: Vec<String>,
    pub cwd: Option<String>,
    pub env: Vec<(String, String)>,
    pub tty: bool,
    pub yield_time_ms: u32,
    pub timeout_ms: Option<u64>,
    pub max_output_chars: u32,
    pub sandbox_spec: Option<SandboxSpec>,
}

#[derive(Debug, Clone)]
pub struct WriteStdinRequest {
    pub exec_session_id: ExecSessionId,
    pub chars: String,
    pub yield_time_ms: u32,
    pub max_output_chars: u32,
}

#[derive(Debug, Clone)]
pub struct ExecCommandResult {
    pub outcome: ToolCallOutcome,
    pub exec_session_id: Option<ExecSessionId>,
    pub output: String,
    pub output_truncated: bool,
    pub omitted_bytes: u64,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub cancelled: bool,
    pub input_bytes_written: Option<usize>,
}

/// Per-call bridge to the session-owned execution manager.
pub trait ExecSessionHost: std::fmt::Debug + Send + Sync {
    fn exec_command(
        &self,
        context: &ToolCallContext,
        request: ExecCommandRequest,
    ) -> Result<ExecCommandResult, super::error::ToolError>;

    fn write_stdin(
        &self,
        context: &ToolCallContext,
        request: WriteStdinRequest,
    ) -> Result<ExecCommandResult, super::error::ToolError>;
}

/// 取消信号：同步轮询面与异步等待面共用一个真身。
///
/// - **同步面**（exec 子进程循环、ptc 工具）： [`Self::shared_flag`] 拿
///   `Arc<AtomicBool>` 在 IO 边界轮询——v1 同步执行器的既有形态，逐字不变。
/// - **异步面**（MCP/LSP 的 `tokio::select!` 桥，v4.4 P1-1）： [`Self::cancelled`]
///   是可等待的 future，`cancel()` 置旗后**逐个唤醒**登记的 waker。取消延迟
///   因此等于一次线程唤醒，不再是轮询步长（旧实现 250ms `recv_timeout`）。
///
/// 置旗与唤醒必须同源：异步面若挂在别处造的裸 `AtomicBool` 上，`cancel()`
/// 就没人能唤醒它——所以「从外部旗标构造 token」的反向入口已随 P1-1 一并删除
/// （调用方改为持有 token 本身，需要裸旗标时用 [`Self::shared_flag`]）。
#[derive(Debug, Clone, Default)]
pub struct CancellationToken {
    inner: Arc<TokenState>,
}

#[derive(Debug, Default)]
struct TokenState {
    /// 同步轮询面直接读这枚旗（[`CancellationToken::shared_flag`] 交出同一个
    /// `Arc`，因此 exec 子进程循环看到的与异步面看到的是同一份真相）。
    flag: Arc<AtomicBool>,
    /// 挂在 [`CancellationToken::cancelled`] 上的任务 waker。
    waiters: Mutex<Vec<Waker>>,
}

impl CancellationToken {
    /// 新建未取消的 token。
    pub fn new() -> Self {
        Self::default()
    }

    /// 置位取消并唤醒全部异步等待者（宿主调用；工具侧只读）。
    pub fn cancel(&self) {
        self.inner.flag.store(true, Ordering::SeqCst);
        let pending = std::mem::take(&mut *self.lock_waiters());
        for waker in pending {
            waker.wake();
        }
    }

    /// 是否已取消（同步面在 IO 边界/循环批次处轮询）。
    pub fn is_cancelled(&self) -> bool {
        self.inner.flag.load(Ordering::SeqCst)
    }

    /// 共享取消标志的 `Arc<AtomicBool>` 句柄（同步轮询面专用）。
    ///
    /// 注意该句柄**不带**唤醒能力：绕过 [`Self::cancel`] 直接对旗标 `store`
    /// 只会让同步面看见取消，异步等待者不会被唤醒。取消的唯一入口是本类型的
    /// [`Self::cancel`]。
    pub fn shared_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.inner.flag)
    }

    /// 等待取消发生的 future（已在取消后轮询立即 Ready）。
    pub fn cancelled(&self) -> Cancelled<'_> {
        Cancelled { token: self }
    }

    fn lock_waiters(&self) -> std::sync::MutexGuard<'_, Vec<Waker>> {
        self.inner
            .waiters
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// [`CancellationToken::cancelled`] 的 future：输出恒为 `()`，仅在取消置位
/// 时被唤醒（`select!` 里当作「取消分支」用）。
#[derive(Debug)]
pub struct Cancelled<'a> {
    token: &'a CancellationToken,
}

impl std::future::Future for Cancelled<'_> {
    type Output = ();

    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
        if self.token.is_cancelled() {
            return Poll::Ready(());
        }
        let mut waiters = self.token.lock_waiters();
        // 取锁后再看一次：置旗与唤醒之间若插进本次 poll，会丢唤醒。
        if self.token.is_cancelled() {
            return Poll::Ready(());
        }
        if !waiters.iter().any(|waker| waker.will_wake(cx.waker())) {
            waiters.push(cx.waker().clone());
        }
        Poll::Pending
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
    /// exec 输出字符上限（P0-2：原门面钩子 `hooks::EXEC_MAX_OUTPUT_CHARS` 的
    /// 显式化——随 fold policy 在执行入口注入，exec handler 据此截断；
    /// `None` = 调用侧按各自缺省兜底）。
    pub exec_max_output_chars: Option<u32>,
    /// 生效超时（调用方显式值覆盖 descriptor 默认值；构造后定稿）。
    pub timeout: Duration,
    /// 取消信号（只读）。
    pub cancellation: CancellationToken,
    /// 进度出口（`capabilities.streaming == true` 且宿主启用时非空）。
    pub progress: Option<ProgressSink>,
    /// exec 直通进度出口（v2.1 client-tools `Progress` 帧的宿主侧生产点）。
    ///
    /// runtime 在执行入口把 exec 进度通道 sender 注入；exec handler 优先
    /// 直用，`progress`（ProgressSink → v1 进度桥）退为兜底。非 exec 工具忽略。
    pub exec_progress: Option<crate::ExecProgressSender>,
    /// Session-owned execution manager injected by the runtime for exec tools.
    pub exec_host: Option<Arc<dyn ExecSessionHost>>,
    /// 官方网页工具后端（fetch / search）。
    ///
    /// 只有声明了官方工具面的 profile（`EndpointCompat::official_web_tools`）才由
    /// `qaqh-runtime` **按调用**注入；`None` = 工具层走自实现 HTTP 抓取。
    /// legacy/ambient 构造点一律 `None`：那里拿不到会话 profile，按 AGENTS-x I10
    /// 不去读线程局部补这个值。
    pub official_web: Option<OfficialWebBackend>,
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
            permission_level: PermissionLevel::ReadOnly,
            sandbox: SandboxMode::Main,
            sandbox_spec: SandboxSpec::workspace_write(PathBuf::from("/tmp/ws")),
            exec_default_shell: None,
            exec_max_output_chars: None,
            timeout: Duration::from_secs(30),
            cancellation: CancellationToken::new(),
            progress: None,
            exec_progress: None,
            exec_host: None,
            official_web: None,
            source: ToolCallSource::Model,
        };
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
            permission_level: PermissionLevel::ReadOnly,
            sandbox: SandboxMode::Main,
            sandbox_spec: SandboxSpec::disabled(),
            exec_default_shell: None,
            exec_max_output_chars: None,
            timeout: Duration::from_secs(30),
            cancellation: CancellationToken::new(),
            progress: None,
            exec_progress: None,
            exec_host: None,
            official_web: None,
            source: ToolCallSource::Model,
        };

        assert!(!ctx.sandbox_spec().enabled);
        assert!(ctx.sandbox_spec().writable_roots.is_empty());
    }

    /// park/unpark 版一次性执行器：给 [`CancellationToken::cancelled`] 一个
    /// waker，让当前线程只被「取消」唤醒。不引 tokio 依赖（本 crate 的 runtime
    /// 依赖只有 async_rt 的 rt 面），也不睡轮询。
    struct ParkWaker(std::thread::Thread);

    impl std::task::Wake for ParkWaker {
        fn wake(self: std::sync::Arc<Self>) {
            self.0.unpark();
        }

        fn wake_by_ref(self: &std::sync::Arc<Self>) {
            self.0.unpark();
        }
    }

    /// **P1-1 反证常驻（原语级）**：`cancel()` 必须在远小于旧轮询步长
    /// （mcp `recv_timeout` / lsp `cancel_tick` 都是 250ms）的时间内叫醒挂在
    /// `cancelled()` 上的等待者。旧原语根本没有唤醒面——等待者只能靠轮询
    /// 发现取消，下界就是轮询间隔本身。
    #[test]
    fn cancel_wakes_registered_waiter_without_polling() {
        let token = CancellationToken::new();
        let waiting = {
            let token = token.clone();
            std::thread::spawn(move || {
                let mut future = std::pin::pin!(token.cancelled());
                let waker = std::task::Waker::from(std::sync::Arc::new(ParkWaker(
                    std::thread::current(),
                )));
                let mut cx = std::task::Context::from_waker(&waker);
                loop {
                    if std::future::Future::poll(future.as_mut(), &mut cx).is_ready() {
                        break std::time::Instant::now();
                    }
                    // 只可能被 cancel() 的 wake 唤醒（虚假唤醒由上面的重poll 兜住）。
                    std::thread::park();
                }
            })
        };
        // 让等待者确实进入 Pending（注册过 waker）再取消。
        std::thread::sleep(std::time::Duration::from_millis(200));
        let cancelled_at = std::time::Instant::now();
        token.cancel();
        let ready_at = waiting.join().expect("waiter thread joined");
        let latency = ready_at.saturating_duration_since(cancelled_at);
        assert!(
            latency < std::time::Duration::from_millis(50),
            "取消唤醒延迟 {latency:?} 达到旧实现的轮询步长量级（250ms）"
        );
        // 已取消后再等待必须立即 Ready（不会永久 Pending）。
        let already = std::pin::pin!(token.cancelled())
            .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()));
        assert!(already.is_ready(), "已取消后不得再挂起等待者");
    }
}
