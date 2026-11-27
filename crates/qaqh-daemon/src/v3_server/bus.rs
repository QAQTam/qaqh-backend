//! 会话事件总线：**有界回放环 + 原子订阅 + 同流合帧**（`ADR-0042` 铁律 4）。
//!
//! ## 为什么"原子订阅"是一个真问题
//!
//! 朴素写法是两步：①读回放环拿到历史帧，②把 receiver 挂到广播表上。两步之间
//! 有一条**致命的缝**：缝里 publish 出去的帧既不在你读到的历史里，也不在挂上之后
//! 的广播里——它凭空消失了，而客户端会以为"我已经从 seq=N 接上了"。
//!
//! 这不是理论风险：洪峰期间 attach 正好踩在这条缝上的概率与流量成正比。
//! 本模块的解法是**同一把锁里完成"读环 + 注册"**（[`EventBus::subscribe`]），
//! 于是 publish 要么整体发生在订阅之前（它的帧在环里，会被回放到），要么整体
//! 发生在订阅之后（它看得见新挂的 receiver）。**没有第三种可能**。
//!
//! 反证在 [`EventBus::subscribe_with_broken_ordering`]（仅测试可见）：同样两件事、
//! 故意拆成两次加锁，守卫测试随即断言"丢帧 > 0"。
//!
//! ## 有界回放尾 vs 静默丢帧
//!
//! 环是有界的（[`EventBus::ring_capacity`] 帧）。游标跌出环覆盖范围时**绝不**
//! "从最老的接着给"——那会把中间的空洞伪装成连续历史。这里一律
//! [`SubscribeOutcome::CursorGap`]，客户端据此重取快照（`ResyncRequired{CursorGap}`）。
//!
//! 订阅队列同样有界：满了就置 lag 哨兵并让写者收流，**不静默丢帧**——
//! 终端与会话状态都不接受"少了一段但看起来正常"。
//!
//! ## 合帧只改"分批形状"，不改内容
//!
//! `Replaceable` 增量（答案/推理/exec 输出）在同一 `seq` 流上紧密合帧：
//! 合出来的帧带**这批增量的最大 `seq`**，文本是连续拼接。这条约定让两件事同时成立：
//!
//! - 客户端 `since_seq = N` 续订时只拿到 `seq > N` 的帧，不会重复收到已合帧的文本；
//! - 回放环里存的永远是**未合帧的权威帧**，所以"重连尾追"拿到的是标准形状，
//!   与"实时看到合帧"的差别只是分批，不是内容。
//!
//! 字节守恒由守卫断言（客户端收到的增量总字节 == 服务端发布的增量总字节）。

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use qaqh_core_vocab::{BlockId, ExecSessionId, SessionId, ToolCallId};
use qaqh_ringing_v3::{DeliveryClass, FrameMeta, SessionEvent, SessionEventFrame};
use tokio::sync::{mpsc, watch};

use super::lock;
use super::now_ms;

/// 默认回放环容量（帧）。4096 帧 ≈ 数百 KiB 量级的会话事件尾巴，足够覆盖
/// 一次重连/一次渲染卡顿，又不至于把内存预算吃掉。
pub(crate) const DEFAULT_RING_CAPACITY: usize = 4096;

/// 默认订阅队列容量（帧）。满了置 lag，不静默丢帧。
pub(crate) const DEFAULT_SUBSCRIBER_QUEUE: usize = 1024;

/// 订阅结果。
pub(crate) enum SubscribeOutcome {
    /// 订阅成功：先写完 `replay`，再接 `live`。
    Attached {
        /// 回放帧（`seq` 升序，全部大于请求的 `since_seq`）。
        replay: Vec<Arc<SessionEventFrame>>,
        /// 实时帧接收端。
        live: mpsc::Receiver<Arc<SessionEventFrame>>,
        /// 队列溢出哨兵（`true` = 已跟不上，必须收流重订）。
        lagged: watch::Receiver<bool>,
        /// 服务端当前已发布的最后 `seq`（attach 点，权威游标）。
        attach_seq: u64,
    },
    /// 游标已跌出回放尾：必须重取快照，不允许"尽量拼一下"。
    CursorGap {
        /// 客户端请求的 `since_seq`。
        requested_since: u64,
        /// 环里最老的可用 `seq`。
        oldest_available: u64,
        /// 服务端当前最后 `seq`。
        newest: u64,
    },
}

struct BusInner {
    ring: VecDeque<Arc<SessionEventFrame>>,
    next_seq: u64,
    subscribers: BTreeMap<u64, Subscriber>,
    next_subscriber_id: u64,
}

struct Subscriber {
    tx: mpsc::Sender<Arc<SessionEventFrame>>,
    lag: watch::Sender<bool>,
}

/// 会话事件总线（每会话一份）。
pub(crate) struct EventBus {
    ring_capacity: usize,
    subscriber_queue: usize,
    inner: std::sync::Mutex<BusInner>,
}

impl EventBus {
    /// 建一条总线。
    pub(crate) fn new(ring_capacity: usize, subscriber_queue: usize) -> Self {
        Self {
            ring_capacity: ring_capacity.max(1),
            subscriber_queue: subscriber_queue.max(1),
            inner: std::sync::Mutex::new(BusInner {
                ring: VecDeque::with_capacity(ring_capacity.max(1)),
                next_seq: 0,
                subscribers: BTreeMap::new(),
                next_subscriber_id: 0,
            }),
        }
    }

    /// 当前最后 `seq`。
    pub(crate) fn last_seq(&self) -> u64 {
        lock(&self.inner).next_seq
    }

    /// 发布一个事件，返回它的 `seq`（同一条流上单调递增，**无洞**）。
    pub(crate) fn publish(&self, event: SessionEvent) -> u64 {
        self.publish_at(event, now_ms())
    }

    /// 同上，但显式给服务端时刻（守卫用来造可复现的时序）。
    pub(crate) fn publish_at(&self, event: SessionEvent, server_ts_ms: u64) -> u64 {
        let mut inner = lock(&self.inner);
        inner.next_seq += 1;
        let seq = inner.next_seq;
        let frame = Arc::new(SessionEventFrame {
            meta: FrameMeta {
                seq,
                server_ts_ms,
                delivery: event.delivery(),
            },
            event,
        });
        inner.ring.push_back(Arc::clone(&frame));
        while inner.ring.len() > self.ring_capacity {
            inner.ring.pop_front();
        }

        let mut dead = Vec::new();
        for (id, subscriber) in &inner.subscribers {
            if subscriber.tx.try_send(Arc::clone(&frame)).is_err() {
                // 队列满：置哨兵并摘除。写者据此收流并广播 ResyncRequired{CursorGap}。
                subscriber.lag.send_replace(true);
                dead.push(*id);
            }
        }
        for id in dead {
            inner.subscribers.remove(&id);
        }
        seq
    }

    /// **原子订阅**：同一把锁里完成"读回放环 + 注册 receiver"。
    pub(crate) fn subscribe(&self, since_seq: u64) -> SubscribeOutcome {
        let mut inner = lock(&self.inner);
        let newest = inner.next_seq;
        let oldest = inner.ring.front().map_or(newest + 1, |f| f.meta.seq);
        if since_seq > newest || since_seq + 1 < oldest {
            return SubscribeOutcome::CursorGap {
                requested_since: since_seq,
                oldest_available: oldest,
                newest,
            };
        }
        let replay: Vec<Arc<SessionEventFrame>> = inner
            .ring
            .iter()
            .filter(|frame| frame.meta.seq > since_seq)
            .cloned()
            .collect();

        let (tx, live) = mpsc::channel(self.subscriber_queue);
        let (lag, lagged) = watch::channel(false);
        let id = inner.next_subscriber_id;
        inner.next_subscriber_id += 1;
        inner.subscribers.insert(id, Subscriber { tx, lag });

        SubscribeOutcome::Attached {
            replay,
            live,
            lagged,
            attach_seq: newest,
        }
    }

    /// **反证原语之一**：只读回放环、不注册 receiver（仅测试可见）。
    ///
    /// 与 [`Self::register_only`] 组合起来，就能**确定性地**复刻"先读环、后注册"
    /// 的破序写法：两步之间插一条 publish，那条帧既不在回放里、也不在广播里。
    #[cfg(test)]
    pub(crate) fn replay_after(&self, since_seq: u64) -> Vec<Arc<SessionEventFrame>> {
        let inner = lock(&self.inner);
        inner
            .ring
            .iter()
            .filter(|frame| frame.meta.seq > since_seq)
            .cloned()
            .collect()
    }

    /// **反证原语之二**：只注册 receiver、不读回放环（仅测试可见）。
    #[cfg(test)]
    pub(crate) fn register_only(
        &self,
    ) -> (
        mpsc::Receiver<Arc<SessionEventFrame>>,
        watch::Receiver<bool>,
    ) {
        let mut inner = lock(&self.inner);
        let (tx, live) = mpsc::channel(self.subscriber_queue);
        let (lag, lagged) = watch::channel(false);
        let id = inner.next_subscriber_id;
        inner.next_subscriber_id += 1;
        inner.subscribers.insert(id, Subscriber { tx, lag });
        (live, lagged)
    }
}

/// 一条可合帧增量的身份（同 key 才允许合）。
#[derive(Debug, Clone, PartialEq, Eq)]
enum CoalesceKey {
    Answer(SessionId, BlockId),
    Reasoning(SessionId, BlockId),
    ExecOutput(SessionId, ExecSessionId),
    ToolArguments(SessionId, ToolCallId),
    ToolOutput(SessionId, ToolCallId),
}

/// 已合并但尚未写出的增量。
struct MergedDelta {
    key: CoalesceKey,
    event: SessionEvent,
    meta: FrameMeta,
}

/// 同流合帧器：**结构上保证 `Reliable` 永远排在它前面的增量之后**。
///
/// 合帧器最多持有一帧未写出的 `Replaceable` 增量。每当来一帧不能合的（`Reliable`，
/// 或换了 key），先把手里那帧吐出去，再处理新帧——所以"终态早于尾增量"这种乱序
/// 在类型层面就发生不了，而不是靠调用方记得先 flush。
pub(crate) struct DeltaCoalescer {
    merged: Option<MergedDelta>,
    /// 被合掉的帧数（守卫据此证明"洪峰真的被合了"，不是"恰好吃得下"）。
    coalesced_away: u64,
}

impl DeltaCoalescer {
    /// 新建。
    pub(crate) fn new() -> Self {
        Self {
            merged: None,
            coalesced_away: 0,
        }
    }

    /// 被合掉的帧数。
    pub(crate) fn coalesced_away(&self) -> u64 {
        self.coalesced_away
    }

    /// 喂一帧，返回**现在就应该按序写出**的帧（可能为空）。
    pub(crate) fn push(&mut self, frame: Arc<SessionEventFrame>) -> Vec<Arc<SessionEventFrame>> {
        // `spilled: true` 标志泄洪边界：不可被合帧吞掉，先 flush 旧帧再直接透传。
        if matches!(
            &frame.event,
            SessionEvent::ExecOutputDelta { spilled: true, .. }
        ) {
            let mut out = self.flush();
            out.push(frame);
            return out;
        }
        let Some(key) = coalesce_key(&frame.event) else {
            // Reliable / 非增量：先 flush 手里的增量，再吐新帧。
            let mut out = self.flush();
            out.push(frame);
            return out;
        };
        let mut out = Vec::new();
        if let Some(merged) = self.merged.as_mut()
            && merged.key == key
            && can_merge(&merged.event, &frame.event)
        {
            merge_event(&mut merged.event, &frame.event);
            merged.meta = frame.meta;
            self.coalesced_away += 1;
            return out;
        }
        out.extend(self.flush());
        self.merged = Some(MergedDelta {
            key,
            event: frame.event.clone(),
            meta: frame.meta,
        });
        out
    }

    /// 吐出手里最后一帧（批次收尾/写者退出前必须调用）。
    pub(crate) fn flush(&mut self) -> Vec<Arc<SessionEventFrame>> {
        match self.merged.take() {
            Some(merged) => vec![Arc::new(SessionEventFrame {
                meta: merged.meta,
                event: merged.event,
            })],
            None => Vec::new(),
        }
    }
}

fn coalesce_key(event: &SessionEvent) -> Option<CoalesceKey> {
    match event {
        SessionEvent::AnswerDelta {
            session_id,
            block_id,
            ..
        } => Some(CoalesceKey::Answer(session_id.clone(), block_id.clone())),
        SessionEvent::ReasoningDelta {
            session_id,
            block_id,
            ..
        } => Some(CoalesceKey::Reasoning(session_id.clone(), block_id.clone())),
        SessionEvent::ExecOutputDelta {
            session_id,
            exec_session_id,
            ..
        } => Some(CoalesceKey::ExecOutput(
            session_id.clone(),
            exec_session_id.clone(),
        )),
        SessionEvent::ToolArgumentsDelta {
            session_id,
            tool_call_id,
            ..
        } => Some(CoalesceKey::ToolArguments(
            session_id.clone(),
            tool_call_id.clone(),
        )),
        SessionEvent::ToolOutputDelta {
            session_id,
            tool_call_id,
            ..
        } => Some(CoalesceKey::ToolOutput(
            session_id.clone(),
            tool_call_id.clone(),
        )),
        _ => None,
    }
}

fn can_merge(target: &SessionEvent, incoming: &SessionEvent) -> bool {
    match (target, incoming) {
        (SessionEvent::AnswerDelta { .. }, SessionEvent::AnswerDelta { .. })
        | (SessionEvent::ReasoningDelta { .. }, SessionEvent::ReasoningDelta { .. })
        | (SessionEvent::ToolArgumentsDelta { .. }, SessionEvent::ToolArgumentsDelta { .. })
        | (SessionEvent::ToolOutputDelta { .. }, SessionEvent::ToolOutputDelta { .. }) => true,
        (
            SessionEvent::ExecOutputDelta {
                end_offset,
                spilled: false,
                ..
            },
            SessionEvent::ExecOutputDelta {
                start_offset,
                spilled: false,
                ..
            },
        ) => *end_offset == *start_offset,
        _ => false,
    }
}

/// 把 `incoming` 的内容并进 `target`（只在 [`can_merge`] 为真时调用）。
fn merge_event(target: &mut SessionEvent, incoming: &SessionEvent) {
    match (target, incoming) {
        (
            SessionEvent::AnswerDelta { text, .. },
            SessionEvent::AnswerDelta {
                text: incoming_text,
                ..
            },
        )
        | (
            SessionEvent::ReasoningDelta { text, .. },
            SessionEvent::ReasoningDelta {
                text: incoming_text,
                ..
            },
        )
        | (
            SessionEvent::ToolOutputDelta { text, .. },
            SessionEvent::ToolOutputDelta {
                text: incoming_text,
                ..
            },
        ) => text.push_str(incoming_text),
        (
            SessionEvent::ToolArgumentsDelta { delta, .. },
            SessionEvent::ToolArgumentsDelta {
                delta: incoming_delta,
                ..
            },
        ) => delta.push_str(incoming_delta),
        (
            SessionEvent::ExecOutputDelta {
                end_offset, text, ..
            },
            SessionEvent::ExecOutputDelta {
                end_offset: incoming_end,
                text: incoming_text,
                ..
            },
        ) => {
            text.push_str(incoming_text);
            *end_offset = *incoming_end;
        }
        _ => {}
    }
}

/// 服务端发布的增量字节账（守卫用来与客户端收到的字节对账）。
#[derive(Debug, Default)]
pub(crate) struct DeltaLedger {
    /// 已发布的增量文本总字节。
    pub(crate) bytes: AtomicU64,
    /// 已发布的增量帧数。
    pub(crate) frames: AtomicU64,
    /// 已发布的 `Reliable` 帧数。
    pub(crate) reliable_frames: AtomicU64,
}

impl DeltaLedger {
    /// 记一帧。
    pub(crate) fn record(&self, event: &SessionEvent) {
        if event.delivery() == DeliveryClass::Reliable {
            self.reliable_frames.fetch_add(1, Ordering::Relaxed);
            return;
        }
        self.frames.fetch_add(1, Ordering::Relaxed);
        let bytes = match event {
            SessionEvent::AnswerDelta { text, .. }
            | SessionEvent::ReasoningDelta { text, .. }
            | SessionEvent::ToolOutputDelta { text, .. } => text.len(),
            SessionEvent::ToolArgumentsDelta { delta, .. } => delta.len(),
            SessionEvent::ExecOutputDelta { text, .. } => text.len(),
            _ => 0,
        };
        self.bytes.fetch_add(bytes as u64, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exec_frame(
        seq: u64,
        start: u64,
        end: u64,
        text: &str,
        spilled: bool,
    ) -> Arc<SessionEventFrame> {
        Arc::new(SessionEventFrame {
            meta: FrameMeta {
                seq,
                server_ts_ms: seq,
                delivery: DeliveryClass::Replaceable,
            },
            event: SessionEvent::ExecOutputDelta {
                session_id: SessionId::new("ses-1").expect("id"),
                exec_session_id: ExecSessionId::new("execs_1").expect("id"),
                start_offset: start,
                end_offset: end,
                text: text.to_string(),
                spilled,
            },
        })
    }

    #[test]
    fn coalescer_never_drops_gapped_or_spilled_exec_deltas() {
        let mut coalescer = DeltaCoalescer::new();
        // 1..2 连续：合并；3 出现偏移洞（10..15）：绝不静默丢弃，必须先吐出 [0..6] 再保留 [10..15]；
        // 4 为 spilled=true：必须先把 [10..15] 吐出，再按序吐出 spilled 帧。
        assert!(coalescer.push(exec_frame(1, 0, 3, "aaa", false)).is_empty());
        assert!(coalescer.push(exec_frame(2, 3, 6, "bbb", false)).is_empty());

        let flushed_on_gap = coalescer.push(exec_frame(3, 10, 15, "ccccc", false));
        assert_eq!(flushed_on_gap.len(), 1);
        match &flushed_on_gap[0].event {
            SessionEvent::ExecOutputDelta {
                start_offset,
                end_offset,
                text,
                spilled,
                ..
            } => {
                assert_eq!((*start_offset, *end_offset, *spilled), (0, 6, false));
                assert_eq!(text, "aaabbb");
            }
            other => panic!("意外帧：{other:?}"),
        }

        let out_on_spill = coalescer.push(exec_frame(4, 15, 20, "ddddd", true));
        assert_eq!(out_on_spill.len(), 2);
        match &out_on_spill[0].event {
            SessionEvent::ExecOutputDelta {
                start_offset,
                end_offset,
                text,
                spilled,
                ..
            } => {
                assert_eq!((*start_offset, *end_offset, *spilled), (10, 15, false));
                assert_eq!(text, "ccccc");
            }
            other => panic!("意外帧：{other:?}"),
        }
        match &out_on_spill[1].event {
            SessionEvent::ExecOutputDelta {
                start_offset,
                end_offset,
                text,
                spilled,
                ..
            } => {
                assert_eq!((*start_offset, *end_offset, *spilled), (15, 20, true));
                assert_eq!(text, "ddddd");
            }
            other => panic!("意外帧：{other:?}"),
        }
        assert!(coalescer.flush().is_empty());
    }
}
