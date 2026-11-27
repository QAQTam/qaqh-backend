//! 按 `(DeviceKey, command_id)` 隔离的有界幂等回执表（`ADR-0042` 铁律 4/5）。
//!
//! ## 为什么必须绑定 `DeviceKey`
//!
//! 幂等键 `command_id` 由客户端生成。若按全局裸 `command_id` 建表，低作用域设备
//! 只要复用或撞中主设备的 `command_id`，就能绕过权限校验直接拿到主设备的缓存回执
//! （跨设备越权 + 串话）。按 `(DeviceKey, command_id)` 分桶后，不同设备之间的
//! 幂等命名空间物理隔离。

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use qaqh_ringing_v3::ControlReply;
use tokio::sync::watch;

use super::identity::DeviceKey;
use super::lock;

/// 已完成回执表上限（FIFO 淘汰最老条目，防止长运行无界增长）。
pub(crate) const MAX_COMPLETED_RECEIPTS: usize = 4096;

type ReceiptKey = (DeviceKey, String);

enum ReceiptSlot {
    InFlight(watch::Receiver<Option<Arc<ControlReply>>>),
    Done(Arc<ControlReply>),
}

/// 幂等回执表的入口判定。
pub(crate) enum ReceiptStart {
    /// 本连接是第一个：执行完必须 [`ReceiptTable::finish`]。
    Fresh {
        /// 完成后用它唤醒所有等待者。
        publish: watch::Sender<Option<Arc<ControlReply>>>,
    },
    /// 已有原回执：原样返回，**绝不再执行一次**。
    Duplicate(Arc<ControlReply>),
    /// 同设备的另一个连接正在执行同一个 `command_id`：等它的结果。
    Wait(watch::Receiver<Option<Arc<ControlReply>>>),
}

struct TableInner {
    slots: BTreeMap<ReceiptKey, ReceiptSlot>,
    completed_order: VecDeque<ReceiptKey>,
}

/// 按 `(DeviceKey, command_id)` 去重且有界的回执表。
pub(crate) struct ReceiptTable {
    inner: Mutex<TableInner>,
}

impl ReceiptTable {
    /// 建表。
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(TableInner {
                slots: BTreeMap::new(),
                completed_order: VecDeque::new(),
            }),
        }
    }

    /// 开始一次调用。
    pub(crate) fn begin(&self, device: &DeviceKey, command_id: &str) -> ReceiptStart {
        let key = (device.clone(), command_id.to_string());
        let mut inner = lock(&self.inner);
        match inner.slots.get(&key) {
            Some(ReceiptSlot::Done(reply)) => ReceiptStart::Duplicate(Arc::clone(reply)),
            Some(ReceiptSlot::InFlight(receiver)) => ReceiptStart::Wait(receiver.clone()),
            None => {
                let (publish, receiver) = watch::channel(None);
                inner.slots.insert(key, ReceiptSlot::InFlight(receiver));
                ReceiptStart::Fresh { publish }
            }
        }
    }

    /// 落库并唤醒等待者（超出 [`MAX_COMPLETED_RECEIPTS`] 按 FIFO 淘汰最老完成项）。
    pub(crate) fn finish(
        &self,
        device: &DeviceKey,
        command_id: &str,
        reply: ControlReply,
        publish: &watch::Sender<Option<Arc<ControlReply>>>,
    ) -> Arc<ControlReply> {
        let key = (device.clone(), command_id.to_string());
        let reply = Arc::new(reply);
        {
            let mut inner = lock(&self.inner);
            inner
                .slots
                .insert(key.clone(), ReceiptSlot::Done(Arc::clone(&reply)));
            inner.completed_order.push_back(key);
            while inner.completed_order.len() > MAX_COMPLETED_RECEIPTS {
                if let Some(evict) = inner.completed_order.pop_front()
                    && matches!(inner.slots.get(&evict), Some(ReceiptSlot::Done(_)))
                {
                    inner.slots.remove(&evict);
                }
            }
        }
        publish.send_replace(Some(Arc::clone(&reply)));
        reply
    }

    /// 领导者异常退出：把在飞槽位摘掉，让后来者重新执行。
    pub(crate) fn abandon(&self, device: &DeviceKey, command_id: &str) {
        let key = (device.clone(), command_id.to_string());
        let mut inner = lock(&self.inner);
        if matches!(inner.slots.get(&key), Some(ReceiptSlot::InFlight(_))) {
            inner.slots.remove(&key);
        }
    }
}
