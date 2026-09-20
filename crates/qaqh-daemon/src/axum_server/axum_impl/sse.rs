//! axum_impl::sse — see parent module docs.

use std::collections::BTreeSet;

use qaqh_runtime::QaqhService;
use qaqh_session::actor::ConnectionId;

use super::*;

/// 活跃会话但尚无任何 seed 分片时的挂起轮询间隔（reviewer 阻断 1）。
///
/// 「先开 SSE、后 attach seed」是既有 TUI 时序，不能因为此刻无分片就切断流；
/// 轮询只为了周期性对账租约（attach/失活），开销可忽略。
const IDLE_SHARD_POLL: Duration = Duration::from_millis(50);

/// 租约对账（attach/失活探测）的节流间隔（reviewer 建议 4）。
///
/// 逐事件对账会在每条交付的事件上取一次全局 lease 锁并 clone 一个
/// `HashSet`——正是本 PR 要消除的 per-event 全局锁竞争。
const REFRESH_INTERVAL: Duration = Duration::from_millis(50);

pub(crate) fn parse_sse_cursor(cursor: &str, epoch: &str, channel: RingingChannel) -> u64 {
    let mut parts = cursor.split(':');
    let e = parts.next().unwrap_or("");
    let c = parts.next().unwrap_or("");
    let seq = parts
        .next()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    if e == epoch && c == channel.as_str() {
        seq
    } else {
        0
    }
}

pub(crate) fn parse_timeline_cursor(cursor: &str, epoch: &str) -> u64 {
    let mut parts = cursor.split(':');
    let e = parts.next().unwrap_or_default();
    let kind = parts.next().unwrap_or_default();
    let seq = parts.next().and_then(|v| v.parse::<u64>().ok());
    if e == epoch && kind == "timeline" && parts.next().is_none() {
        seq.unwrap_or(0)
    } else {
        0
    }
}

fn envelope_to_event(
    epoch: &str,
    channel: RingingChannel,
    env: &qaqh_ringing::RingingEventEnvelope,
) -> Event {
    let event_type = serde_json::to_value(&env.event)
        .ok()
        .and_then(|v| v["type"].as_str().map(|s| s.to_string()))
        .unwrap_or_else(|| "message".into());
    // data is full envelope JSON (renderer expects seed/stream_seq/event_id)
    let data = serde_json::to_string(env).unwrap_or_else(|_| "{}".into());
    Event::default()
        .id(format!("{}:{}:{}", epoch, channel.as_str(), env.stream_seq))
        .event(event_type)
        .data(data)
}

fn reset_to_event(reset: &RingingResetRequired) -> Event {
    let data = serde_json::to_string(reset).unwrap_or_else(|_| "{}".into());
    Event::default().event("ringing.reset_required").data(data)
}

fn timeline_entry_to_event(epoch: &str, seed: &str, entry: &qaqh_domain::TimelineEntry) -> Event {
    let data = serde_json::json!({
        "schema": "qaqh.Ringing",
        "version": 1,
        "server_epoch": epoch,
        "seed": seed,
        "entry": entry,
    });
    Event::default()
        .id(format!("{}:timeline:{}", epoch, entry.timeline_seq))
        .event("timeline.entry")
        .json_data(data)
        .unwrap_or_else(|_| Event::default().data("{}"))
}

/// 回放过滤：**锁内只取一次租约归属快照，过滤在锁外做**。
///
/// BUG-2026-09-12-12（issue #31）：原实现在**一次**全局 `leases` 锁内做
/// R 次 `owns_seed`（R = 回放事件数）。实测单次持锁 62 ms @(L=200, R=8192)、
/// 247 ms @(L=200, R=32768)——持锁期间 `renew`/命令/bootstrap/所有会话的
/// 每事件租约检查全部排队，`renew` 被拖过 30 s TTL 即触发 401。
///
/// 现在把归属判定的**输入**（该会话已 attach 的 seed 集合）在锁内一次性
/// 拷出，之后纯内存过滤，与 R 无关；`owns_seed` 的活跃性语义用
/// `is_active_session` 在同一临界区内取，保证「取快照那一刻」的判定等价。
fn filter_replay_for_session(
    mut replay: qaqh_runtime::ringing::hub::ChannelReplay,
    session_id: &str,
    leases: &Arc<Mutex<RingingLeaseStore>>,
) -> qaqh_runtime::ringing::hub::ChannelReplay {
    // 唯一的锁临界区：取一次归属快照（见上方文档）。
    let owned = session_owned_seeds(session_id, leases).unwrap_or_default();
    // 锁外过滤：O(R) 次 HashSet 命中查询，不持任何锁。
    replay.events.retain(|e| owned.contains(&e.seed));
    replay.resets.retain(|r| owned.contains(&r.seed));
    replay
}

/// timeline live 投递判定（纯函数，便于回归测试）。
///
/// BUG-2026-09-13-10：入口（`handle_timeline_events`）只在建流时查一次
/// `owns_seed`；长连接内若发生 seed 级吊销（`detach_seed`：session
/// close/delete，或重新协商后旧 cs 失去归属），lease 本身仍活跃 →
/// `is_active_session` 仍为 true → 已失去该 seed 的会话会继续收到
/// timeline 新条目（数据暴露窗口）。
///
/// 因此这里对**每个** live 事件都复查 `owns_seed`，且刻意置于窗口/去重
/// 过滤之前——否则「窗口外/重复」的事件会先被 `continue` 吞掉，复查形同
/// 不存在（吊销后仍会继续处理后续事件）。
///
/// 注意 `owns_seed` 已内联 `is_active_session`（活跃性 + 归属），故吊销与
/// 整体失活都能在此拦截；lease 彻底失效仍由调用方按原语义 `break`。
#[allow(clippy::too_many_arguments)]
fn should_deliver_timeline_live(
    live: &qaqh_runtime::TimelineLiveEntry,
    session_id: &str,
    seed: &str,
    after: u64,
    replayed: &HashSet<u64>,
    leases: &Arc<Mutex<RingingLeaseStore>>,
) -> bool {
    // 1) 逐事件归属复查（吊销后立即截断）
    if live.seed != seed
        || !leases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .owns_seed(session_id, &live.seed)
    {
        return false;
    }
    // 2) replay 窗口与去重
    !(live.entry.timeline_seq <= after || replayed.contains(&live.entry.timeline_seq))
}

/// 取该会话当前**活跃**的 seed 归属快照（一次短临界区）。
///
/// 活跃性 + 归属必须在**同一**临界区内取——否则快照会不一致（过期 lease
/// 的归属被放行 = 僵尸身份仍能读到数据，见 BUG-2026-09-12-10）。
/// 返回 `None` = 会话不活跃（调用方应终止/过滤为空）。
fn session_owned_seeds(
    session_id: &str,
    leases: &Arc<Mutex<RingingLeaseStore>>,
) -> Option<HashSet<String>> {
    let g = leases.lock().unwrap_or_else(|e| e.into_inner());
    if g.is_active_session(session_id) {
        Some(g.owned_seeds(session_id))
    } else {
        None
    }
}

/// Transport-owned socket lease for one logical `(connection, channel)`.
///
/// The session actor stores the logical subscription; this object only tracks
/// which seed shards the transport currently has receivers for and mirrors
/// additions/removals into that actor.
struct SubscriptionLease {
    service: QaqhService,
    connection_id: ConnectionId,
    channel: RingingChannel,
    seeds: BTreeSet<String>,
}

impl SubscriptionLease {
    fn new(service: QaqhService, connection_id: ConnectionId, channel: RingingChannel) -> Self {
        Self {
            service,
            connection_id,
            channel,
            seeds: BTreeSet::new(),
        }
    }

    fn subscribe(&mut self, seed: &str) {
        if !self.seeds.insert(seed.to_string()) {
            return;
        }
        if let Err(error) = self
            .service
            .subscribe_channel(seed, &self.connection_id, self.channel)
        {
            log::debug!(
                "[sse] logical subscribe skipped for {seed}/{}: {error}",
                self.channel
            );
        }
    }

    fn unsubscribe(&mut self, seed: &str) {
        if !self.seeds.remove(seed) {
            return;
        }
        if let Err(error) =
            self.service
                .unsubscribe_channel(seed, &self.connection_id, self.channel)
        {
            log::debug!(
                "[sse] logical unsubscribe skipped for {seed}/{}: {error}",
                self.channel
            );
        }
    }

    fn close_all(&mut self) {
        let seeds = std::mem::take(&mut self.seeds);
        for seed in seeds {
            if let Err(error) = self.service.connection_closed(&seed, &self.connection_id) {
                log::debug!(
                    "[sse] logical connection close skipped for {seed}/{}: {error}",
                    self.channel
                );
            }
        }
    }
}

impl Drop for SubscriptionLease {
    fn drop(&mut self) {
        self.close_all();
    }
}

/// 某会话在某频道上的**分片**实时流：每个已 attach 的 seed 一个
/// `broadcast::Receiver`，`recv` 在其上做合并。
///
/// BUG-2026-09-12-12（issue #31）：分片前所有连接共享频道单环，任一会话
/// 灌满 1024 容量的环就把**同频道所有连接**推向 `Lagged`。分片后每个
/// seed 独立成环，风暴只影响订阅了该 seed 的连接；本结构把连接**实际
/// 拥有的**几个分片合并回一条流，客户端可见的内容与顺序不变（客户端
/// 本就按 `stream_seq` 游标消费并去重）。
struct ShardedChannelStream {
    channel: RingingChannel,
    session_id: String,
    leases: Arc<Mutex<RingingLeaseStore>>,
    receivers: Vec<(
        String,
        tokio::sync::broadcast::Receiver<qaqh_ringing::RingingEventEnvelope>,
    )>,
    /// Optional P2-2d-b logical subscription mirror. Tests and non-SSE
    /// callers may construct the transport without a session actor.
    subscription: Option<SubscriptionLease>,
    /// 每个分片「已取出但尚未交付」的事件（跨分片按 stream_seq 归并）。
    pending: HashMap<String, qaqh_ringing::RingingEventEnvelope>,
    /// 已检测到溢出、但尚未上报的 `Lagged`（reviewer 阻断 2）。
    ///
    /// 某分片溢出时**不能**立即返回：其它分片 `pending` 里序号更小、本可安全
    /// 交付的事件会被一并丢弃（分片前的频道单环按发布顺序淘汰，不会出现
    /// 「小序号被大序号分片的溢出顺带带走」）。因此溢出先记为标志，待
    /// `pending` 中水位已覆盖的最小序号都交付完之后，再上报终止帧。
    pending_lag: Option<u64>,
}

impl ShardedChannelStream {
    fn new(
        hub: &RingingHub,
        channel: RingingChannel,
        session_id: String,
        leases: Arc<Mutex<RingingLeaseStore>>,
    ) -> Self {
        let owned = session_owned_seeds(&session_id, &leases).unwrap_or_default();
        let receivers = owned
            .iter()
            .map(|seed| (seed.clone(), hub.subscribe(channel, seed)))
            .collect();
        Self {
            channel,
            session_id,
            leases,
            receivers,
            subscription: None,
            pending: HashMap::new(),
            pending_lag: None,
        }
    }

    fn with_subscription(mut self, service: QaqhService, connection_id: ConnectionId) -> Self {
        let seeds: Vec<String> = self
            .receivers
            .iter()
            .map(|(seed, _)| seed.clone())
            .collect();
        let mut subscription = SubscriptionLease::new(service, connection_id, self.channel);
        for seed in seeds {
            subscription.subscribe(&seed);
        }
        self.subscription = Some(subscription);
        self
    }

    /// 对账租约的 seed 归属：新 attach 的 seed 增量补订（保持「订阅先于
    /// 回放」的无缝语义）；已 detach 的 seed 退订。会话失活时返回 false。
    fn refresh(&mut self, hub: &RingingHub) -> bool {
        let Some(owned) = session_owned_seeds(&self.session_id, &self.leases) else {
            if let Some(subscription) = self.subscription.as_mut() {
                subscription.close_all();
            }
            return false;
        };
        let removed: Vec<String> = self
            .receivers
            .iter()
            .filter(|(seed, _)| !owned.contains(seed))
            .map(|(seed, _)| seed.clone())
            .collect();
        for seed in &removed {
            if let Some(subscription) = self.subscription.as_mut() {
                subscription.unsubscribe(seed);
            }
        }
        self.receivers.retain(|(seed, _)| owned.contains(seed));
        self.pending.retain(|seed, _| owned.contains(seed));
        let active: HashSet<&String> = self.receivers.iter().map(|(seed, _)| seed).collect();
        let added: Vec<String> = owned
            .iter()
            .filter(|seed| !active.contains(seed))
            .cloned()
            .collect();
        for seed in added {
            if let Some(subscription) = self.subscription.as_mut() {
                subscription.subscribe(&seed);
            }
            self.receivers
                .push((seed.clone(), hub.subscribe(self.channel, &seed)));
        }
        true
    }

    /// 合并等待下一个事件。全部分片关闭且会话失活时返回 `None`。
    ///
    /// **顺序保证**：`stream_seq` 在 `(server_epoch, channel)` 内全局唯一且
    /// 客户端的 `Last-Event-ID` 游标单调递增（`stream_seq <= cursor` 丢弃），
    /// 因此跨分片必须按 `stream_seq` 升序交付，否则较大序号先到会把稍后到达
    /// 的较小序号永久挡在游标外（丢事件）。
    ///
    /// 可判定性来自发布水位 `hub.live_watermark(channel)`：发布侧在 `live`
    /// 锁内「先投递分片、再抬水位」，所以**凡 `stream_seq <= 水位` 的事件都已
    /// 落在某分片里**——空分片不可能再产出 `<= 水位` 的序号。据此：
    ///
    /// 1. 先把各分片 `try_recv` 到的就绪事件收进 pending（非阻塞）；
    /// 2. 若 pending 最小值 `<= 水位`，直接交付该最小值（严格升序且不等待）；
    /// 3. 否则说明「水位更高的事件已在别处交付过、当前 pending 都是更新的」，
    ///    真等待所有空分片的下一个事件即可（此时不会再出现更小序号）。
    ///
    /// **空分片不终止**（reviewer 阻断 1）：活跃会话可能尚未 attach 任何
    /// seed（先开 SSE、后经 SessionNew/SessionResume attach 的既有 TUI 时序），
    /// 此时 `receivers` 为空。旧实现直接 `return None` → 流被立即切断，之后
    /// attach 的 seed 事件永远收不到（分片前的频道单环无此问题）。现在改为
    /// 挂起轮询：仅当 `refresh` 确认会话失活才结束流。
    ///
    /// **溢出延后上报**（reviewer 阻断 2）：某分片 `Lagged` 时不能立即返回，
    /// 否则其它分片 pending 里序号更小、本可安全交付的事件被一并丢弃。改为先
    /// 记标志，把 pending 中水位已覆盖的最小序号全部交付完之后再上报终止帧。
    async fn recv(&mut self, hub: &RingingHub) -> Option<ShardEvent> {
        use tokio::sync::broadcast::error::{RecvError, TryRecvError};
        loop {
            // 0) 无分片可等：活跃会话可能尚未 attach seed（见文档）。
            if self.receivers.is_empty() {
                if !self.refresh(hub) {
                    // 会话失活 → 干净结束。
                    return None;
                }
                if self.receivers.is_empty() {
                    tokio::time::sleep(IDLE_SHARD_POLL).await;
                    continue;
                }
            }
            let watermark = hub.live_watermark(self.channel);
            // 1) 非阻塞收满各分片的就绪事件。
            // 已记下溢出则不再拉新事件：先把 pending 里可安全交付的更小序号发完。
            let mut closed_seed: Option<String> = None;
            if self.pending_lag.is_none() {
                for (seed, rx) in self.receivers.iter_mut() {
                    if self.pending.contains_key(seed) {
                        continue;
                    }
                    match rx.try_recv() {
                        Ok(envelope) => {
                            self.pending.insert(seed.clone(), envelope);
                        }
                        Err(TryRecvError::Empty) => {}
                        Err(TryRecvError::Lagged(skipped)) => {
                            // 先记标志：先把其它分片可安全交付的更小序号发完。
                            self.pending_lag = Some(match self.pending_lag {
                                Some(prev) => prev.saturating_add(skipped),
                                None => skipped,
                            });
                        }
                        Err(TryRecvError::Closed) => {
                            closed_seed = Some(seed.clone());
                            break;
                        }
                    }
                }
            }
            // 2) 有分片关闭：摘除（其 pending 一并丢弃）后重试。
            if let Some(seed) = closed_seed {
                if let Some(subscription) = self.subscription.as_mut() {
                    subscription.unsubscribe(&seed);
                }
                self.receivers.retain(|(s, _)| s != &seed);
                self.pending.remove(&seed);
                continue;
            }
            // 3) pending 中存在「水位已覆盖」的事件 → 交付其中最小的序号。
            let next = self
                .pending
                .iter()
                .filter(|(_, env)| env.stream_seq <= watermark)
                .min_by_key(|(_, env)| env.stream_seq)
                .map(|(seed, env)| (seed.clone(), env.clone()));
            if let Some((seed, envelope)) = next {
                self.pending.remove(&seed);
                return Some(Ok(envelope));
            }
            // 3.5) 可安全交付的事件已清空，此时才上报溢出（阻断 2）。
            if let Some(skipped) = self.pending_lag.take() {
                return Some(Err(RecvError::Lagged(skipped)));
            }
            // 4) 水位已覆盖的事件都交付完了：真等待空分片的下一个事件。
            let futures: Vec<_> = self
                .receivers
                .iter_mut()
                .filter(|(seed, _)| !self.pending.contains_key(seed.as_str()))
                .map(|(seed, rx)| {
                    let seed = seed.clone();
                    Box::pin(async move { (seed, rx.recv().await) })
                })
                .collect();
            if futures.is_empty() {
                continue;
            }
            let ((seed, result), _, _) = futures_util::future::select_all(futures).await;
            match result {
                Ok(envelope) => {
                    self.pending.insert(seed, envelope);
                }
                Err(RecvError::Lagged(skipped)) => {
                    self.pending_lag = Some(match self.pending_lag {
                        Some(prev) => prev.saturating_add(skipped),
                        None => skipped,
                    });
                }
                Err(RecvError::Closed) => {
                    if let Some(subscription) = self.subscription.as_mut() {
                        subscription.unsubscribe(&seed);
                    }
                    self.receivers.retain(|(s, _)| s != &seed);
                    self.pending.remove(&seed);
                }
            }
        }
    }
}

/// 分片合并流的单个结果。
type ShardEvent =
    Result<qaqh_ringing::RingingEventEnvelope, tokio::sync::broadcast::error::RecvError>;

// ---- SSE handlers (P2) ----
pub(crate) async fn handle_events(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(channel_str): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    if !is_authorized(&headers, &state.token) {
        return unauthorized();
    }
    let Some(session_id) = get_session_id(&headers) else {
        return lease_required_json();
    };
    if !state
        .leases
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_active_session(&session_id)
    {
        return lease_required_json();
    }
    let Some(channel) = parse_channel(&channel_str) else {
        return (StatusCode::NOT_FOUND, "unknown channel").into_response();
    };
    // Last-Event-ID from header or ?last_event_id= query (ringing_http compat)
    let last_event_id = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .or_else(|| query.get("last_event_id").map(|s| s.as_str()))
        .or_else(|| query.get("last-event-id").map(|s| s.as_str()))
        .unwrap_or("");
    let after_seq = parse_sse_cursor(last_event_id, &state.epoch, channel);

    // Subscribe before replay to avoid gap。BUG-2026-09-12-12（issue #31）：
    // 按 **(channel, seed) 分片**订阅该会话实际拥有的 seed（而非频道单环），
    // 其它会话的风暴不再把本连接推向 `Lagged`（验收标准 1）。
    let connection_id = ConnectionId::new(format!(
        "{}:{}:{}",
        session_id,
        channel.as_str(),
        crate::server::random_hex()
    ));
    let mut rx = ShardedChannelStream::new(
        &state.hub,
        channel,
        session_id.clone(),
        state.leases.clone(),
    )
    .with_subscription(state.service.clone(), connection_id);
    let replay = filter_replay_for_session(
        state
            .hub
            .replay_channel_since(channel, after_seq, after_seq == 0),
        &session_id,
        &state.leases,
    );
    let replayed_ids: HashSet<String> = replay.events.iter().map(|e| e.event_id.clone()).collect();
    let epoch = state.epoch.clone();
    let hub = state.hub.clone();
    let mut last_refresh = std::time::Instant::now();

    // Use mpsc channel to bridge broadcast to Sse stream (keeps axum 0.8 Send + 'static)
    let (tx, rx_stream) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(128);
    tokio::spawn(async move {
        // Replay
        for env in replay.events {
            let ev = envelope_to_event(&epoch, channel, &env);
            if tx.send(Ok(ev)).await.is_err() {
                return;
            }
        }
        for reset in replay.resets {
            let ev = reset_to_event(&reset);
            if tx.send(Ok(ev)).await.is_err() {
                return;
            }
        }
        loop {
            // 租约可能在本连接存活期间新增 attach；对账分片订阅（失活即结束
            // 流），保证新 attach 的 seed 事件不丢。
            //
            // reviewer 建议 4：对账按**时间**节流而非逐事件——每交付一条事件就
            // 取一次全局 lease 锁并 clone 一个 HashSet，正是本 PR 要消除的那类
            // per-event 全局锁竞争（与 `filter_replay_for_session` 刻意「与事件
            // 数解耦」的注释自相矛盾）。50ms 的延迟对 attach/失活探测无实际影响。
            if last_refresh.elapsed() >= REFRESH_INTERVAL {
                if !rx.refresh(&hub) {
                    break;
                }
                last_refresh = std::time::Instant::now();
            }
            match rx.recv(&hub).await {
                Some(Ok(envelope)) => {
                    // 分片订阅已按 seed 作用域限定；此处只做游标去重。
                    if envelope.stream_seq <= after_seq || replayed_ids.contains(&envelope.event_id)
                    {
                        continue;
                    }
                    let ev = envelope_to_event(&epoch, channel, &envelope);
                    if tx.send(Ok(ev)).await.is_err() {
                        break;
                    }
                }
                Some(Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped))) => {
                    // BUG-2026-09-12-11：此前直接 break——不记日志、不发终止帧，
                    // 客户端只看到「干净结束」无从定案。现在两条都补上。
                    log::warn!(
                        "[sse] {channel} stream lagged: {skipped} events skipped; terminating for client re-baseline"
                    );
                    let ev = Event::default().event("ringing.stream_terminated").data(
                        serde_json::json!({
                            "code": "lagged",
                            "channel": channel.as_str(),
                            "skipped": skipped,
                            "message": "server event buffer overflow; reconnect to re-baseline",
                        })
                        .to_string(),
                    );
                    let _ = tx.send(Ok(ev)).await;
                    break;
                }
                // 该分片关闭（对账竞态）：下一轮 refresh 会收敛。
                Some(Err(tokio::sync::broadcast::error::RecvError::Closed)) => continue,
                // 全部分片关闭（会话失活/退订）→ 干净结束。
                None => break,
            }
        }
    });

    let stream = ReceiverStream::new(rx_stream);
    Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("keep-alive"),
        )
        .into_response()
}

pub(crate) async fn handle_timeline_events(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(seed): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    if !is_authorized(&headers, &state.token) {
        return unauthorized();
    }
    let Some(session_id) = get_session_id(&headers) else {
        return lease_required_json();
    };
    if seed.is_empty()
        || !state
            .leases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .owns_seed(&session_id, &seed)
    {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::CONTENT_TYPE, "application/json")],
            br#"{"code":"lease_required"}"#.to_vec(),
        )
            .into_response();
    }
    let last_event_id = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .or_else(|| query.get("last_event_id").map(|s| s.as_str()))
        .or_else(|| query.get("last-event-id").map(|s| s.as_str()))
        .unwrap_or("");
    let after = parse_timeline_cursor(last_event_id, &state.epoch);
    let rx = state.hub.subscribe_timeline();
    let replay = state.hub.timeline_replay_since(&seed, after);
    let replayed: HashSet<u64> = replay.iter().map(|e| e.timeline_seq).collect();
    let epoch = state.epoch.clone();
    let leases = state.leases.clone();
    let seed_clone = seed.clone();
    let session_id_clone = session_id.clone();

    let (tx, rx_stream) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(128);
    tokio::spawn(async move {
        for entry in replay {
            let ev = timeline_entry_to_event(&epoch, &seed_clone, &entry);
            if tx.send(Ok(ev)).await.is_err() {
                return;
            }
        }
        let mut rx = rx;
        loop {
            match rx.recv().await {
                Ok(live) => {
                    if !should_deliver_timeline_live(
                        &live,
                        &session_id_clone,
                        &seed_clone,
                        after,
                        &replayed,
                        &leases,
                    ) {
                        continue;
                    }
                    if !leases
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .is_active_session(&session_id_clone)
                    {
                        break;
                    }
                    let ev = timeline_entry_to_event(&epoch, &seed_clone, &live.entry);
                    if tx.send(Ok(ev)).await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    // BUG-2026-09-12-11：同 handle_events，补日志与终止帧。
                    log::warn!(
                        "[sse] timeline {seed_clone} stream lagged: {skipped} entries skipped; terminating for client re-baseline"
                    );
                    let ev = Event::default().event("ringing.stream_terminated").data(
                        serde_json::json!({
                            "code": "lagged",
                            "seed": seed_clone.as_str(),
                            "skipped": skipped,
                            "message": "server event buffer overflow; reconnect to re-baseline",
                        })
                        .to_string(),
                    );
                    let _ = tx.send(Ok(ev)).await;
                    break;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    let stream = ReceiverStream::new(rx_stream);
    Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("keep-alive"),
        )
        .into_response()
}

// ---- debug static (P3) ----

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_domain::{DomainEvent, ToolEvent};
    use qaqh_ringing::{RingingEvent, RingingEventEnvelope};
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    fn live(seed: &str, seq: u64) -> qaqh_runtime::TimelineLiveEntry {
        qaqh_runtime::TimelineLiveEntry {
            seed: seed.into(),
            entry: qaqh_domain::TimelineEntry {
                timeline_seq: seq,
                turn_id: "t1".into(),
                round_num: Some(0),
                event: qaqh_domain::TimelineEvent::TurnOpened {
                    user_text: "q".into(),
                },
            },
        }
    }

    fn store_with_seed() -> Arc<Mutex<RingingLeaseStore>> {
        let leases = Arc::new(Mutex::new(RingingLeaseStore::new()));
        {
            let mut g = leases.lock().unwrap();
            g.open("cs-1".into(), "ci-1".into());
            assert!(g.attach_seed("cs-1", "seed-a"));
        }
        leases
    }

    /// BUG-2026-09-13-10 回归：长连接中途 seed 级吊销（detach_seed）后，
    /// 同连接后续事件必须立即截断，不得继续投递。
    #[test]
    fn timeline_live_is_truncated_after_seed_revoked() {
        let leases = store_with_seed();
        let after = 0;
        let replayed = HashSet::new();

        assert!(
            should_deliver_timeline_live(
                &live("seed-a", 1),
                "cs-1",
                "seed-a",
                after,
                &replayed,
                &leases
            ),
            "吊销前同 seed 事件应投递"
        );

        // 中途吊销（session close / delete → detach_seed）；lease 本身仍活跃。
        leases.lock().unwrap().detach_seed("cs-1", "seed-a");
        assert!(
            leases.lock().unwrap().is_active_session("cs-1"),
            "precondition: lease 仍活跃——旧逻辑正是因此放行"
        );
        assert!(
            !should_deliver_timeline_live(
                &live("seed-a", 2),
                "cs-1",
                "seed-a",
                after,
                &replayed,
                &leases
            ),
            "吊销后同连接后续事件必须立即截断"
        );
        // 窗口外/已回放的事件同样不得因早退而绕过复查
        assert!(
            !should_deliver_timeline_live(
                &live("seed-a", 0),
                "cs-1",
                "seed-a",
                after,
                &replayed,
                &leases
            ),
            "吊销后窗口外事件也不得投递"
        );

        // 重新 attach 后恢复投递
        leases.lock().unwrap().attach_seed("cs-1", "seed-a");
        assert!(
            should_deliver_timeline_live(
                &live("seed-a", 3),
                "cs-1",
                "seed-a",
                after,
                &replayed,
                &leases
            ),
            "重新 attach 后恢复投递"
        );
    }

    /// BUG-2026-09-13-10 回归：重新协商后旧 cs 失去归属（僵尸身份），
    /// 旧连接必须被截断；新 cs 未 attach 前同样不投递。
    #[test]
    fn timeline_live_is_truncated_after_renegotiation() {
        let leases = store_with_seed();
        let after = 0;
        let replayed = HashSet::new();

        assert!(should_deliver_timeline_live(
            &live("seed-a", 1),
            "cs-1",
            "seed-a",
            after,
            &replayed,
            &leases
        ));

        // 重新协商：同 instance 换新 cs → 旧 cs 归属被清除。
        leases.lock().unwrap().open("cs-2".into(), "ci-1".into());
        assert!(
            !should_deliver_timeline_live(
                &live("seed-a", 2),
                "cs-1",
                "seed-a",
                after,
                &replayed,
                &leases
            ),
            "重新协商后旧 cs 必须被截断（僵尸身份）"
        );
        assert!(
            !should_deliver_timeline_live(
                &live("seed-a", 3),
                "cs-2",
                "seed-a",
                after,
                &replayed,
                &leases
            ),
            "新 cs 未 attach 不投递"
        );
        leases.lock().unwrap().attach_seed("cs-2", "seed-a");
        assert!(should_deliver_timeline_live(
            &live("seed-a", 4),
            "cs-2",
            "seed-a",
            after,
            &replayed,
            &leases
        ));
    }

    fn envelope(seed: &str, seq: u64) -> RingingEventEnvelope {
        RingingEventEnvelope::new(
            seed,
            seq,
            1,
            1,
            format!("event-{seed}-{seq}"),
            RingingEvent::Tool(qaqh_domain::ToolEvent::ToolStarted {
                tool_call_id: "call".into(),
                turn_id: "turn".into(),
                round_num: 0,
                name: "exec".into(),
            }),
        )
    }

    fn replay_fixture() -> qaqh_runtime::ringing::hub::ChannelReplay {
        qaqh_runtime::ringing::hub::ChannelReplay {
            events: vec![envelope("s-a", 1), envelope("s-b", 2), envelope("s-a", 3)],
            resets: vec![
                RingingResetRequired::new(RingingChannel::Tool, "s-a", 1),
                RingingResetRequired::new(RingingChannel::Tool, "s-b", 2),
            ],
        }
    }

    /// BUG-2026-09-12-12（issue #31）回归：回放过滤的**语义**不变——只保留
    /// 会话拥有的 seed，且顺序保持；同时锁收窄后行为与原先逐事件判定一致。
    #[test]
    fn replay_filter_keeps_owned_seeds_and_order() {
        let leases = Arc::new(Mutex::new(RingingLeaseStore::new()));
        {
            let mut g = leases.lock().unwrap();
            g.open("cs-1".into(), "ci-1".into());
            g.attach_seed("cs-1", "s-a");
        }
        let filtered = filter_replay_for_session(replay_fixture(), "cs-1", &leases);
        let seeds: Vec<&str> = filtered.events.iter().map(|e| e.seed.as_str()).collect();
        assert_eq!(seeds, vec!["s-a", "s-a"], "只保留归属 seed 且顺序不变");
        assert_eq!(filtered.events[0].stream_seq, 1);
        assert_eq!(filtered.events[1].stream_seq, 3);
        assert_eq!(filtered.resets.len(), 1);
        assert_eq!(filtered.resets[0].seed, "s-a");
    }

    /// BUG-2026-09-12-12（issue #31）回归：分片流把连接拥有的多个 seed
    /// 分片合并成一条流——内容与顺序与分片前一致（客户端可见语义不变）。
    #[tokio::test]
    async fn sharded_stream_merges_owned_seeds_in_publish_order() {
        let hub = RingingHub::new("sse-shard-test");
        let leases = Arc::new(Mutex::new(RingingLeaseStore::new()));
        {
            let mut g = leases.lock().unwrap();
            g.open("cs-1".into(), "ci-1".into());
            g.attach_seed("cs-1", "s-a");
            g.attach_seed("cs-1", "s-b");
        }
        let mut rx =
            ShardedChannelStream::new(&hub, RingingChannel::Tool, "cs-1".into(), leases.clone());
        // 发布顺序：a1, b2, a3 —— 合并流应原样送达（每条各出现一次）。
        hub.publish(
            "s-a",
            DomainEvent::Tool(ToolEvent::ToolStarted {
                tool_call_id: "call-a1".into(),
                turn_id: "t".into(),
                round_num: 0,
                name: "exec".into(),
            }),
        );
        hub.publish(
            "s-b",
            DomainEvent::Tool(ToolEvent::ToolStarted {
                tool_call_id: "call-b2".into(),
                turn_id: "t".into(),
                round_num: 0,
                name: "exec".into(),
            }),
        );
        hub.publish(
            "s-a",
            DomainEvent::Tool(ToolEvent::ToolStarted {
                tool_call_id: "call-a3".into(),
                turn_id: "t".into(),
                round_num: 0,
                name: "exec".into(),
            }),
        );

        let mut seen = Vec::new();
        for _ in 0..3 {
            let env = rx.recv(&hub).await.expect("shard event").expect("ok");
            seen.push((env.seed.clone(), env.stream_seq));
        }
        assert_eq!(
            seen,
            vec![
                ("s-a".to_string(), 1),
                ("s-b".to_string(), 2),
                ("s-a".to_string(), 3),
            ],
            "分片合并流的顺序必须等于发布顺序"
        );
    }

    /// 严格升序 + 不饥饿：**闲置分片**（永无事件）不得拖住合并流。
    ///
    /// 水位让合并可判定：`stream_seq <= 水位` 的事件一定已在分片里，因此
    /// 空分片无需等待即可交付当前最小值——既保持升序，也不会因某会话静默
    /// 而让整条连接饿死。
    #[tokio::test]
    async fn sharded_stream_does_not_starve_on_idle_shard() {
        let hub = RingingHub::new("sse-shard-idle");
        let leases = Arc::new(Mutex::new(RingingLeaseStore::new()));
        {
            let mut g = leases.lock().unwrap();
            g.open("cs-1".into(), "ci-1".into());
            g.attach_seed("cs-1", "s-a");
            // s-idle 永不发布：其分片永远为空。
            g.attach_seed("cs-1", "s-idle");
        }
        let mut rx =
            ShardedChannelStream::new(&hub, RingingChannel::Tool, "cs-1".into(), leases.clone());
        for i in 1..=5u64 {
            hub.publish(
                "s-a",
                DomainEvent::Tool(ToolEvent::ToolStarted {
                    tool_call_id: format!("call-{i}"),
                    turn_id: "t".into(),
                    round_num: 0,
                    name: "exec".into(),
                }),
            );
        }
        // 必须在不依赖闲置分片的情况下按序交付全部 5 条。
        let mut got = Vec::new();
        for _ in 0..5 {
            let env = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv(&hub))
                .await
                .expect("闲置分片不得让合并流饿死")
                .expect("shard event")
                .expect("ok");
            got.push(env.stream_seq);
        }
        assert_eq!(got, vec![1, 2, 3, 4, 5], "必须严格升序交付");
    }

    /// 分片流只收到自己 seed 的事件：他人风暴不进本连接（隔离验收）。
    #[tokio::test]
    async fn sharded_stream_ignores_other_seeds() {
        let hub = RingingHub::new("sse-shard-iso");
        let leases = Arc::new(Mutex::new(RingingLeaseStore::new()));
        {
            let mut g = leases.lock().unwrap();
            g.open("cs-1".into(), "ci-1".into());
            g.attach_seed("cs-1", "s-a");
        }
        let mut rx =
            ShardedChannelStream::new(&hub, RingingChannel::Tool, "cs-1".into(), leases.clone());
        // 他人的风暴。
        for seq in 1..=512u64 {
            hub.publish(
                "s-b",
                DomainEvent::Tool(ToolEvent::ToolStarted {
                    tool_call_id: format!("call-{seq}"),
                    turn_id: "t".into(),
                    round_num: 0,
                    name: "exec".into(),
                }),
            );
        }
        // 本连接自己的事件必须照常收到。
        hub.publish(
            "s-a",
            DomainEvent::Tool(ToolEvent::ToolStarted {
                tool_call_id: "mine".into(),
                turn_id: "t".into(),
                round_num: 0,
                name: "exec".into(),
            }),
        );
        let env = rx.recv(&hub).await.expect("own event").expect("ok");
        assert_eq!(env.seed, "s-a", "他人风暴不得进入本连接的分片流");
    }

    /// 未 attach 任何 seed 的活跃会话 → 过滤为空（不退化为「全放行」）。
    #[test]
    fn replay_filter_empty_for_session_without_seeds() {
        let leases = Arc::new(Mutex::new(RingingLeaseStore::new()));
        leases.lock().unwrap().open("cs-1".into(), "ci-1".into());
        let filtered = filter_replay_for_session(replay_fixture(), "cs-1", &leases);
        assert!(filtered.events.is_empty());
        assert!(filtered.resets.is_empty());
    }

    /// 过期/未知会话 → 过滤为空（活跃性仍与归属同临界区判定，
    /// 不得因锁收窄而放行僵尸身份）。
    #[test]
    fn replay_filter_rejects_inactive_session() {
        let leases = Arc::new(Mutex::new(RingingLeaseStore::new()));
        {
            let mut g = leases.lock().unwrap();
            g.open("cs-1".into(), "ci-1".into());
            g.attach_seed("cs-1", "s-a");
            g.set_expiry_for_test("ci-1", Instant::now() - Duration::from_secs(1));
        }
        let filtered = filter_replay_for_session(replay_fixture(), "cs-1", &leases);
        assert!(filtered.events.is_empty(), "过期租约不得回放任何事件");
        assert!(filtered.resets.is_empty());
    }

    /// reviewer 阻断 1 回归：活跃会话但**尚未 attach 任何 seed**（先开 SSE、
    /// 后经 SessionNew/SessionResume attach 的既有 TUI 时序）时，`recv()` 不得
    /// 返回 `None`（旧实现会立即切断流，之后 attach 的 seed 事件永远收不到）。
    /// 修后应在 attach 后正常交付该 seed 的事件。
    #[tokio::test]
    async fn sharded_stream_waits_when_no_seed_attached_yet() {
        let hub = Arc::new(RingingHub::new("sse-shard-noseed"));
        let leases = Arc::new(Mutex::new(RingingLeaseStore::new()));
        leases.lock().unwrap().open("cs-1".into(), "ci-1".into());
        let mut rx =
            ShardedChannelStream::new(&hub, RingingChannel::Tool, "cs-1".into(), leases.clone());
        // 后置 attach（模拟 SessionNew/SessionResume 到达）。
        let attach_leases = leases.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            attach_leases.lock().unwrap().attach_seed("cs-1", "s-late");
        });
        // 再等一会才发布，确保事件在 attach 之后才产生。
        let publish_hub = hub.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            publish_hub.publish(
                "s-late",
                DomainEvent::Tool(ToolEvent::ToolStarted {
                    tool_call_id: "late".into(),
                    turn_id: "t".into(),
                    round_num: 0,
                    name: "exec".into(),
                }),
            );
        });
        let env = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv(&hub))
            .await
            .expect("活跃会话无分片时不得切断流（旧实现立即 return None）")
            .expect("shard event")
            .expect("ok");
        assert_eq!(env.seed, "s-late", "后 attach 的 seed 事件必须能收到");
    }

    /// reviewer 阻断 2 回归：某分片 `Lagged` 时，其它分片 pending 中序号更小、
    /// 本可安全交付的事件不得被一并丢弃（旧实现立即 `return Lagged` 把它们丢了）。
    #[tokio::test]
    async fn lagged_shard_does_not_drop_smaller_pending_events() {
        let hub = Arc::new(RingingHub::new("sse-shard-lag"));
        let leases = Arc::new(Mutex::new(RingingLeaseStore::new()));
        {
            let mut g = leases.lock().unwrap();
            g.open("cs-1".into(), "ci-1".into());
            g.attach_seed("cs-1", "s-quiet");
            g.attach_seed("cs-1", "s-storm");
        }
        let mut rx =
            ShardedChannelStream::new(&hub, RingingChannel::Tool, "cs-1".into(), leases.clone());
        // s-quiet 先发一条（seq 较小，落在它的分片里，未被取走）。
        hub.publish(
            "s-quiet",
            DomainEvent::Tool(ToolEvent::ToolStarted {
                tool_call_id: "quiet-1".into(),
                turn_id: "t".into(),
                round_num: 0,
                name: "exec".into(),
            }),
        );
        // s-storm 灌满自己的分片 → 其分片 Lagged。
        for i in 0..4096u64 {
            hub.publish(
                "s-storm",
                DomainEvent::Tool(ToolEvent::ToolStarted {
                    tool_call_id: format!("storm-{i}"),
                    turn_id: "t".into(),
                    round_num: 0,
                    name: "exec".into(),
                }),
            );
        }
        // 首个 recv：s-storm 溢出，但 s-quiet 的 seq=1 更小且水位已覆盖 → 必须先交付它。
        let first = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv(&hub))
            .await
            .expect("must not hang")
            .expect("shard event");
        match first {
            Ok(env) => assert_eq!(
                env.seed, "s-quiet",
                "更小序号的 pending 事件不得被溢出顺带丢弃"
            ),
            Err(_) => panic!("旧实现会先返回 Lagged，把 s-quiet 的 seq=1 丢掉"),
        }
        // 之后才应上报溢出（供调用方发终止帧 + 客户端 re-baseline）。
        let second = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv(&hub))
            .await
            .expect("must not hang")
            .expect("shard event");
        assert!(second.is_err(), "可安全交付的事件清空后才上报 Lagged");
    }
}
