//! Shell 独立并发句柄表（`ADR-0042` 铁律 4/6）。
//!
//! - 热路径全程只碰单个 `ShellHandle` 内的 `Mutex<ShellInner>`，**零中央锁**；
//! - `output_offset`、`history` 与全部 `sinks` 处于同一把锁下：多端同看时既不乱序也不重复/漏段；
//! - `push_output` 对多个观看者广播，单个慢观看者滞后只摘除其自身 `sink`，不影响其它端。

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use qaqh_core_vocab::SessionId;
use qaqh_ringing_v3::ShellView;
use tokio::sync::{mpsc, watch};

use super::identity::DeviceKey;
use super::lock;
use super::state::ServerConfig;

/// Shell PTY 的一个观看者附着点。
struct ShellSink {
    tx: mpsc::Sender<(u64, Bytes)>,
    lag: watch::Sender<bool>,
}

/// 附着结果（先写完 `replay`，再接 `live`）。
pub(crate) struct ShellAttach {
    /// 回放块（偏移 + 字节，从请求偏移开始）。
    pub(crate) replay: Vec<(u64, Bytes)>,
    /// 实时输出（每块自带起始偏移）。
    pub(crate) live: mpsc::Receiver<(u64, Bytes)>,
    /// 队列溢出哨兵。
    pub(crate) lagged: watch::Receiver<bool>,
    /// 服务端权威输出水位。
    pub(crate) attach_offset: u64,
    /// 请求偏移已跌出历史（必须从 `attach_offset` 全文重拉）。
    pub(crate) gap: bool,
    /// 本观看者代际（断开时只摘自己这一代）。
    pub(crate) generation: u64,
}

struct ShellHistory {
    chunks: VecDeque<(u64, Bytes)>,
    bytes: usize,
    capacity: usize,
}

struct ShellInner {
    output_offset: u64,
    size: (u16, u16),
    history: ShellHistory,
    sinks: BTreeMap<u64, ShellSink>,
    next_viewer: u64,
}

/// 一个 shell 句柄（**独立于连接**：断联不杀它；支持多端同时观看）。
pub(crate) struct ShellHandle {
    pub(crate) id: String,
    pub(crate) session_id: Option<SessionId>,
    pub(crate) owner: DeviceKey,
    exited: AtomicBool,
    inner: Mutex<ShellInner>,
}

impl ShellHandle {
    /// 权威输出水位。
    pub(crate) fn output_offset(&self) -> u64 {
        lock(&self.inner).output_offset
    }

    /// 视图。
    pub(crate) fn view(&self) -> ShellView {
        let inner = lock(&self.inner);
        ShellView {
            shell_id: self.id.clone(),
            pid: 0,
            shell: "skeleton".to_string(),
            cwd: String::new(),
            cols: inner.size.0,
            rows: inner.size.1,
            output_offset: inner.output_offset,
            exited: self.exited.load(Ordering::SeqCst),
        }
    }

    /// 调整窗口。
    pub(crate) fn resize(&self, cols: u16, rows: u16) {
        lock(&self.inner).size = (cols, rows);
    }

    /// 标记结束并唤醒所有观看者收流。
    pub(crate) fn mark_exited(&self) {
        self.exited.store(true, Ordering::SeqCst);
        let mut inner = lock(&self.inner);
        for (_, sink) in std::mem::take(&mut inner.sinks) {
            sink.lag.send_replace(true);
        }
    }

    /// 是否已结束。
    pub(crate) fn exited(&self) -> bool {
        self.exited.load(Ordering::SeqCst)
    }

    /// 产出一段 PTY 字节并广播给全部当前观看者。
    pub(crate) fn push_output(&self, bytes: Bytes) {
        let mut inner = lock(&self.inner);
        let len = bytes.len() as u64;
        let start = inner.output_offset;
        inner.output_offset += len;

        inner.history.bytes += bytes.len();
        inner.history.chunks.push_back((start, bytes.clone()));
        while inner.history.bytes > inner.history.capacity {
            if let Some((_, dropped)) = inner.history.chunks.pop_front() {
                inner.history.bytes = inner.history.bytes.saturating_sub(dropped.len());
            } else {
                break;
            }
        }

        let mut dead = Vec::new();
        for (&viewer_id, sink) in &inner.sinks {
            if sink.tx.try_send((start, bytes.clone())).is_err() {
                sink.lag.send_replace(true);
                dead.push(viewer_id);
            }
        }
        for viewer_id in dead {
            inner.sinks.remove(&viewer_id);
        }
    }

    /// 原子附着（同一把锁下读回放 + 挂入多端观看者集合：不踢既有观看者、不丢/不重块）。
    pub(crate) fn attach(&self, from_offset: u64, config: &ServerConfig) -> ShellAttach {
        let mut inner = lock(&self.inner);
        inner.next_viewer += 1;
        let generation = inner.next_viewer;
        let (tx, live) = mpsc::channel(config.shell_queue_chunks.max(1));
        let (lag, lagged) = watch::channel(false);

        let offset = inner.output_offset;
        let oldest = inner
            .history
            .chunks
            .front()
            .map(|(start, _)| *start)
            .unwrap_or(offset);
        let mut replay = Vec::new();
        let mut gap = false;
        if from_offset < oldest {
            gap = true;
        } else {
            for (start, chunk) in &inner.history.chunks {
                if start + chunk.len() as u64 > from_offset {
                    replay.push((*start, chunk.clone()));
                }
            }
        }
        inner.sinks.insert(generation, ShellSink { tx, lag });

        ShellAttach {
            replay,
            live,
            lagged,
            attach_offset: offset,
            gap,
            generation,
        }
    }

    /// 断开单个观看者（不影响其它端）。
    pub(crate) fn detach(&self, generation: u64) {
        lock(&self.inner).sinks.remove(&generation);
    }
}

/// Shell 句柄表（**独立锁**：PTY 热路径不碰任何会话/中央锁）。
pub(crate) struct ShellTable {
    inner: Mutex<BTreeMap<String, Arc<ShellHandle>>>,
    next: AtomicU64,
}

impl ShellTable {
    /// 建表。
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(BTreeMap::new()),
            next: AtomicU64::new(0),
        }
    }

    /// 打开一个 shell。
    pub(crate) fn open(
        &self,
        session_id: Option<&SessionId>,
        owner: &DeviceKey,
        cols: u16,
        rows: u16,
        config: &ServerConfig,
    ) -> Arc<ShellHandle> {
        let ordinal = self.next.fetch_add(1, Ordering::SeqCst) + 1;
        let id = format!("shell-{ordinal:04}");
        let handle = Arc::new(ShellHandle {
            id: id.clone(),
            session_id: session_id.cloned(),
            owner: owner.clone(),
            exited: AtomicBool::new(false),
            inner: Mutex::new(ShellInner {
                output_offset: 0,
                size: (cols, rows),
                history: ShellHistory {
                    chunks: VecDeque::new(),
                    bytes: 0,
                    capacity: config.shell_history_bytes,
                },
                sinks: BTreeMap::new(),
                next_viewer: 0,
            }),
        });
        lock(&self.inner).insert(id, Arc::clone(&handle));
        handle
    }

    /// 查句柄。
    pub(crate) fn get(&self, id: &str) -> Option<Arc<ShellHandle>> {
        lock(&self.inner).get(id).cloned()
    }

    /// 本设备/指定会话的 shell 列表（按 id 升序）。
    pub(crate) fn list(
        &self,
        owner: &DeviceKey,
        session_id: Option<&SessionId>,
    ) -> Vec<ShellView> {
        lock(&self.inner)
            .values()
            .filter(|handle| match session_id {
                Some(sid) => handle.session_id.as_ref() == Some(sid),
                None => &handle.owner == owner,
            })
            .map(|handle| handle.view())
            .collect()
    }

    /// 关闭并移除。
    pub(crate) fn close(&self, id: &str) -> bool {
        let handle = lock(&self.inner).remove(id);
        match handle {
            Some(handle) => {
                handle.mark_exited();
                true
            }
            None => false,
        }
    }

    /// 句柄数（守卫断言"断联不杀进程"用）。
    pub(crate) fn len(&self) -> usize {
        lock(&self.inner).len()
    }
}
