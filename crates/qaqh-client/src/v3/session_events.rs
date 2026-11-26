//! 会话事件流（**前端唯一状态真相源**）。
//!
//! ## 这份句柄提供什么
//!
//! - `next_frame()`：拿下一帧（原生 async）；
//! - `into_frame_stream()`：变成 `Stream<Item = Result<SessionEventFrame, _>>`，
//!   直接喂给任何 `StreamExt` 组合子；
//! - [`SessionStreamStats`]：协议层自证需要的计数（帧数、`resync_cycles`、
//!   增量字节、乱序次数……）——**不是业务状态**，只是这条流的体检数据；
//! - 按块累积的答案文本：让调用方能验证"合帧没有吃掉任何字节"。
//!
//! ## `seq` 只增不减；缺口只可能是"合帧"或"重同步"
//!
//! 合帧会把多条 `Replaceable` 增量并成一帧，带**这批里最大的 `seq`**，
//! 所以实时流上看到的 `seq` 是"非递减、可以跳号"的。跳号不代表丢内容——
//! 内容被并进了前一帧；真正的丢内容只有一种表现：`ResyncRequired`。

use std::collections::BTreeMap;

use futures_util::Stream;
use qaqh_core_vocab::BlockId;
use qaqh_ringing_v3::{DeliveryClass, SessionEvent, SessionEventFrame, StreamAttach};
use tokio::io::AsyncRead;

use super::framing::{FrameIoError, read_frame};
use super::ClientError;

/// 会话事件流的体检数据。
#[derive(Debug, Default, Clone)]
pub struct SessionStreamStats {
    /// 收到的帧数。
    pub frames: u64,
    /// 收到的 `ResyncRequired` 次数（`0` 表示**从未需要重同步**）。
    pub resync_cycles: u64,
    /// 收到的最后一帧 `seq`。
    pub last_seq: u64,
    /// 收到的 `Replaceable` 增量文本字节数。
    pub delta_bytes: u64,
    /// 收到的 `Reliable` 帧数。
    pub reliable_frames: u64,
    /// `seq` 倒退次数（必须恒为 `0`）。
    pub ordering_violations: u64,
}

/// 一条会话事件流。
pub struct SessionEventsStream<S> {
    stream: S,
    attach: StreamAttach,
    stats: SessionStreamStats,
    /// 按块 id 累积的答案文本（守卫用它做"字节守恒"断言）。
    answers: BTreeMap<String, String>,
    /// 按块 id 累积的推理文本。
    reasoning: BTreeMap<String, String>,
    /// 按 exec id 累积的输出文本。
    exec_output: BTreeMap<String, String>,
    /// 已收口的回合数（`TurnFinished` 计数）。
    pub finished_turns: u64,
}

impl<S> SessionEventsStream<S> {
    pub(super) fn new(stream: S, attach: StreamAttach) -> Self {
        Self {
            stream,
            attach,
            stats: SessionStreamStats::default(),
            answers: BTreeMap::new(),
            reasoning: BTreeMap::new(),
            exec_output: BTreeMap::new(),
            finished_turns: 0,
        }
    }

    /// 服务端给出的 attach 点（权威 `seq` + 时间线代际）。
    pub fn attach(&self) -> &StreamAttach {
        &self.attach
    }

    /// 体检数据。
    pub fn stats(&self) -> &SessionStreamStats {
        &self.stats
    }

    /// 某一块的答案全文（按收到的增量拼接）。
    pub fn answer_text(&self, block_id: &BlockId) -> &str {
        self.answers
            .get(block_id.as_str())
            .map(String::as_str)
            .unwrap_or("")
    }

    /// 某一块的推理全文。
    pub fn reasoning_text(&self, block_id: &BlockId) -> &str {
        self.reasoning
            .get(block_id.as_str())
            .map(String::as_str)
            .unwrap_or("")
    }

    /// 某个 exec 会话的输出全文。
    pub fn exec_output(&self, exec_session_id: &str) -> &str {
        self.exec_output
            .get(exec_session_id)
            .map(String::as_str)
            .unwrap_or("")
    }

    fn observe(&mut self, frame: &SessionEventFrame) {
        self.stats.frames += 1;
        if frame.meta.seq < self.stats.last_seq {
            self.stats.ordering_violations += 1;
        }
        self.stats.last_seq = self.stats.last_seq.max(frame.meta.seq);
        match &frame.event {
            SessionEvent::AnswerDelta { block_id, text, .. } => {
                self.stats.delta_bytes += text.len() as u64;
                self.answers
                    .entry(block_id.as_str().to_string())
                    .or_default()
                    .push_str(text);
            }
            SessionEvent::ReasoningDelta { block_id, text, .. } => {
                self.stats.delta_bytes += text.len() as u64;
                self.reasoning
                    .entry(block_id.as_str().to_string())
                    .or_default()
                    .push_str(text);
            }
            SessionEvent::ExecOutputDelta {
                exec_session_id,
                text,
                spilled,
                ..
            } => {
                self.stats.delta_bytes += text.len() as u64;
                if !spilled {
                    self.exec_output
                        .entry(exec_session_id.as_str().to_string())
                        .or_default()
                        .push_str(text);
                }
            }
            SessionEvent::TurnFinished { .. } => self.finished_turns += 1,
            SessionEvent::ResyncRequired { .. } => self.stats.resync_cycles += 1,
            _ => {}
        }
        if frame.meta.delivery == DeliveryClass::Reliable {
            self.stats.reliable_frames += 1;
        }
    }
}

impl<S> SessionEventsStream<S>
where
    S: AsyncRead + Unpin,
{
    /// 拿下一帧。
    pub async fn next_frame(&mut self) -> Result<SessionEventFrame, ClientError> {
        let frame: SessionEventFrame = match read_frame(&mut self.stream).await {
            Ok(frame) => frame,
            Err(FrameIoError::Eof) => {
                return Err(ClientError::Protocol("会话事件流被服务端关闭".to_string()));
            }
            Err(error) => return Err(ClientError::Frame(error)),
        };
        self.observe(&frame);
        Ok(frame)
    }

    /// 变成原生 `Stream`（`futures_util::stream::unfold`：零手写状态机）。
    pub fn into_frame_stream(
        self,
    ) -> impl Stream<Item = Result<SessionEventFrame, ClientError>> {
        futures_util::stream::unfold(self, |mut handle| async move {
            let frame = handle.next_frame().await;
            Some((frame, handle))
        })
    }
}
