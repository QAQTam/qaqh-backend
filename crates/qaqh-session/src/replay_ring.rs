//! 会话事件有界回放尾：用于重连断线追帧与防止高频大帧撑爆内存。

use std::collections::VecDeque;
use std::sync::Mutex;

use qaqh_ringing_contract::events::{SessionEvent, SessionEventFrame};
use qaqh_ringing_contract::fact::ContentValue;

use crate::owner::EVENT_CHANNEL_CAPACITY;

/// 回放尾预算(P0-2026-11-10 报告 §6 H1'):帧数覆盖广播深度,字节预算
/// 防 exec 大帧撑爆内存。超出覆盖范围的远古游标仍走快照 resync——回放尾
/// 消灭的是「缺口即掐流」的活锁,不是 resync 本身。
const REPLAY_FRAME_CAP: usize = 2 * EVENT_CHANNEL_CAPACITY;
const REPLAY_BYTE_CAP: usize = 4 * 1024 * 1024;

/// 会话事件回放尾。定序纪律:「先入环、后广播」(`emit`)与「先 grab receiver、
/// 后读环」(`attach_after`)交错时,每帧要么已在环内被 replay 覆盖,要么其
/// send 晚于 receiver 订阅由转发直送;两处重合同时由转发侧水位去重。
/// 论证见 P0-2026-11-10 报告 §6 H1'。
#[derive(Default)]
pub struct ReplayRing {
    state: Mutex<RingState>,
}

#[derive(Default)]
struct RingState {
    frames: VecDeque<SessionEventFrame>,
    bytes: usize,
}

/// 一帧的粗算占用:结构体本体 + 主要堆文本。预算是护栏不是精确账本——
/// 低速率大条目(TimelineAppended 等)走 serde 实测,高频 delta 直取字段。
fn frame_footprint(frame: &SessionEventFrame) -> usize {
    std::mem::size_of::<SessionEventFrame>() + event_heap_bytes(&frame.event)
}

fn event_heap_bytes(event: &SessionEvent) -> usize {
    match event {
        SessionEvent::ToolOutputDelta { text, .. }
        | SessionEvent::ToolArgumentsDelta { text, .. }
        | SessionEvent::MessageDelta { text, .. }
        | SessionEvent::ExecSessionOutputDelta { text, .. } => text.len(),
        SessionEvent::ToolFinished { output, diff, .. } => {
            let content_size = |value: &Option<ContentValue>| {
                value
                    .as_ref()
                    .map(|value| match value {
                        ContentValue::Inline { text } => text.len(),
                        ContentValue::Ref { .. } => 64,
                        ContentValue::Unavailable(_) => 8,
                    })
                    .unwrap_or_default()
            };
            content_size(output) + content_size(diff)
        }
        SessionEvent::ExecSessionStarted { session }
        | SessionEvent::ExecSessionStateChanged { session }
        | SessionEvent::ExecSessionFinished { session } => session.output_preview.len(),
        SessionEvent::TimelineAppended { entry } => serde_json::to_string(entry)
            .map(|json| json.len())
            .unwrap_or_default(),
        SessionEvent::TimelineTruncated {
            removed_turn_ids, ..
        } => removed_turn_ids
            .iter()
            .map(|turn| turn.as_ref().len())
            .sum(),
        _ => 0,
    }
}

impl ReplayRing {
    pub(crate) fn push(&self, frame: SessionEventFrame) {
        let footprint = frame_footprint(&frame);
        let mut ring = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        ring.bytes += footprint;
        ring.frames.push_back(frame);
        while ring.frames.len() > REPLAY_FRAME_CAP
            || (ring.frames.len() > 1 && ring.bytes > REPLAY_BYTE_CAP)
        {
            let Some(evicted) = ring.frames.pop_front() else {
                break;
            };
            ring.bytes = ring.bytes.saturating_sub(frame_footprint(&evicted));
        }
    }

    /// `after_seq` = 客户端游标(exclusive 语义,同契约文档 SessionEventFrame
    /// 头注:「跳过 `seq < state.seq`,处理 `seq >= state.seq`」)。
    /// `None` = 无游标 live 直推(不回放既有帧)。
    pub(crate) fn slice_after(&self, after_seq: Option<u64>) -> ReplaySlice {
        let ring = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(after) = after_seq else {
            return ReplaySlice {
                frames: Vec::new(),
                watermark: None,
                gap: false,
            };
        };
        let (Some(head), Some(back)) = (ring.frames.front(), ring.frames.back()) else {
            // 本 epoch 内一帧未发:无东西可补,live 直推。
            return ReplaySlice {
                frames: Vec::new(),
                watermark: None,
                gap: false,
            };
        };
        if head.seq > after {
            // 游标与环首之间的事件已被淘汰——回放尾兜不住,回到快照 resync。
            return ReplaySlice {
                frames: Vec::new(),
                watermark: None,
                gap: true,
            };
        }
        let frames: Vec<SessionEventFrame> = ring
            .frames
            .iter()
            .filter(|frame| frame.seq >= after)
            .cloned()
            .collect();
        let watermark = frames.last().map(|frame| frame.seq);
        debug_assert!(watermark.is_none_or(|last| last == back.seq));
        ReplaySlice {
            frames,
            watermark,
            gap: false,
        }
    }
}

pub struct ReplaySlice {
    pub frames: Vec<SessionEventFrame>,
    /// 本次补发的最大 seq;转发端跳过 `seq <= watermark` 的 receiver 重帧。
    pub watermark: Option<u64>,
    /// 游标早于回放尾覆盖范围——只能快照重同步。
    pub gap: bool,
}

/// 读面原子 attach 结果:receiver 已先于环读取建立,补发与 live 无缝且无重。
pub struct OwnerAttach {
    pub replay: ReplaySlice,
    pub receiver: tokio::sync::broadcast::Receiver<SessionEventFrame>,
}
