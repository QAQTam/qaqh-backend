//! High-level Ringing V1 client: discovery + open + three SSE channels + lease
//! renewal + commands/queries/bootstrap/stop.
//!
//! The client owns a global tokio runtime and runs all transport tasks in the
//! background; the shell receives events through callbacks (which must marshal
//! to the UI thread themselves) and calls the async methods for commands.

use std::sync::Arc;

use serde_json::Value;
use tokio::sync::{watch, Mutex};

use crate::discovery::{read_discovery, DaemonDiscovery};
use crate::error::{ClientError, Result};
use crate::session::{RingingSession, SessionState};
use crate::sse::{ChannelStream, StreamHandlers};
use crate::timeline::TimelineStream;
use crate::types::{
    Channel, ChannelStatus, CommandReceipt, CommandRequest, EventBatch, TimelineEntry,
    TimelineStatus,
};

/// Callbacks delivered on the client's background tasks.
#[derive(Clone)]
pub struct ClientHandlers {
    pub on_batch: std::sync::Arc<dyn Fn(EventBatch) + Send + Sync>,
    pub on_status: std::sync::Arc<dyn Fn(Channel, ChannelStatus) + Send + Sync>,
    pub on_reset: Option<std::sync::Arc<dyn Fn(crate::types::ResetRequired) + Send + Sync>>,
    /// Per-session timeline entry (seed, entry).
    pub on_timeline_entry: std::sync::Arc<dyn Fn(String, TimelineEntry) + Send + Sync>,
    pub on_timeline_status: std::sync::Arc<dyn Fn(TimelineStatus) + Send + Sync>,
    /// Fresh timeline snapshot pushed on gap recovery.
    pub on_timeline_snapshot: std::sync::Arc<dyn Fn(serde_json::Value) + Send + Sync>,
}

pub struct ClientOptions {
    pub handlers: ClientHandlers,
    /// Spawn `deepx-daemon run` when no discovery file exists yet.
    pub launch_daemon_if_missing: bool,
    /// Path to the daemon executable (default: `target/debug/deepx-daemon(.exe)`
    /// relative to `DEEPX_BACKEND_ROOT` or the workspace root).
    pub daemon_path: Option<std::path::PathBuf>,
    /// Maximum time to wait for the daemon to publish discovery.
    pub start_timeout: std::time::Duration,
}

impl Default for ClientHandlers {
    fn default() -> Self {
        Self {
            on_batch: std::sync::Arc::new(|_| {}),
            on_status: std::sync::Arc::new(|_, _| {}),
            on_reset: None,
            on_timeline_entry: std::sync::Arc::new(|_, _| {}),
            on_timeline_status: std::sync::Arc::new(|_| {}),
            on_timeline_snapshot: std::sync::Arc::new(|_| {}),
        }
    }
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            handlers: ClientHandlers::default(),
            launch_daemon_if_missing: false,
            daemon_path: None,
            start_timeout: std::time::Duration::from_secs(8),
        }
    }
}

/// Outcome of `POST /control/v1/stop` / `stop-if-idle`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopStatus {
    Stopping,
    Busy,
    Unsupported,
}

/// A connected Ringing V1 client. Cloneable handle; `close()` stops all tasks.
#[derive(Clone)]
pub struct Client {
    inner: Arc<ClientInner>,
}

struct ClientInner {
    base_url: String,
    token: String,
    http: reqwest::Client,
    session: Arc<RingingSession>,
    handlers: ClientHandlers,
    stop_tx: watch::Sender<bool>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// Active per-session timeline stream (activated on demand).
    timeline: Mutex<Option<TimelineHandle>>,
}

/// Bookkeeping for the currently activated timeline stream.
struct TimelineHandle {
    stop_tx: watch::Sender<bool>,
    status: watch::Sender<Option<TimelineStatus>>,
}

impl Client {
    /// Connect using the discovery file, optionally launching the daemon first.
    ///
    /// This blocks the calling thread until open negotiation completes (or the
    /// start timeout elapses). For UI threads, call it from a worker thread.
    pub fn connect(options: ClientOptions) -> Result<Client> {
        let runtime = runtime();
        runtime.block_on(Self::connect_async(options))
    }

    /// Async variant for callers that already own a runtime.
    pub async fn connect_async(options: ClientOptions) -> Result<Client> {
        // 只接受"pid 存活"的 discovery：残留的 daemon.json（daemon 被强杀
        // 后遗留）会导致直连死端口（connection refused），此前仅检查文件
        // 存在与否。pid 已死的 discovery 视为缺失，走拉起路径（新 daemon
        // 启动时经单实例锁清理 stale lock/discovery 自愈）。
        let (discovery, launched) = match read_discovery()
            .ok()
            .filter(|d| crate::discovery::process_is_running(d.pid))
        {
            Some(d) => (d, false),
            None => {
                if options.launch_daemon_if_missing {
                    log::info!("[deepx-client] no live daemon discovery; launching daemon");
                    (
                        wait_for_daemon(options.daemon_path.as_deref(), options.start_timeout)
                            .await?,
                        true,
                    )
                } else {
                    return Err(ClientError::Discovery(
                        "no live daemon discovery (daemon.json missing or stale)".into(),
                    ));
                }
            }
        };
        let _ = launched;

        let base_url = discovery.base_url()?;
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .build()?;
        let session = Arc::new(RingingSession::new(base_url.clone(), discovery.token.clone(), http.clone()));

        // Open negotiation (single lease; SSE streams and commands share it).
        session.open().await?;

        let (stop_tx, stop_rx) = watch::channel(false);
        // (server_epoch, client_session_id) shared with channel streams.
        // Subscribe to the session's live ctx: renewal failure triggers a
        // re-negotiation (new lease) that broadcasts a fresh value here,
        // so reconnecting streams never pin a stale expired session.
        let ctx_rx = session.session_ctx_rx();

        let tasks = Mutex::new(Vec::new());
        let client = Client {
            inner: Arc::new(ClientInner {
                base_url: base_url.clone(),
                token: discovery.token.clone(),
                http: http.clone(),
                session: session.clone(),
                handlers: options.handlers.clone(),
                stop_tx,
                tasks,
                timeline: Mutex::new(None),
            }),
        };

        // Lease renewal.
        let renewal = {
            let session = session.clone();
            let stop = stop_rx.clone();
            tokio::spawn(async move { session.run_renewal(stop).await })
        };
        client.push_task(renewal).await;

        // Three SSE channels.
        for channel in Channel::ALL {
            let stream = ChannelStream::new(
                format!("{base_url}/ringing/v1/events/{}", channel.as_str()),
                discovery.token.clone(),
                channel,
                http.clone(),
                StreamHandlers {
                    on_batch: options.handlers.on_batch.clone(),
                    on_status: {
                        let channel = channel;
                        let cb = options.handlers.on_status.clone();
                        std::sync::Arc::new(move |status| cb(channel, status))
                    },
                    on_reset: options.handlers.on_reset.clone(),
                },
                ctx_rx.clone(),
            );
            let stop = stop_rx.clone();
            let task = tokio::spawn(async move {
                let mut stream = stream;
                stream.run(stop).await;
            });
            client.push_task(task).await;
        }

        Ok(client)
    }

    async fn push_task(&self, task: tokio::task::JoinHandle<()>) {
        self.inner.tasks.lock().await.push(task);
    }

    /// Current negotiated session state.
    pub async fn session_state(&self) -> Option<SessionState> {
        self.inner.session.state().await
    }

    /// `POST /ringing/v1/commands/{channel}` with the shared lease identity.
    ///
    /// `seed` may be `None` only for `SessionCreate`-style commands.
    pub async fn command(
        &self,
        channel: Channel,
        seed: Option<String>,
        command_id: String,
        command: Value,
        expected_revision: Option<u64>,
    ) -> Result<Value> {
        let state = self
            .inner
            .session
            .state()
            .await
            .ok_or_else(|| ClientError::Negotiation("session not open".into()))?;
        let mut command = command;
        // The daemon's RingingCommand is internally tagged by `channel`
        // (`#[serde(tag = "channel")]`); the wire envelope also carries a
        // top-level channel. Mirror Electron main, which aligns both at the
        // preload boundary.
        if let Some(obj) = command.as_object_mut() {
            obj.insert("channel".into(), serde_json::json!(channel.as_str()));
        }
        let payload = CommandRequest {
            schema: "deepx.Ringing",
            version: 1,
            channel: channel.as_str(),
            command_id,
            client_instance_id: state.client_instance_id.clone(),
            client_session_id: state.client_session_id.clone(),
            seed,
            expected_revision,
            command,
        };
        let path = format!("/ringing/v1/commands/{}", channel.as_str());
        let session_id = self.session_id_header().await?;
        let response = self
            .inner
            .http
            .post(format!("{}{path}", self.inner.base_url))
            .bearer_auth(&self.inner.token)
            .header("X-DeepX-Client-Session-Id", session_id)
            .json(&payload)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(ClientError::Http {
                status: response.status().as_u16(),
                path,
            });
        }
        Ok(response.json().await?)
    }

    /// `GET /ringing/v1/commands/{command_id}` — resolve post-acceptance uncertainty.
    pub async fn command_status(&self, command_id: &str) -> Result<CommandReceipt> {
        let path = format!("/ringing/v1/commands/{}", command_id);
        let session_id = self.session_id_header().await?;
        let response = self
            .inner
            .http
            .get(format!("{}{path}", self.inner.base_url))
            .bearer_auth(&self.inner.token)
            .header("X-DeepX-Client-Session-Id", session_id)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(ClientError::Http {
                status: response.status().as_u16(),
                path,
            });
        }
        Ok(response.json().await?)
    }

    /// `POST /ringing/v1/queries/{name}` — typed query.
    pub async fn query(&self, name: &str, params: Value) -> Result<Value> {
        let session_id = self.session_id_header().await?;
        let path = format!("/ringing/v1/queries/{name}");
        let response = self
            .inner
            .http
            .post(format!("{}{path}", self.inner.base_url))
            .bearer_auth(&self.inner.token)
            .header("X-DeepX-Client-Session-Id", session_id)
            .json(&params)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(ClientError::Http {
                status: response.status().as_u16(),
                path,
            });
        }
        Ok(response.json().await?)
    }

    /// `GET /ringing/v1/sessions/{seed}/bootstrap` — authoritative snapshot.
    pub async fn bootstrap(&self, seed: &str) -> Result<Value> {
        let session_id = self.session_id_header().await?;
        let path = format!("/ringing/v1/sessions/{seed}/bootstrap");
        let response = self
            .inner
            .http
            .get(format!("{}{path}", self.inner.base_url))
            .bearer_auth(&self.inner.token)
            .header("X-DeepX-Client-Session-Id", session_id)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(ClientError::Http {
                status: response.status().as_u16(),
                path,
            });
        }
        Ok(response.json().await?)
    }

    /// `POST /ringing/v1/actions/{name}` — connection-level auxiliary action
    /// (git/workspace/config/skills/plan/todo etc). Mirrors the Electron
    /// `ringingManager.action` payload (action_id + sha256 fingerprint).
    pub async fn action(&self, name: &str, mut params: Value) -> Result<Value> {
        let session_id = self.session_id_header().await?;
        let action_id = uuid::Uuid::new_v4().to_string();
        let fingerprint = {
            let mut hasher = <sha2::Sha256 as sha2::Digest>::new();
            use sha2::Digest;
            hasher.update(serde_json::to_string(&serde_json::json!({
                "method": name,
                "params": params,
            }))?);
            let digest = hasher.finalize();
            digest.iter().map(|b| format!("{b:02x}")).collect::<String>()
        };
        if let Some(obj) = params.as_object_mut() {
            obj.insert("action_id".into(), serde_json::json!(action_id));
            obj.insert("fingerprint".into(), serde_json::json!(fingerprint));
        } else {
            params = serde_json::json!({
                "action_id": action_id,
                "fingerprint": fingerprint,
            });
        }
        let path = format!("/ringing/v1/actions/{name}");
        let response = self
            .inner
            .http
            .post(format!("{}{path}", self.inner.base_url))
            .bearer_auth(&self.inner.token)
            .header("X-DeepX-Client-Session-Id", session_id)
            .json(&params)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(ClientError::Http {
                status: response.status().as_u16(),
                path,
            });
        }
        Ok(response.json().await?)
    }

    /// Attach a session seed to this client session (Ringing v1 semantics:
    /// `session_resume` on the control channel — the daemon records the seed
    /// ownership so subsequent seed-scoped commands are accepted). The seed
    /// is carried both in the envelope and in the command body (validate
    /// requires a non-empty envelope seed for every command except create).
    pub async fn attach(&self, seed: &str) -> Result<Value> {
        self.command(
            Channel::Control,
            Some(seed.to_string()),
            uuid::Uuid::new_v4().to_string(),
            serde_json::json!({ "type": "session_resume", "seed": seed }),
            None,
        )
        .await
    }

    /// Activate the native timeline for one session (mirrors Electron
    /// `ringingManager.activateTimeline`): fetch the authoritative snapshot,
    /// replace any previous timeline stream with a new one seeded at the
    /// snapshot watermark, and return the snapshot. The seed must have been
    /// attached first (`backend.attach` / `session_resume`), otherwise the
    /// daemon rejects the request with 401.
    pub async fn activate_timeline(&self, seed: &str) -> Result<Value> {
        if seed.is_empty() {
            return Err(ClientError::Negotiation("seed is required".into()));
        }
        let state = self
            .inner
            .session
            .state()
            .await
            .ok_or_else(|| ClientError::Negotiation("session not open".into()))?;
        let path = format!("/ringing/v1/sessions/{seed}/timeline");
        let response = self
            .inner
            .http
            .get(format!("{}{path}", self.inner.base_url))
            .bearer_auth(&self.inner.token)
            .header("X-DeepX-Client-Session-Id", &state.client_session_id)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(ClientError::Http {
                status: response.status().as_u16(),
                path,
            });
        }
        let snapshot: Value = response.json().await?;
        if snapshot.get("schema").and_then(|v| v.as_str()) != Some("deepx.Ringing")
            || snapshot.get("version").and_then(|v| v.as_u64()) != Some(1)
            || snapshot.get("seed").and_then(|v| v.as_str()) != Some(seed)
            || snapshot
                .get("snapshot")
                .and_then(|s| s.get("watermark"))
                .and_then(|w| w.as_u64())
                .is_none()
        {
            return Err(ClientError::Protocol(
                "invalid Ringing V1 timeline snapshot".into(),
            ));
        }
        let watermark = snapshot["snapshot"]["watermark"].as_u64().unwrap_or(0);

        // Replace any previous timeline stream (one transcript at a time).
        let mut guard = self.inner.timeline.lock().await;
        if let Some(prev) = guard.take() {
            let _ = prev.stop_tx.send(true);
            let _ = prev.status.send_replace(Some(TimelineStatus::Closed {
                seed: seed.to_string(),
                reason: "session changed".into(),
            }));
        }
        let (stop_tx, stop_rx) = watch::channel(false);
        let (status_tx, _status_rx) = watch::channel(None);
        let seed_owned = seed.to_string();
        let mut stream = TimelineStream::new(
            self.inner.base_url.clone(),
            self.inner.token.clone(),
            seed_owned.clone(),
            self.inner.http.clone(),
            self.inner.session.clone(),
            self.inner.handlers.on_timeline_entry.clone(),
            self.inner.handlers.on_timeline_status.clone(),
            self.inner.handlers.on_timeline_snapshot.clone(),
            watermark,
            Some(status_tx.clone()),
        );
        let task_status_tx = status_tx.clone();
        let session_stop = self.inner.stop_tx.subscribe();
        let task = tokio::spawn(async move {
            stream.run(stop_rx, session_stop).await;
            let _ = task_status_tx.send_replace(Some(TimelineStatus::Closed {
                seed: seed_owned,
                reason: "stream ended".into(),
            }));
        });
        self.push_task(task).await;
        *guard = Some(TimelineHandle {
            stop_tx,
            status: status_tx,
        });
        drop(guard);

        // Mirror Electron: the activate response is both returned to the
        // caller and pushed as a snapshot event so listeners rebuild the
        // transcript immediately.
        (self.inner.handlers.on_timeline_snapshot)(snapshot.clone());
        Ok(snapshot)
    }

    /// Current timeline connection status (`None` when never activated).
    pub async fn timeline_status(&self) -> Option<TimelineStatus> {
        let guard = self.inner.timeline.lock().await;
        guard
            .as_ref()
            .and_then(|handle| handle.status.borrow().clone())
    }

    /// Current client session id for request headers.
    async fn session_id_header(&self) -> Result<String> {
        self.inner
            .session
            .state()
            .await
            .map(|s| s.client_session_id)
            .ok_or_else(|| ClientError::Negotiation("session not open".into()))
    }

    /// `POST /control/v1/stop` / `stop-if-idle` — graceful daemon stop.
    pub async fn stop_daemon(&self, idle_only: bool) -> Result<StopStatus> {
        let path = if idle_only { "/control/v1/stop-if-idle" } else { "/control/v1/stop" };
        let response = self
            .inner
            .http
            .post(format!("{}{path}", self.inner.base_url))
            .bearer_auth(&self.inner.token)
            .send()
            .await?;
        match response.status().as_u16() {
            200 => Ok(StopStatus::Stopping),
            409 => Ok(StopStatus::Busy),
            _ => Ok(StopStatus::Unsupported),
        }
    }

    /// Stop all background tasks (SSE streams + renewal).
    pub fn close(&self) {
        let _ = self.inner.stop_tx.send(true);
    }
}

/// Global tokio runtime handle for shells that need to run client futures
/// from non-async contexts (e.g. the WinUI bridge).
pub fn runtime_handle() -> tokio::runtime::Handle {
    runtime().handle().clone()
}

/// Global tokio runtime shared by all clients in this process.
fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("deepx-client")
            .build()
            .expect("failed to build deepx-client runtime")
    })
}

/// Spawn the daemon (`deepx-daemon run`) and wait for its discovery file.
///
/// 进程内 spawn 串行化：并发 `connect_async`（壳首屏多个 invoke 同时触发）
/// 各自进入本函数时，仅第一个执行「检查 + spawn」决策，其余等待锁后重新
/// 检查——发现 lock/discovery 已就绪则不再 spawn，杜绝并发 spawn 多个
/// daemon 实例（双 daemon 并存触发源）。
static DAEMON_SPAWN_GUARD: std::sync::OnceLock<tokio::sync::Mutex<()>> =
    std::sync::OnceLock::new();

async fn wait_for_daemon(
    daemon_path: Option<&std::path::Path>,
    timeout: std::time::Duration,
) -> Result<DaemonDiscovery> {
    let executable = daemon_path
        .map(|p| p.to_path_buf())
        .unwrap_or_else(default_daemon_path);
    // 串行化 spawn 决策（临界区只做文件检查 + spawn，很快）。
    let guard = DAEMON_SPAWN_GUARD
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    // 已有存活 daemon（discovery pid 存活 或 lock 持有者存活）时不重复
    // spawn：daemon 冷启动窗口内 lock 先行发布、discovery 延迟——lock
    // 持有者活着即意味着有实例正在初始化，直接轮询等待其发布即可。
    let live = read_discovery()
        .ok()
        .filter(|d| crate::discovery::process_is_running(d.pid));
    if live.is_none() && !crate::discovery::lock_holder_alive() {
        log::info!("[deepx-client] spawning daemon: {}", executable.display());
        spawn_detached(executable.as_ref())?;
    }
    drop(guard);

    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match read_discovery() {
            // 同样要求 pid 存活：spawn 前残留的 stale discovery 不得被
            // 当作新 daemon 的就绪信号（旧 pid 已死）。
            Ok(d) if crate::discovery::process_is_running(d.pid) => return Ok(d),
            Ok(_) | Err(_) if tokio::time::Instant::now() >= deadline => {
                return Err(ClientError::Discovery(
                    "daemon did not publish live discovery in time".into(),
                ));
            }
            Ok(_) | Err(_) => tokio::time::sleep(std::time::Duration::from_millis(120)).await,
        }
    }
}

/// Resolve the daemon executable. 与 [`crate::discovery::daemon_executable`]
/// 的候选顺序保持一致：dev 布局（`DEEPX_BACKEND_ROOT`/cwd 的 `target/debug`）
/// → exe 旁 `resources/`（安装布局）→ exe 旁 → PATH 兜底。
///
/// 注意：此前仅支持 dev 布局，安装版在「本地映射模式下由桥首次拉起 daemon」
/// 时（`daemon.json` 不存在 → `wait_for_daemon` → 此处）会直接命中 PATH 裸名，
/// 报 `io error: program not found`。统一为 `daemon_executable` 后安装布局
/// 正确命中。
fn default_daemon_path() -> std::path::PathBuf {
    crate::discovery::daemon_executable()
}

/// Spawn a detached process (Windows: `CREATE_NEW_PROCESS_GROUP` + no console).
fn spawn_detached(executable: &std::path::Path) -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        let _ = std::process::Command::new(executable)
            .arg("run")
            .creation_flags(CREATE_NEW_PROCESS_GROUP)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new(executable)
            .arg("run")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        Ok(())
    }
}
