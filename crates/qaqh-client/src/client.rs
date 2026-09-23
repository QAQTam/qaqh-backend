//! High-level Ringing V1 client: discovery + open + three SSE channels + lease
//! renewal + commands/service-rpc/bootstrap/stop.
//!
//! The client owns a global tokio runtime and runs all transport tasks in the
//! background; the shell receives events through callbacks (which must marshal
//! to the UI thread themselves) and calls the async methods for commands.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::{Mutex, watch};

use qaqh_domain::ControlCommand;
use qaqh_ringing::{RingingCommandEnvelope, RingingCommandStatus};

use crate::discovery::{DaemonDiscovery, DiscoveryExt, read_discovery};
use crate::endpoint::{ActionRequest, QueryRequest};
use crate::error::{ClientError, Result};
use crate::session::{RingingSession, SessionState};
use crate::sse::{ChannelStream, StreamHandlers};
use crate::timeline::TimelineStream;
use crate::types::{
    CHANNELS, Channel, ChannelStatus, CommandOptions, ContentRef, EventBatch, RingingCommand,
    RingingCommandAck, RingingCommandAckStatus, TimelineEntry, TimelinePage, TimelineStatus,
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
    pub on_timeline_snapshot: std::sync::Arc<dyn Fn(TimelinePage) + Send + Sync>,
    /// **存活信号：从 socket 收到了字节**（每次读到一个 chunk 调一次）。
    ///
    /// 与其它回调的区别是它**不针对内容**——包括被解码器丢掉的 SSE 注释行
    /// （也就是 daemon 每 15s 一次的 keepalive，见 `qaqh-daemon` 的
    /// `KeepAlive::new().interval(15s)`）。keepalive 的意义本就是「我还活着」，
    /// 但这个信号此前**没有任何出口**：壳层只能看见 `on_status`，而后者只在
    /// **连接建立**时发一次。
    ///
    /// 缺了它，「前端与 daemon 失联了吗」这个问题就只能靠「连接建立了多久」来猜，
    /// 而那与「daemon 还在不在」无关——TUI 曾因此在**每个**活过 15 秒的连接上
    /// 误报失联（而且因为 keepalive 让空闲重连永不发生，它永不自愈）。
    pub on_liveness: std::sync::Arc<dyn Fn() + Send + Sync>,
}

/// 远端 daemon 的直连目标（临时跨端模式）。
///
/// 与本地模式互斥：设置后跳过 `daemon.json` discovery、pid 判活和本地
/// spawn，直接用 `base_url + token` 走 Ringing V1。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteEndpoint {
    /// `http://<ip>:<port>` 或 `https://...`（不带尾随斜杠）。
    pub base_url: String,
    /// daemon `--server` 模式下的 Bearer token。
    pub token: String,
}

pub struct ClientOptions {
    pub handlers: ClientHandlers,
    /// Spawn `qaqh-daemon run` when no discovery file exists yet.
    pub launch_daemon_if_missing: bool,
    /// Path to the daemon executable (default: `target/debug/qaqh-daemon(.exe)`
    /// relative to `QAQH_BACKEND_ROOT` or the workspace root).
    pub daemon_path: Option<std::path::PathBuf>,
    /// Maximum time to wait for the daemon to publish discovery.
    pub start_timeout: std::time::Duration,
    /// 远端直连目标；`Some` 时忽略本地 discovery / spawn。
    pub remote: Option<RemoteEndpoint>,
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
            on_liveness: std::sync::Arc::new(|| {}),
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
            remote: None,
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
    pub(crate) inner: Arc<ClientInner>,
}

pub(crate) struct ClientInner {
    pub(crate) http: reqwest::Client,
    /// 端点与 Bearer token 的唯一权威来源——**不要**在这里另存一份：
    /// daemon 重启会换掉两者，而 [`RingingSession::refresh_discovery`] 只能
    /// 更新 session 持有的那份，副本会静默变陈旧。
    session: Arc<RingingSession>,
    handlers: ClientHandlers,
    stop_tx: watch::Sender<bool>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// Active per-session timeline streams, keyed by seed (activated on demand).
    ///
    /// Keyed rather than single-slot: shells that keep more than one session
    /// live at once (multiple tabs, subagent sessions) need several concurrent
    /// timeline streams. A single slot would silently stop another session's
    /// stream mid-turn.
    timeline: Mutex<HashMap<String, TimelineHandle>>,
    /// Seeds this client has attached (BUG-2026-09-12-10): replayed after a
    /// lease re-negotiation so seed-scoped reads do not 401 with the new
    /// client_session_id.
    attached_seeds: Mutex<HashSet<String>>,
    /// Optional v2 negotiation state. Kept separate from the v1 lease so the
    /// 2.0 compatibility window can run both protocols without aliasing
    /// identities.
    pub(crate) v2_session: Mutex<Option<crate::v2::ClientV2SessionState>>,
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
        // 远端直连：跳过本地 discovery / pid 判活 / spawn，直接用端点 + token。
        let (base_url, token) = match options.remote.clone() {
            Some(remote) => {
                let base_url = remote.base_url.trim_end_matches('/').to_string();
                if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
                    return Err(ClientError::Discovery(format!(
                        "remote base_url must start with http:// or https://: {base_url}"
                    )));
                }
                (base_url, remote.token)
            }
            None => {
                // 只接受「pid 存活 **且** 端点真的在听」的 discovery：残留的
                // daemon.json（daemon 被强杀后遗留）会导致直连死端口
                // （connection refused），此前仅检查文件存在与否。判据不成立的
                // discovery 一律视为缺失，走拉起路径（新 daemon 启动时经单实例
                // 锁清理 stale lock/discovery 自愈）。
                let discovery = match read_discovery()
                    .ok()
                    .filter(crate::discovery::discovery_is_live)
                {
                    Some(d) => d,
                    None => {
                        if options.launch_daemon_if_missing {
                            log::info!("[qaqh-client] no live daemon discovery; launching daemon");
                            wait_for_daemon(options.daemon_path.as_deref(), options.start_timeout)
                                .await?
                        } else {
                            return Err(ClientError::Discovery(
                                "no live daemon discovery (daemon.json missing or stale)".into(),
                            ));
                        }
                    }
                };
                (discovery.base_url()?, discovery.token)
            }
        };

        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .build()?;
        let session = Arc::new(RingingSession::new(
            base_url.clone(),
            token.clone(),
            http.clone(),
        ));
        // 远端直连模式不得从本机 `daemon.json` 刷新凭据（会覆盖成另一台机器
        // 的记录）；只有本地发现模式才允许 daemon 重启后的自愈。
        session.set_local_discovery(options.remote.is_none());

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
                http: http.clone(),
                session: session.clone(),
                handlers: options.handlers.clone(),
                stop_tx,
                tasks,
                timeline: Mutex::new(HashMap::new()),
                attached_seeds: Mutex::new(HashSet::new()),
                v2_session: Mutex::new(None),
            }),
        };

        // Lease renewal.
        let renewal = {
            let session = session.clone();
            let stop = stop_rx.clone();
            tokio::spawn(async move { session.run_renewal(stop).await })
        };
        client.push_task(renewal).await;

        // BUG-2026-09-12-10：租约重新协商（renewal 失败 → open() 换新 lease）后，
        // daemon 侧 seed 归属随旧 client_session_id 一并消失且不迁移——必须为
        // 「已 attach 的 seed」重放 attach，否则 timeline/bootstrap/service 等
        // seed 域读路径会持续 401 lease_required（「切会话被拒」的主因之一）。
        let replay = {
            let client = client.clone();
            let mut ctx = ctx_rx.clone();
            let mut stop = stop_rx.clone();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        changed = ctx.changed() => {
                            if changed.is_err() {
                                return; // session dropped: client is going away
                            }
                        }
                        _ = stop.changed() => return,
                    }
                    let seeds: Vec<String> = {
                        let guard = client.inner.attached_seeds.lock().await;
                        guard.iter().cloned().collect()
                    };
                    for seed in seeds {
                        match client.attach(&seed).await {
                            Ok(_) => log::info!(
                                "[qaqh-client] re-attached {seed} after lease re-negotiation"
                            ),
                            Err(err) => log::warn!(
                                "[qaqh-client] re-attach {seed} failed: {err}; will retry on next re-negotiation"
                            ),
                        }
                    }
                }
            })
        };
        client.push_task(replay).await;

        // Three SSE channels.
        for channel in CHANNELS {
            let stream = ChannelStream::new(
                channel,
                http.clone(),
                StreamHandlers {
                    on_batch: options.handlers.on_batch.clone(),
                    on_status: {
                        let cb = options.handlers.on_status.clone();
                        std::sync::Arc::new(move |status| cb(channel, status))
                    },
                    on_reset: options.handlers.on_reset.clone(),
                    on_liveness: options.handlers.on_liveness.clone(),
                },
                session.clone(),
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

    /// 当前端点与 Bearer token。每次请求现取——daemon 重启后 session 会
    /// 换上新发现记录，任何缓存的副本都会变陈旧。
    pub(crate) fn credentials(&self) -> crate::session::Credentials {
        self.inner.session.credentials()
    }

    /// Current negotiated session state.
    pub async fn session_state(&self) -> Option<SessionState> {
        self.inner.session.state().await
    }

    /// 订阅 `(server_epoch, client_session_id)`。
    ///
    /// 壳层据此感知**会话被重新协商**：续租失败后 client 会重新 open 换新
    /// lease，`server_epoch`（daemon 重启）与 `client_session_id`（同 daemon 内
    /// 重新协商）都可能变。想要在变化时刷新自己的派生状态（会话作用域的缓存、
    /// 服务面凭据、UI 的「daemon 已重启」提示）就得订阅它——`Client` 不会替
    /// 壳层决定这些副作用。
    pub fn session_ctx_rx(&self) -> watch::Receiver<Option<(String, String)>> {
        self.inner.session.session_ctx_rx()
    }

    /// Submit a canonical Ringing command with the shared lease identity.
    ///
    /// The command determines its own channel. `seed` may be `None` only for
    /// `ControlCommand::SessionCreate`; callers never assemble wire JSON or
    /// duplicate the channel tag.
    pub async fn send_command(
        &self,
        seed: Option<&str>,
        command: RingingCommand,
        options: CommandOptions,
    ) -> Result<RingingCommandAck> {
        let state = self
            .inner
            .session
            .state()
            .await
            .ok_or_else(|| ClientError::Negotiation("session not open".into()))?;
        let channel = command.channel();
        let command_id = options
            .command_id
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let mut payload = RingingCommandEnvelope::new(
            command_id.clone(),
            state.client_instance_id.clone(),
            command,
        )
        .with_client_session_id(state.client_session_id.clone());
        if let Some(seed) = seed {
            payload = payload.with_seed(seed);
        }
        payload.expected_revision = options.expected_revision;
        payload
            .validate()
            .map_err(|code| ClientError::Protocol(format!("invalid command: {code}")))?;
        let path = format!("/ringing/v1/commands/{}", channel.as_str());
        let session_id = self.session_id_header().await?;
        let response = self
            .inner
            .http
            .post(format!("{}{path}", self.credentials().base_url))
            .bearer_auth(&self.credentials().token)
            .header("X-QAQH-Client-Session-Id", session_id)
            .json(&payload)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(ClientError::Http {
                status: response.status().as_u16(),
                path,
            });
        }
        let ack: RingingCommandAck = response.json().await?;
        if ack.command_id != command_id {
            return Err(ClientError::Protocol(
                "command ack id does not match submission".into(),
            ));
        }
        Ok(ack)
    }

    /// `GET /ringing/v1/commands/{command_id}` — resolve post-acceptance uncertainty.
    pub async fn command_status(&self, command_id: &str) -> Result<RingingCommandStatus> {
        let path = format!("/ringing/v1/commands/{}", command_id);
        let session_id = self.session_id_header().await?;
        let response = self
            .inner
            .http
            .get(format!("{}{path}", self.credentials().base_url))
            .bearer_auth(&self.credentials().token)
            .header("X-QAQH-Client-Session-Id", session_id)
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

    /// `POST /ringing/v1/service/{name}` — typed read-only query.
    pub async fn query(&self, request: QueryRequest) -> Result<Value> {
        let (name, params) = request.into_parts();
        let session_id = self.session_id_header().await?;
        let path = format!("/ringing/v1/service/{name}");
        let response = self
            .inner
            .http
            .post(format!("{}{path}", self.credentials().base_url))
            .bearer_auth(&self.credentials().token)
            .header("X-QAQH-Client-Session-Id", session_id)
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
    pub async fn bootstrap(&self, seed: &str) -> Result<qaqh_ringing::RingingSessionBootstrap> {
        let session_id = self.session_id_header().await?;
        let path = format!("/ringing/v1/sessions/{seed}/bootstrap");
        let response = self
            .inner
            .http
            .get(format!("{}{path}", self.credentials().base_url))
            .bearer_auth(&self.credentials().token)
            .header("X-QAQH-Client-Session-Id", session_id)
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

    /// Execute a closed, typed auxiliary action (Write 类服务方法)。Method
    /// names and wire params are centralized in `ActionRequest`; native shells
    /// cannot route a mutation through the read-only query surface.
    pub async fn action(&self, request: ActionRequest) -> Result<Value> {
        let (name, mut params) = request.into_parts();
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
            digest
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
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
        let path = format!("/ringing/v1/service/{name}");
        let response = self
            .inner
            .http
            .post(format!("{}{path}", self.credentials().base_url))
            .bearer_auth(&self.credentials().token)
            .header("X-QAQH-Client-Session-Id", session_id)
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
    pub async fn attach(&self, seed: &str) -> Result<RingingCommandAck> {
        let ack = self
            .send_command(
                Some(seed),
                RingingCommand::Control(ControlCommand::SessionResume {
                    seed: seed.to_string(),
                }),
                CommandOptions::default(),
            )
            .await?;
        if ack.status == RingingCommandAckStatus::Accepted {
            // BUG-2026-09-12-10：记录归属，供租约重新协商后重放 attach。
            self.inner
                .attached_seeds
                .lock()
                .await
                .insert(seed.to_string());
        }
        Ok(ack)
    }

    /// Activate the native timeline for one session (mirrors Electron
    /// `ringingManager.activateTimeline`): fetch the authoritative snapshot,
    /// replace any previous timeline stream with a new one seeded at the
    /// snapshot watermark, and return the snapshot. The seed must have been
    /// attached first (`backend.attach` / `session_resume`), otherwise the
    /// daemon rejects the request with 401.
    /// 拉取 timeline 快照页（服务端默认尾部窗口，见 daemon
    /// `TIMELINE_PAGE_LIMIT`）。`before_turn` = 返回该 turn **之前**（更早）
    /// 的页（上滚翻页）；`limit` 覆盖默认页大小。响应含分页元数据
    /// `has_more` / `total_turns`。与 [`Self::activate_timeline`] 不同：
    /// 纯读，**不重建** timeline SSE 流。
    ///
    /// `before_index` = **排他**游标（返回全局序号小于它的那一页），`None` = 最新一页。
    /// 取下一页的游标是**本页最旧那个回合**的 `TimelineTurn::turn_index`。
    ///
    /// 原先是 `before_turn`（turn_id）作游标，已按 spec §0b 的兼容政策**替换**
    /// （BUG-2026-09-15-05）：turn_id 由 worker 计数器生成、会复用，归档投影侧的 id
    /// 又只是消息池内下标——两者都当不了稳定游标。
    pub async fn fetch_timeline_page(
        &self,
        seed: &str,
        before_index: Option<u64>,
        limit: Option<u32>,
    ) -> Result<TimelinePage> {
        self.get_timeline_page(seed, before_index, limit).await
    }

    /// GET `/ringing/v1/sessions/{seed}/timeline` + typed protocol validation.
    async fn get_timeline_page(
        &self,
        seed: &str,
        before_index: Option<u64>,
        limit: Option<u32>,
    ) -> Result<TimelinePage> {
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
        let mut request = self
            .inner
            .http
            .get(format!("{}{path}", self.credentials().base_url))
            .bearer_auth(&self.credentials().token)
            .header("X-QAQH-Client-Session-Id", &state.client_session_id);
        if let Some(before_index) = before_index {
            request = request.query(&[("before_index", before_index)]);
        }
        if let Some(limit) = limit {
            request = request.query(&[("limit", limit)]);
        }
        let response = request.send().await?;
        if !response.status().is_success() {
            return Err(ClientError::Http {
                status: response.status().as_u16(),
                path,
            });
        }
        let page: TimelinePage = response.json().await?;
        page.validate_for(seed).map_err(ClientError::Protocol)?;
        Ok(page)
    }

    /// Activate the native timeline for one session (mirrors Electron
    /// `ringingManager.activateTimeline`): fetch the authoritative snapshot
    /// (tail page), replace that seed's previous timeline stream with a new one
    /// seeded at the snapshot watermark, and return the snapshot. The seed
    /// must have been attached first (`backend.attach` / `session_resume`),
    /// otherwise the daemon rejects the request with 401.
    ///
    /// Streams are **keyed by seed**, so activating one session never disturbs
    /// another's. Shells that keep several sessions live at once (tabs,
    /// subagent sessions, nested subagents) can therefore hold as many
    /// concurrent timelines as they track; a shell that shows one transcript at
    /// a time simply calls this for the newly focused seed and
    /// [`Self::deactivate_timeline`] for the one it left.
    pub async fn activate_timeline(&self, seed: &str) -> Result<TimelinePage> {
        let page = self.get_timeline_page(seed, None, None).await?;
        let watermark = page.snapshot.watermark;

        // Replace this seed's previous stream only.
        let mut guard = self.inner.timeline.lock().await;
        if let Some(prev) = guard.remove(seed) {
            let _ = prev.stop_tx.send(true);
            let _ = prev.status.send_replace(Some(TimelineStatus::Closed {
                seed: seed.to_string(),
                reason: "re-activated".into(),
            }));
        }
        let (stop_tx, stop_rx) = watch::channel(false);
        let (status_tx, _status_rx) = watch::channel(None);
        let seed_owned = seed.to_string();
        let mut stream = TimelineStream::new(
            seed_owned.clone(),
            self.inner.http.clone(),
            self.inner.session.clone(),
            self.inner.handlers.on_timeline_entry.clone(),
            self.inner.handlers.on_timeline_status.clone(),
            self.inner.handlers.on_timeline_snapshot.clone(),
            self.inner.handlers.on_liveness.clone(),
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
        guard.insert(
            seed.to_string(),
            TimelineHandle {
                stop_tx,
                status: status_tx,
            },
        );
        drop(guard);

        // Mirror Electron: the activate response is both returned to the
        // caller and pushed as a snapshot event so listeners rebuild the
        // transcript immediately.
        (self.inner.handlers.on_timeline_snapshot)(page.clone());
        Ok(page)
    }

    /// Stop the timeline stream for one seed (no-op when not active). The other
    /// seeds' streams are untouched.
    pub async fn deactivate_timeline(&self, seed: &str) {
        let handle = self.inner.timeline.lock().await.remove(seed);
        if let Some(handle) = handle {
            let _ = handle.stop_tx.send(true);
            let _ = handle.status.send_replace(Some(TimelineStatus::Closed {
                seed: seed.to_string(),
                reason: "deactivated".into(),
            }));
        }
    }

    /// Timeline connection status for one seed (`None` when never activated).
    pub async fn timeline_status_for(&self, seed: &str) -> Option<TimelineStatus> {
        let guard = self.inner.timeline.lock().await;
        guard.get(seed).and_then(|h| h.status.borrow().clone())
    }

    /// Seeds with an active timeline stream, in unspecified order.
    pub async fn active_timelines(&self) -> Vec<String> {
        self.inner.timeline.lock().await.keys().cloned().collect()
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

    /// `GET /ringing/v1/content/{content_id}` — resolve session-owned external
    /// content and verify it against the digest carried by the canonical ref.
    pub async fn download_content(&self, seed: &str, reference: &ContentRef) -> Result<Vec<u8>> {
        let path = format!("/ringing/v1/content/{}", reference.content_id);
        let session_id = self.session_id_header().await?;
        let response = self
            .inner
            .http
            .get(format!("{}{path}", self.credentials().base_url))
            .query(&[("seed", seed)])
            .bearer_auth(&self.credentials().token)
            .header("X-QAQH-Client-Session-Id", session_id)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(ClientError::Http {
                status: response.status().as_u16(),
                path,
            });
        }
        let bytes = response.bytes().await?.to_vec();
        let digest = {
            use sha2::Digest;
            let hash = sha2::Sha256::digest(&bytes);
            hash.iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        };
        if !digest.eq_ignore_ascii_case(&reference.sha256) {
            return Err(ClientError::Protocol(format!(
                "content digest mismatch for {}: expected {}, received {digest}",
                reference.content_id, reference.sha256
            )));
        }
        Ok(bytes)
    }

    /// `POST /ringing/v1/content` — upload a local attachment as a session
    /// content reference.
    ///
    /// Hand-rolled multipart/form-data（daemon 受限解析只认
    /// `seed` / `media_type` / `content` 三字段，见 `handle_content_upload`）；
    /// 返回的 `ContentRef` 可放入 `conversation_send_message` 的
    /// `attachments`（命令中不允许出现本地路径）。失败调用方自行记录。
    pub async fn upload_content(
        &self,
        seed: &str,
        media_type: &str,
        data: Vec<u8>,
    ) -> Result<ContentRef> {
        let boundary = format!("qaqh-{}", uuid::Uuid::new_v4());
        let mut body = Vec::with_capacity(data.len() + 256);
        let mut push_field = |name: &str, value: &[u8]| {
            body.extend_from_slice(
                format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n")
                    .as_bytes(),
            );
            body.extend_from_slice(value);
            body.extend_from_slice(b"\r\n");
        };
        push_field("seed", seed.as_bytes());
        push_field("media_type", media_type.as_bytes());
        push_field("content", &data);
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());

        let session_id = self.session_id_header().await?;
        let response = self
            .inner
            .http
            .post(format!(
                "{}/ringing/v1/content",
                self.credentials().base_url
            ))
            .bearer_auth(&self.credentials().token)
            .header("X-QAQH-Client-Session-Id", session_id)
            .header(
                "Content-Type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(body)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(ClientError::Http {
                status: response.status().as_u16(),
                path: "/ringing/v1/content".into(),
            });
        }
        Ok(response.json().await?)
    }

    /// `POST /control/v1/stop` / `stop-if-idle` — graceful daemon stop.
    pub async fn stop_daemon(&self, idle_only: bool) -> Result<StopStatus> {
        let path = if idle_only {
            "/control/v1/stop-if-idle"
        } else {
            "/control/v1/stop"
        };
        let response = self
            .inner
            .http
            .post(format!("{}{path}", self.credentials().base_url))
            .bearer_auth(&self.credentials().token)
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
            .thread_name("qaqh-client")
            .build()
            .expect("failed to build qaqh-client runtime")
    })
}

/// Spawn the daemon (`qaqh-daemon run`) and wait for its discovery file.
///
/// 进程内 spawn 串行化：并发 `connect_async`（壳首屏多个 invoke 同时触发）
/// 各自进入本函数时，仅第一个执行「检查 + spawn」决策，其余等待锁后重新
/// 检查——发现 lock/discovery 已就绪则不再 spawn，杜绝并发 spawn 多个
/// daemon 实例（双 daemon 并存触发源）。
static DAEMON_SPAWN_GUARD: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();

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
        .filter(crate::discovery::discovery_is_live);
    if live.is_none() && !crate::discovery::lock_holder_alive() {
        // 唯一 spawn 出口（内含脱离 shell 进程组的保护，见该函数文档）。
        crate::discovery::spawn_daemon_process(executable.as_ref())?;
    }
    drop(guard);

    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match read_discovery() {
            // 同样要求「pid 存活 + 端点真的在听」：spawn 前残留的 stale
            // discovery 不得被当作新 daemon 的就绪信号。
            Ok(d) if crate::discovery::discovery_is_live(&d) => return Ok(d),
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
/// 的候选顺序保持一致：dev 布局（`QAQH_BACKEND_ROOT`/cwd 的 `target/debug`）
/// → exe 旁 `resources/`（安装布局）→ exe 旁 → PATH 兜底。
///
/// 注意：此前仅支持 dev 布局，安装版在「本地映射模式下由桥首次拉起 daemon」
/// 时（`daemon.json` 不存在 → `wait_for_daemon` → 此处）会直接命中 PATH 裸名，
/// 报 `io error: program not found`。统一为 `daemon_executable` 后安装布局
/// 正确命中。
fn default_daemon_path() -> std::path::PathBuf {
    crate::discovery::daemon_executable()
}
