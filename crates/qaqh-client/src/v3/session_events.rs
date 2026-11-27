//! 会话事件流（**前端唯一状态真相源**）。
//!
//! ## 这份句柄提供什么
//!
//! - `next_frame()`：拿下一帧（原生 async）；
//! - `into_frame_stream()`：变成 `Stream<Item = Result<SessionEventFrame, _>>`，
//!   遇到流关闭或错误吐出终态后立刻结束，绝不无限循环；
//! - [`SessionStreamStats`]：协议层自证需要的计数（帧数、`resync_cycles`、
//!   增量字节、乱序次数……）——**常数内存**，不在客户端缓存第二份文本全集。
//!
//! ## `seq` 只增不减；缺口只可能是"合帧"或"重同步"
//!
//! 合帧会把多条 `Replaceable` 增量并成一帧，带**这批里最大的 `seq`**，
//! 所以实时流上看到的 `seq` 是"非递减、可以跳号"的。跳号不代表丢内容——
//! 内容被并进了前一帧；真正的丢内容只有一种表现：`ResyncRequired`。

use futures_util::Stream;
use qaqh_ringing_v3::{DeliveryClass, SessionEvent, SessionEventFrame, StreamAttach};
use qaqh_v3_transport::{FrameIoError, read_frame};
use tokio::io::AsyncRead;

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

/// 一条会话事件流（O(1) 内存，不维护第二份会话文本缓存）。
pub struct SessionEventsStream<S> {
    stream: S,
    attach: StreamAttach,
    stats: SessionStreamStats,
    /// 已收口的回合数（`TurnFinished` 计数）。
    pub finished_turns: u64,
}

struct StreamState<S> {
    inner: SessionEventsStream<S>,
    ended: bool,
}

impl<S> SessionEventsStream<S> {
    pub(super) fn new(stream: S, attach: StreamAttach) -> Self {
        Self {
            stream,
            attach,
            stats: SessionStreamStats::default(),
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

    fn observe(&mut self, frame: &SessionEventFrame) {
        self.stats.frames += 1;
        if frame.meta.seq < self.stats.last_seq {
            self.stats.ordering_violations += 1;
        }
        self.stats.last_seq = self.stats.last_seq.max(frame.meta.seq);
        match &frame.event {
            SessionEvent::AnswerDelta { text, .. } | SessionEvent::ReasoningDelta { text, .. } => {
                self.stats.delta_bytes += text.len() as u64;
            }
            SessionEvent::ExecOutputDelta { text, .. } => {
                self.stats.delta_bytes += text.len() as u64;
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

    /// 变成原生 `Stream`（遇到 EOF 或错误后在下一拍返回 `None`，绝不无限自旋）。
    pub fn into_frame_stream(
        self,
    ) -> impl Stream<Item = Result<SessionEventFrame, ClientError>> {
        futures_util::stream::unfold(
            StreamState {
                inner: self,
                ended: false,
            },
            |mut state| async move {
                if state.ended {
                    return None;
                }
                match state.inner.next_frame().await {
                    Ok(frame) => Some((Ok(frame), state)),
                    Err(error) => {
                        state.ended = true;
                        Some((Err(error), state))
                    }
                }
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use qaqh_core_vocab::SessionId;
    use qaqh_ringing_v3::{FrameMeta, StreamKind};
    use qaqh_v3_transport::write_frame;

    #[tokio::test]
    async fn frame_stream_terminates_cleanly_after_server_eof() {
        let (client, mut server) = tokio::io::duplex(4096);
        let session_id = SessionId::new("ses-stream-eof").expect("id");
        let frame = SessionEventFrame {
            meta: FrameMeta {
                seq: 1,
                server_ts_ms: 1,
                delivery: DeliveryClass::Reliable,
            },
            event: SessionEvent::CatalogChanged {
                session_id: session_id.clone(),
            },
        };
        write_frame(&mut server, &frame).await.expect("write");
        drop(server);

        let attach = StreamAttach {
            stream: StreamKind::SessionEvents {
                session_id,
                since_seq: 0,
            },
            seq: 1,
            timeline_epoch: 1,
        };
        let mut stream = std::pin::pin!(SessionEventsStream::new(client, attach).into_frame_stream());
        let first = stream.next().await.expect("第一帧必须到达").expect("ok");
        assert_eq!(first, frame);
        let second = stream.next().await.expect("流结束信号");
        assert!(second.is_err(), "服务端关闭后应给出一次错误终态");
        assert!(
            stream.next().await.is_none(),
            "终态之后必须返回 None，绝不死循环"
        );
    }
}
