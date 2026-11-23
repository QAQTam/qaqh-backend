//! per-server 连接生命周期（mcp connection.rs 同款状态机，LSP 化）。
//!
//! ```text
//! Disconnected --ensure_connected--> Connected
//! Disconnected(冷却期) --ensure_connected--> Err(ConnectFailed)   // 不重启
//! Connected --crash(mainloop 退出/IO 错)--> Disconnected           // 下一次调用单次重启
//!   重启失败 --> 冷却 30s（期间调用直接报错；LSP server 重，冷却比 mcp 长）
//! Connected --inflight==0 连续 idle_shutdown_secs--> Disconnected // watchdog 回收
//! 任意状态 --shutting_down 闸--> 拒绝 lazy connect/重连/idle 重启（Shutdown）
//! ```
//!
//! 锁模型（mcp 同款）：`state: StdMutex<ConnState>` 短临界段禁跨 await；
//! service 句柄包 `Arc<tokio::sync::Mutex<ServerSession>>`；watchdog 持
//! `Weak<ServerConnection>` 防环。
//!
//! LSP 会话语义（与 mcp 的差异）：
//! - `initialize` 参数带 `workspace_folders=[root]` + `root_uri`（root 钉死在
//!   连接键里，同一 server 不同 root 是不同连接）；
//! - `initialized` 通知后等索引门（rust-analyzer `rustAnalyzer/Indexing` 或
//!   `rustAnalyzer/cachePriming` 的 `WorkDone::End`；通用回退：收到 initialize
//!   回复后等 `startup_timeout` 内首个 `Progress::End`，超时则放行——非 RA
//!   server 无此 token，不能硬等）；
//! - 文档同步 M1 薄版：每次查询前 `didOpen`（磁盘文本，version 自增；已 open
//!   则先 `didClose` 再重开——保证查的是落盘态）；常驻 open 不做增量。

use std::collections::BTreeMap;
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::time::{Duration, Instant};

use async_lsp::concurrency::ConcurrencyLayer;
use async_lsp::panic::CatchUnwindLayer;
use async_lsp::router::Router;
use async_lsp::tracing::TracingLayer;
use async_lsp::{MainLoop, ServerSocket};
use futures::channel::oneshot;
use lsp_types::notification::{Progress, PublishDiagnostics, ShowMessage};
use lsp_types::{
    ClientCapabilities, InitializeParams, InitializedParams, NumberOrString, ProgressParamsValue,
    Url, WindowClientCapabilities, WorkDoneProgress, WorkspaceFolder,
};
use tokio::task::JoinHandle;
use tower::ServiceBuilder;

use qaqh_config::config::LspServerConfig;
use qaqh_config::secrets::SecretStore;

use crate::adapter;
use crate::error::{LspError, LspErrorKind};

/// 连接状态快照（管理面只读视图；mcp ConnStatus 同款）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnStatus {
    Connected { inflight: u64 },
    Disconnected,
    Cooling { remaining_ms: u64 },
    ShuttingDown,
}

/// 生命周期参数（生产默认见 [`LifecycleSettings::default`]；测试可调小）。
#[derive(Debug, Clone)]
pub struct LifecycleSettings {
    /// lazy connect 总超时（LSP 含索引门，默认 30s；mcp 为 10s）。
    pub connect_timeout: Duration,
    /// 重连冷却（默认 30s；LSP server 重，冷却比 mcp 的 5s 长）。
    pub reconnect_cooldown: Duration,
    /// 单连接优雅关闭兜底。
    pub close_timeout: Duration,
    /// idle watchdog 巡检间隔。
    pub idle_tick: Duration,
}

impl Default for LifecycleSettings {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(30),
            reconnect_cooldown: Duration::from_secs(30),
            close_timeout: Duration::from_secs(2),
            idle_tick: Duration::from_millis(200),
        }
    }
}

/// 已连接的 LSP 会话句柄（mainloop 驱动任务 + ServerSocket 请求口 +
/// 子进程持有者）。字段 crate 内可见（tool.rs 的 didOpen 薄版同步用）。
pub(crate) struct ServerSession {
    pub(crate) socket: ServerSocket,
    /// 子进程持有者（drop 即组杀；connection 释放/关闭时连带退出）。
    pub(crate) _child: Box<dyn process_wrap::tokio::ChildWrapper>,
    /// 已 didOpen 的文档版本（uri → version；M1 每次查询重开即自增）。
    pub(crate) opened: BTreeMap<String, i32>,
}

struct ConnState {
    session: Option<Arc<tokio::sync::Mutex<ServerSession>>>,
    /// mainloop 驱动任务句柄（abort 即断连）。
    ///
    /// L-09：这个句柄**不放进** `ServerSession`。session mutex 是数据面锁，
    /// 会被在途请求占住；而「硬拆 driver」是控制面动作，必须无条件做得到，
    /// 否则 `close_timeout` 只是把死等从 Shutdown 请求挪到了锁上。
    /// 每次 `store_connected` 换上一代的句柄（旧的不肯退也已被 abort）。
    driver: Option<JoinHandle<()>>,
    connected: bool,
    cooling_until: Option<Instant>,
    inflight: u64,
    idle_since: Option<Instant>,
}

/// 单个 LSP server 在单个 root 下的连接（lazy connect / 冷却 / idle 回收）。
pub struct ServerConnection {
    server: String,
    root: String,
    server_cfg: LspServerConfig,
    idle_secs: u64,
    settings: LifecycleSettings,
    gate: Arc<AtomicBool>,
    secret_store: SecretStore,
    /// 本代连接 spawn 的进程组 id（adapter 登记；组杀兜底清扫用，Unix）。
    pgid: StdMutex<Option<u32>>,
    state: StdMutex<ConnState>,
    connect_serializer: tokio::sync::Mutex<()>,
    watchdog: StdMutex<Option<JoinHandle<()>>>,
}

impl ServerConnection {
    /// 构造连接（不发起任何 IO——lazy 语义由 [`Self::ensure_connected`] 承担）。
    pub fn new(
        server: impl Into<String>,
        root: impl Into<String>,
        server_cfg: LspServerConfig,
        idle_secs: u64,
        settings: LifecycleSettings,
        gate: Arc<AtomicBool>,
        secret_store: SecretStore,
    ) -> Self {
        Self {
            server: server.into(),
            root: root.into(),
            server_cfg,
            idle_secs,
            settings,
            gate,
            secret_store,
            pgid: StdMutex::new(None),
            state: StdMutex::new(ConnState {
                session: None,
                driver: None,
                connected: false,
                cooling_until: None,
                inflight: 0,
                idle_since: None,
            }),
            connect_serializer: tokio::sync::Mutex::new(()),
            watchdog: StdMutex::new(None),
        }
    }

    pub fn server(&self) -> &str {
        &self.server
    }

    pub fn root(&self) -> &str {
        &self.root
    }

    pub fn server_config(&self) -> &LspServerConfig {
        &self.server_cfg
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, ConnState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 当前状态快照。
    pub fn status(&self) -> ConnStatus {
        if self.gate.load(Ordering::Relaxed) {
            return ConnStatus::ShuttingDown;
        }
        let state = self.lock_state();
        if state.connected && state.session.is_some() {
            return ConnStatus::Connected {
                inflight: state.inflight,
            };
        }
        if let Some(until) = state.cooling_until {
            let now = Instant::now();
            if now < until {
                return ConnStatus::Cooling {
                    remaining_ms: (until - now).as_millis() as u64,
                };
            }
        }
        ConnStatus::Disconnected
    }

    fn cooling_remaining(&self) -> Option<Duration> {
        let state = self.lock_state();
        state
            .cooling_until
            .and_then(|until| until.checked_duration_since(Instant::now()))
    }

    fn arm_cooldown(&self) {
        let mut state = self.lock_state();
        state.cooling_until = Some(Instant::now() + self.settings.reconnect_cooldown);
    }

    /// 调用守卫：inflight +1，drop 时 -1 并刷新 idle_since（mcp CallGuard 同款）。
    ///
    /// L-09 状态闸门（对齐 mcp `begin_call`）：关停闸已落、或本连接根本不在
    /// 连接态，**当场拒**，不再把 inflight 记到一条正被看门狗回收的连接上。
    /// 旧实现无条件 `inflight += 1`，回收窗口里的调用会把守卫挂到死连接上，
    /// 并让 idle 判定误读成「有活在跑」——与无硬顶的 `shutdown()` 互为拖住的两端。
    ///
    /// 未连接报 `ServerCrashed`（与 `request_with_cancel` 取会话句柄时的既有
    /// 词表同律：调用方据此 `mark_crashed()`，下次调用自动重连），不是新码。
    pub fn begin_call(&self) -> Result<CallGuard<'_>, LspError> {
        if self.gate.load(Ordering::Relaxed) {
            return Err(LspError::new(
                LspErrorKind::Shutdown,
                format!(
                    "lsp server {}: daemon shutting down; call rejected",
                    self.server
                ),
            ));
        }
        let mut state = self.lock_state();
        if !state.connected || state.session.is_none() {
            return Err(LspError::new(
                LspErrorKind::ServerCrashed,
                format!("lsp server {} is not connected", self.server),
            ));
        }
        state.inflight += 1;
        state.idle_since = None;
        Ok(CallGuard { conn: self })
    }

    /// lazy connect（幂等；已连接直接返回）。
    pub async fn ensure_connected(self: &Arc<Self>) -> Result<(), LspError> {
        {
            let state = self.lock_state();
            if state.connected && state.session.is_some() {
                return Ok(());
            }
        }
        let _serial = self.connect_serializer.lock().await;
        {
            let state = self.lock_state();
            if state.connected && state.session.is_some() {
                return Ok(());
            }
        }
        if self.gate.load(Ordering::Relaxed) {
            return Err(LspError::new(
                LspErrorKind::Shutdown,
                format!(
                    "lsp server {}: daemon shutting down; lazy connect rejected",
                    self.server
                ),
            ));
        }
        if let Some(remaining) = self.cooling_remaining() {
            return Err(LspError::new(
                LspErrorKind::ConnectFailed,
                format!(
                    "lsp server {}: reconnect cooldown {:.1}s remaining; retry later",
                    self.server,
                    remaining.as_secs_f32()
                ),
            ));
        }
        if self.gate.load(Ordering::Relaxed) {
            return Err(LspError::new(
                LspErrorKind::Shutdown,
                format!(
                    "lsp server {}: daemon shutting down; lazy connect rejected",
                    self.server
                ),
            ));
        }

        let timeout = self
            .server_cfg
            .startup_timeout_secs
            .clamp(1, 600)
            .max(self.settings.connect_timeout.as_secs());
        let timeout = Duration::from_secs(timeout);
        let this = Arc::clone(self);
        let attempt = tokio::time::timeout(timeout, this.connect_once()).await;
        match attempt {
            Err(_elapsed) => {
                self.arm_cooldown();
                Err(LspError::new(
                    LspErrorKind::ConnectTimeout,
                    format!(
                        "lsp server {}: connect timed out after {timeout:?}",
                        self.server
                    ),
                ))
            }
            Ok(Err(error)) => {
                self.arm_cooldown();
                Err(error)
            }
            Ok(Ok(())) => Ok(()),
        }
    }

    /// 单次连接全流程：解析 secret → spawn → mainloop → initialize → 索引门。
    async fn connect_once(self: Arc<Self>) -> Result<(), LspError> {
        let resolved = adapter::resolve_server_secrets(&self.server_cfg, &self.secret_store)?;
        let wrap = adapter::build_stdio_command(&resolved, &self.root);
        log::info!(
            "[lsp] server {}: spawning stdio transport (command={:?}, root={})",
            self.server,
            resolved.command,
            self.root
        );
        let spawned = adapter::spawn_server(wrap)?;
        // L-07：本代 spawn 的进程组 id 只在这条连接手里流动——旧实现先塞进
        // 按 server 名键控的全局表，双 root 并发连同名 server 时后者覆盖前者，
        // 先连的一方 close 时会对**别人的**进程组 SIGKILL。
        let pgid = spawned.pid;
        let mut child = spawned.child;
        let stdin = child.stdin().take().ok_or_else(|| {
            LspError::new(
                LspErrorKind::ConnectFailed,
                "spawned LSP server has no stdin pipe".to_owned(),
            )
        })?;
        let stdout = child.stdout().take().ok_or_else(|| {
            LspError::new(
                LspErrorKind::ConnectFailed,
                "spawned LSP server has no stdout pipe".to_owned(),
            )
        })?;

        // 索引门：RA 系 token 的 WorkDone::End 到达即放行。
        let (indexed_tx, indexed_rx) = oneshot::channel::<()>();
        let (mainloop, socket) = MainLoop::new_client(|_server| {
            let mut router = Router::new(ClientGate {
                indexed_tx: Some(indexed_tx),
            });
            router
                .notification::<Progress>(|this, prog| {
                    // 索引门：RA 系 token 的 WorkDone::End 到达即放行。
                    let is_index_end = matches!(&prog.token, NumberOrString::String(s) if is_index_end_token(s))
                        && matches!(
                            prog.value,
                            ProgressParamsValue::WorkDone(WorkDoneProgress::End(_))
                        );
                    if is_index_end && let Some(tx) = this.indexed_tx.take() {
                        let _: Result<_, _> = tx.send(());
                    }
                    ControlFlow::Continue(())
                })
                .notification::<PublishDiagnostics>(|_, _| ControlFlow::Continue(()))
                .notification::<ShowMessage>(|_, params| {
                    log::info!("[lsp] server message {:?}: {}", params.typ, params.message);
                    ControlFlow::Continue(())
                })
                .event(|_, _: StopLoop| ControlFlow::Break(Ok(())));
            ServiceBuilder::new()
                .layer(TracingLayer::default())
                .layer(CatchUnwindLayer::default())
                .layer(ConcurrencyLayer::default())
                .service(router)
        });

        let driver = tokio::spawn(async move {
            // tokio IO → futures IO（async-lsp inspector.rs 同款 compat）。
            use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
            let stdout = stdout.compat();
            let mut stdin = stdin.compat_write();
            if let Err(e) = mainloop.run_buffered(stdout, &mut stdin).await {
                log::warn!("[lsp] mainloop exited with error: {e}");
            }
        });

        // initialize（root 钉死：workspace_folders 主用；root_uri 已废弃但
        // 部分老 server 仍读它，双写兼容——deprecated 警告 allow 压住，M2 基线
        // 确认后移除）。
        let root_uri = Url::from_file_path(&self.root).map_err(|_| {
            LspError::new(
                LspErrorKind::ConnectFailed,
                format!(
                    "lsp server {}: root {:?} is not a valid file path",
                    self.server, self.root
                ),
            )
        })?;
        let init = socket
            .request::<lsp_types::request::Initialize>(InitializeParams {
                workspace_folders: Some(vec![WorkspaceFolder {
                    uri: root_uri.clone(),
                    name: "root".to_owned(),
                }]),
                #[allow(deprecated)]
                root_uri: Some(root_uri),
                capabilities: ClientCapabilities {
                    window: Some(WindowClientCapabilities {
                        work_done_progress: Some(true),
                        ..WindowClientCapabilities::default()
                    }),
                    ..ClientCapabilities::default()
                },
                ..InitializeParams::default()
            })
            .await
            .map_err(|e| {
                LspError::new(
                    LspErrorKind::ConnectFailed,
                    format!("lsp server {}: initialize failed: {e}", self.server),
                )
            })?;
        log::info!(
            "[lsp] server {} initialized: {:?}",
            self.server,
            init.capabilities
        );
        socket.notify::<lsp_types::notification::Initialized>(InitializedParams {})?;

        // 索引门：等 RA 系 End token；超时放行（非 RA server 无此 token）。
        // 等待上限取 min(60s, startup_timeout_secs)，与 initialize 已耗时间无关。
        let gate_wait =
            Duration::from_secs(60.min(self.server_cfg.startup_timeout_secs.clamp(1, 600)));
        match tokio::time::timeout(gate_wait, indexed_rx).await {
            Ok(Ok(())) => log::info!("[lsp] server {}: index gate passed", self.server),
            _ => log::info!(
                "[lsp] server {}: index gate skipped (no RA progress token within {gate_wait:?})",
                self.server
            ),
        }

        if self.gate.load(Ordering::Relaxed) {
            socket.emit(StopLoop).ok();
            driver.abort();
            return Err(LspError::new(
                LspErrorKind::Shutdown,
                format!(
                    "lsp server {}: daemon shut down during connect; connection discarded",
                    self.server
                ),
            ));
        }
        self.store_connected(socket, driver, child, pgid);
        Ok(())
    }

    fn store_connected(
        self: &Arc<Self>,
        socket: ServerSocket,
        driver: JoinHandle<()>,
        child: Box<dyn process_wrap::tokio::ChildWrapper>,
        pgid: u32,
    ) {
        // L-07：pgid 与 session/driver 同批落地，且只属于**这条**连接
        // （`(server, root)` 双键的那一格）。清扫入口 `sweep_group` 因此不可能
        // 拿到别人的进程组 id。写在取 state 锁之前：锁序保持 state → pgid 单向，
        // 与 `mark_crashed`（先放 state guard 再 sweep）一致，不新增嵌套边。
        *self
            .pgid
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(pgid);
        {
            let mut state = self.lock_state();
            state.driver = Some(driver);
            state.session = Some(Arc::new(tokio::sync::Mutex::new(ServerSession {
                socket,
                _child: child,
                opened: BTreeMap::new(),
            })));
            state.connected = true;
            state.cooling_until = None;
            state.idle_since = Some(Instant::now());
        }
        self.ensure_watchdog();
    }

    /// 会话句柄快照（请求路径用；None = 未连接）。
    pub(crate) fn session_snapshot(&self) -> Option<Arc<tokio::sync::Mutex<ServerSession>>> {
        self.lock_state().session.clone()
    }

    /// 标记 crash（mainloop 退出/IO 错时调用）：清会话 + 组杀兜底。
    /// 下次调用单次重启（冷却只在重启失败后设，mcp 同款语义）。
    pub(crate) fn mark_crashed(&self) {
        {
            let mut state = self.lock_state();
            state.session = None;
            state.connected = false;
            state.idle_since = None;
        }
        self.sweep_group();
        log::warn!(
            "[lsp] server {} marked crashed; next call will reconnect",
            self.server
        );
    }

    /// 组杀兜底清扫（Unix；mcp sweep_group 同款）。
    fn sweep_group(&self) {
        let pid = self
            .pgid
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        #[cfg(unix)]
        if let Some(pgid) = pid {
            // SAFETY: pgid 来自本连接 spawn 的独立进程组，不含 daemon 自身。
            unsafe {
                libc::kill(-(pgid as libc::pid_t), libc::SIGKILL);
            }
        }
        #[cfg(not(unix))]
        let _ = pid;
    }

    fn ensure_watchdog(self: &Arc<Self>) {
        let mut slot = self
            .watchdog
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // L-09 随附（对齐 mcp `ensure_watchdog`）：只看 `is_some()` 会把**已结束**
        // 的上一代句柄当成活着——回收后重连的连接从此再无 idle 巡检。判据换成
        // `is_finished()`：任务自己退出（回收完成 / 断连 / 关停闸）后必须能重拉。
        let respawn = match slot.as_ref() {
            Some(handle) => handle.is_finished(),
            None => true,
        };
        if !respawn {
            return;
        }
        let weak: Weak<Self> = Arc::downgrade(self);
        let tick = self.settings.idle_tick;
        *slot = Some(tokio::spawn(async move {
            loop {
                tokio::time::sleep(tick).await;
                let Some(this) = weak.upgrade() else { return };
                if this.gate.load(Ordering::Relaxed) {
                    return;
                }
                let idle_secs = this.idle_secs;
                if idle_secs == 0 {
                    continue;
                }
                let should_drop = {
                    let state = this.lock_state();
                    !state.connected
                        || state.inflight != 0
                        || state.session.is_none()
                        || state
                            .idle_since
                            .is_none_or(|since| since.elapsed() < Duration::from_secs(idle_secs))
                };
                if should_drop {
                    continue;
                }
                log::info!(
                    "[lsp] server {} idle for {idle_secs}s; recycling",
                    this.server
                );
                this.shutdown().await;
                // L-09：回收完成即退出，下一次连接经 `ensure_watchdog` 重新拉起
                // （mcp `spawn_watchdog_task` 同款）。旧实现继续 loop，任务永远
                // 挂在槽里 ⇒ 配合上面的 `is_some()` 判据，重连后无人巡检；
                // 而 Shutdown 无硬顶时这个 loop 本身就是永久挂起的宿主。
                return;
            }
        }));
    }

    /// 优雅关闭：限时 graceful 往返 → 停 mainloop → 释放会话 → 组杀兜底。
    ///
    /// L-09：`Shutdown` 请求**必须**有硬顶（消费 `settings.close_timeout`，此前
    /// 该字段全仓无人读取）。LSP server 卡死（rust-analyzer 索引挂起是现实场景）
    /// 时旧实现 `socket.request::<Shutdown>().await` 永不返回，后果是三重的：
    /// ① idle 看门狗挂在 `shutdown().await` 上，旧子进程句柄扣在
    ///    `Arc<Mutex<ServerSession>>` 里 ⇒ 进程泄漏且重连又 spawn 一个；
    /// ② lsp 热重载器（daemon 主 runtime 任务）对 updated/removed server 调
    ///    `conn.shutdown().await` ⇒ 该任务永久卡死，后续 `[lsp]` 配置变更全废；
    /// ③ `shutdown_global()` 接线后（L-08）daemon 退出的 `block_on` 一起挂死。
    ///
    /// 超时不重试优雅路径：`emit(StopLoop)` + `abort` driver 是**控制面**动作，
    /// 拿的是 `ConnState.driver`（因此不在 session mutex 里），无条件做得到——
    /// 否则只是把死等从 Shutdown 请求挪到锁上。最后照旧 `sweep_group` 兜底；
    /// 会话 Arc 在本方法结束时放手，在途请求持有的最后一份 drop 时由子进程
    /// kill 链收尾（mcp `close_with_timeout` 同款降级收割语义：超时是收割成功，
    /// 不是关闭失败）。
    pub async fn shutdown(&self) {
        let (session, driver) = {
            let mut state = self.lock_state();
            state.connected = false;
            (state.session.take(), state.driver.take())
        };
        if let Some(session) = session {
            let close_timeout = self.settings.close_timeout;
            let server = self.server.clone();
            let graceful = async move {
                let mut guard = session.lock().await;
                // 先发 shutdown+exit（best-effort），再停 loop。
                let socket = guard.socket.clone();
                socket
                    .request::<lsp_types::request::Shutdown>(())
                    .await
                    .ok();
                socket.notify::<lsp_types::notification::Exit>(()).ok();
                socket.emit(StopLoop).ok();
                guard.opened.clear();
            };
            if tokio::time::timeout(close_timeout, graceful).await.is_err() {
                log::warn!(
                    "[lsp] server {server} graceful shutdown exceeded {close_timeout:?}; \
                     hard-stopping mainloop driver"
                );
            }
        }
        // 无论优雅轮次是否跑完，driver 都必须拆掉（句柄已随上面的 take 离开
        // session mutex 的管辖）。
        if let Some(driver) = driver {
            driver.abort();
        }
        self.sweep_group();
    }
}

/// 调用守卫（mcp CallGuard 同款：drop 时 inflight -1 + idle_since 刷新）。
pub struct CallGuard<'a> {
    conn: &'a ServerConnection,
}

impl Drop for CallGuard<'_> {
    fn drop(&mut self) {
        let mut state = self.conn.lock_state();
        state.inflight = state.inflight.saturating_sub(1);
        if state.inflight == 0 {
            state.idle_since = Some(Instant::now());
        }
    }
}

/// 索引门 token（RA 新老两代命名；async-lsp client_builder.rs 实测）。
fn is_index_end_token(token: &str) -> bool {
    matches!(token, "rustAnalyzer/Indexing" | "rustAnalyzer/cachePriming")
}

struct ClientGate {
    indexed_tx: Option<oneshot::Sender<()>>,
}

struct StopLoop;

impl async_lsp::LanguageClient for ClientGate {
    type Error = async_lsp::ResponseError;
    type NotifyResult = ControlFlow<async_lsp::Result<()>>;
}

#[cfg(test)]
mod lifecycle_tests {
    //! v4.4 `P1-0c` 反证守卫（宪法 #18）：连接层两项资源回收缺陷必须真的闭口。
    //!
    //! - **L-07**：pgid 只能跟着**本连接自己 spawn 的那一代**进程走。旧实现把它
    //!   塞进按 server 名键控的全局表，双 root 并发连同名 server 时后来者覆盖
    //!   前者，先关的一方 `sweep_group` 会对**别人的**进程组 SIGKILL（旧代码上
    //!   临时用例实测 `take` 返回 `Some(202)` 而非 `Some(101)`）。
    //! - **L-09**：`Shutdown` 请求受 `settings.close_timeout` 硬顶。server 永不
    //!   回包时 `shutdown()` 仍必须有界返回并走完组杀兜底；旧实现此处 `await`
    //!   无顶，idle 看门狗、lsp 热重载器与接线后的 daemon 退出路径一起挂死。
    #![allow(clippy::unwrap_used)] // 测试代码豁免（仓库惯例）

    use std::future;

    use qaqh_config::config::LspServerConfig;
    use qaqh_config::secrets::SecretStore;
    use tokio_util::compat::TokioAsyncReadCompatExt;

    use super::*;

    fn test_cfg() -> LspServerConfig {
        LspServerConfig {
            command: "rust-analyzer".to_owned(),
            args: vec![],
            env: BTreeMap::new(),
            extensions: vec!["rs".to_owned()],
            startup_timeout_secs: 5,
            default_timeout_secs: 5,
        }
    }

    fn settings(close_timeout: Duration) -> LifecycleSettings {
        LifecycleSettings {
            connect_timeout: Duration::from_secs(1),
            reconnect_cooldown: Duration::from_millis(50),
            close_timeout,
            idle_tick: Duration::from_millis(20),
        }
    }

    fn conn(server: &str, root: &std::path::Path, close_timeout: Duration) -> Arc<ServerConnection> {
        Arc::new(ServerConnection::new(
            server,
            root.to_string_lossy().into_owned(),
            test_cfg(),
            // idle_secs = 0：用例只走显式关闭路径，不让看门狗参与时序判定。
            0,
            settings(close_timeout),
            Arc::new(AtomicBool::new(false)),
            SecretStore::default_location(),
        ))
    }

    fn pgid(conn: &ServerConnection) -> Option<u32> {
        *conn
            .pgid
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 真实 spawn 一个与 LSP 无关的短命子进程：只为拿到两个互不相同的**真 pid**
    /// 与本代 child 句柄（协议面由内存双工的假对端负责，不需要真 server）。
    fn spawn_trivial_child(root: &std::path::Path) -> (Box<dyn process_wrap::tokio::ChildWrapper>, u32) {
        #[cfg(windows)]
        let (command, args) = (
            "cmd",
            vec!["/C".to_owned(), "exit".to_owned()],
        );
        #[cfg(not(windows))]
        let (command, args) = (
            "sh",
            vec!["-c".to_owned(), "exit 0".to_owned()],
        );
        let cfg = LspServerConfig {
            command: command.to_owned(),
            args,
            env: BTreeMap::new(),
            extensions: vec!["rs".to_owned()],
            startup_timeout_secs: 5,
            default_timeout_secs: 5,
        };
        let wrap = adapter::build_stdio_command(&cfg, &root.to_string_lossy());
        let spawned = adapter::spawn_server(wrap)
            .unwrap_or_else(|error| panic!("trivial child must spawn: {}", error.message));
        let pid = spawned.pid;
        (spawned.child, pid)
    }

    struct NullClient;

    impl async_lsp::LanguageClient for NullClient {
        type Error = async_lsp::ResponseError;
        type NotifyResult = ControlFlow<async_lsp::Result<()>>;
    }

    /// 一条**永不回包**的 client socket：对端只把字节读干净（防写侧堵住），
    /// 永不生成响应——正是 rust-analyzer 索引挂起时 `Shutdown` 的遭遇。
    async fn stalled_socket() -> (ServerSocket, JoinHandle<()>) {
        use futures::AsyncReadExt;

        let (peer, client_stream) = tokio::io::duplex(64 << 10);
        let (client_main, socket) = MainLoop::new_client(|_server| {
            ServiceBuilder::new().service(Router::new(NullClient))
        });
        let (client_rx, client_tx) = client_stream.compat().split();
        let driver = tokio::spawn(async move {
            let _ = client_main.run_buffered(client_rx, client_tx).await;
        });
        tokio::spawn(async move {
            let mut sink = [0u8; 4096];
            let mut peer = peer.compat();
            loop {
                if peer.read(&mut sink).await.unwrap_or(0) == 0 {
                    // 对端读完即 EOF：挂住不退出，保持 write 半侧活着。
                    future::pending::<()>().await;
                }
            }
        });
        (socket, driver)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_roots_of_one_server_name_own_their_own_pgid() {
        let root_a = tempfile::tempdir().unwrap();
        let root_b = tempfile::tempdir().unwrap();
        let a = conn("rust", root_a.path(), Duration::from_millis(250));
        let b = conn("rust", root_b.path(), Duration::from_millis(250));
        let (child_a, pid_a) = spawn_trivial_child(root_a.path());
        let (child_b, pid_b) = spawn_trivial_child(root_b.path());
        assert_ne!(pid_a, pid_b, "fixture 必须交出两个不同的真 pid");

        let (socket_a, driver_a) = stalled_socket().await;
        a.store_connected(socket_a, driver_a, child_a, pid_a);
        let (socket_b, driver_b) = stalled_socket().await;
        b.store_connected(socket_b, driver_b, child_b, pid_b);

        // 旧实现两条连接都从同一把 `SPAWN_PIDS["rust"]` 取号，先连的一方拿到
        // 后 spawn 那个 root 的 pid —— 下面第一条断言当场红。
        assert_eq!(
            pgid(&a),
            Some(pid_a),
            "L-07: root-a 的组杀兜底必须只认自己 spawn 的进程组 id"
        );
        assert_eq!(pgid(&b), Some(pid_b), "L-07: root-b 同理");

        // 清扫的语义是「取走自己的」，不是「取走这个名字下的」。
        a.sweep_group();
        assert_eq!(pgid(&a), None, "清扫后本连接不得再持有 pgid");
        assert_eq!(
            pgid(&b),
            Some(pid_b),
            "L-07: A 的清扫不得动走 B 的 pgid（旧实现里 A 拿走的正是 B 的 pid，\
             于是 SIGKILL 打到 B 的活进程组）"
        );
        b.sweep_group();
        assert_eq!(pgid(&b), None);

        // 收尾：两条连接都显式关闭（拆 driver 走控制面句柄），不给测试运行时
        // 留挂着的 mainloop 任务。
        a.shutdown().await;
        b.shutdown().await;
        assert_eq!(a.status(), ConnStatus::Disconnected);
        assert_eq!(b.status(), ConnStatus::Disconnected);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_is_bounded_by_close_timeout_when_server_never_replies() {
        let close_timeout = Duration::from_millis(250);
        let root = tempfile::tempdir().unwrap();
        let c = conn("rust", root.path(), close_timeout);
        let (child, pid) = spawn_trivial_child(root.path());
        let (socket, driver) = stalled_socket().await;
        c.store_connected(socket, driver, child, pid);
        assert_eq!(c.status(), ConnStatus::Connected { inflight: 0 });

        let started = Instant::now();
        // 外层 5s 是「永久挂起」的探测兜底：旧实现在这里必然被它抓住（红）。
        let closer = {
            let c = Arc::clone(&c);
            tokio::time::timeout(Duration::from_secs(5), async move {
                c.shutdown().await;
            })
        };
        let reclaimed = closer.await;
        let elapsed = started.elapsed();
        assert!(
            reclaimed.is_ok(),
            "L-09: server 不回 Shutdown 时旧实现永久挂起，watchdog / lsp 热重载 / \
             daemon 退出路径一起被钉死（5s 兜底已触发）"
        );
        assert!(
            elapsed >= close_timeout,
            "用例必须真的等满 close_timeout 才降级——否则说明 Shutdown 其实有人应答，\
             反证装置失效: {elapsed:?}"
        );
        assert!(
            elapsed < close_timeout * 6,
            "降级收割必须有界（graceful + hard 各一个 close_timeout），实测 {elapsed:?}"
        );
        assert_eq!(pgid(&c), None, "超时路径同样必须走完组杀兜底");
        assert_eq!(c.status(), ConnStatus::Disconnected);
    }

    #[test]
    fn begin_call_gates_disconnected_and_shutting_down() {
        // 未连接：拒，并用与既有取句柄路径相同的 `ServerCrashed` 词表
        // （调用方据此 mark_crashed，下次调用自动重连）。
        let gate = Arc::new(AtomicBool::new(false));
        let c = ServerConnection::new(
            "rust",
            "/tmp/qaqh-lsp-begin-call",
            test_cfg(),
            0,
            settings(Duration::from_millis(250)),
            gate,
            SecretStore::default_location(),
        );
        let error = c
            .begin_call()
            .err()
            .expect("未连接不得把 inflight 记到死连接上");
        assert_eq!(error.kind, LspErrorKind::ServerCrashed);

        // 关停闸已落：稳定码 `lsp_shutdown`（对齐 mcp begin_call）。
        let gate = Arc::new(AtomicBool::new(true));
        let c = ServerConnection::new(
            "rust",
            "/tmp/qaqh-lsp-begin-call",
            test_cfg(),
            0,
            settings(Duration::from_millis(250)),
            gate,
            SecretStore::default_location(),
        );
        let error = c
            .begin_call()
            .err()
            .expect("daemon 关停中不得接受新调用");
        assert_eq!(error.kind, LspErrorKind::Shutdown);
    }
}
