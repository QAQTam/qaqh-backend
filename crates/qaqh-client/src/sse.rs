//! SSE stream reader: frame parsing, cursor tracking, idle timeout, reconnect.

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use tokio::sync::watch;

use crate::error::{ClientError, Result};
use crate::session::RingingSession;
use crate::sse_decoder::SseDecoder;
use crate::types::{Channel, ChannelStatus, SseFrame};

const SSE_IDLE_TIMEOUT: Duration = Duration::from_secs(45);
const RETRY_BASE_MS: u64 = 1_000;
const RETRY_MAX_MS: u64 = 30_000;

/// Callbacks for one channel stream.
pub struct StreamHandlers {
    pub on_batch: std::sync::Arc<dyn Fn(crate::types::EventBatch) + Send + Sync>,
    pub on_status: std::sync::Arc<dyn Fn(ChannelStatus) + Send + Sync>,
    pub on_reset: Option<std::sync::Arc<dyn Fn(crate::types::ResetRequired) + Send + Sync>>,
    /// 见 `crate::ClientHandlers::on_liveness`：收到字节即报活（含被丢弃的 keepalive）。
    pub on_liveness: std::sync::Arc<dyn Fn() + Send + Sync>,
}

/// One SSE channel with independent cursor, reconnect backoff and idle timeout.
/// Cursor/epoch come from the shared session so `Last-Event-ID` stays coherent.
///
/// Endpoint and Bearer token are read from the session on **every** connect:
/// a daemon restart changes both (random port, random token), so baking them in
/// at construction would leave the stream retrying a dead endpoint forever.
pub struct ChannelStream {
    channel: Channel,
    http: reqwest::Client,
    handlers: StreamHandlers,
    session: Arc<RingingSession>,
    /// (server_epoch, client_session_id) — read on each connect. `None` until
    /// the session is negotiated; updated by lease re-negotiation.
    session_ctx: watch::Receiver<Option<(String, String)>>,
    /// Cursor of the last accepted frame (per channel).
    cursor: u64,
    /// Epoch of the last successful connect. A daemon restart swaps the epoch,
    /// and frame ids are `{epoch}:{channel}:{seq}` on a **fresh** sequence — so
    /// carrying the old cursor across an epoch change makes the stream resume
    /// at a position the new epoch never had (silent black-out: the daemon
    /// replays nothing, the client still reports `Open`). Zero it instead,
    /// mirroring [`crate::timeline::TimelineStream`].
    last_epoch: Option<String>,
}

impl ChannelStream {
    pub fn new(
        channel: Channel,
        http: reqwest::Client,
        handlers: StreamHandlers,
        session: Arc<RingingSession>,
    ) -> Self {
        let session_ctx = session.session_ctx_rx();
        Self {
            channel,
            http,
            handlers,
            session,
            session_ctx,
            cursor: 0,
            last_epoch: None,
        }
    }

    /// Run the connect loop until `stop` is signalled. Never returns an error
    /// to the caller unless the stream is stopped.
    pub async fn run(&mut self, mut stop: watch::Receiver<bool>) {
        let mut retry_ms = RETRY_BASE_MS;
        while !*stop.borrow() {
            match self.connect_once(&mut stop, &mut retry_ms).await {
                Ok(()) => {
                    // Clean stream end: reconnect without backoff reset (mirrors TS).
                }
                Err(err) => {
                    if *stop.borrow() {
                        return;
                    }
                    (self.handlers.on_status)(ChannelStatus::Reconnecting {
                        retry_ms,
                        last_cursor: self.cursor,
                    });
                    log::warn!(
                        "[qaqh-client] SSE {} reconnect in {retry_ms}ms: {err}",
                        self.channel.as_str()
                    );
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_millis(retry_ms)) => {}
                        _ = stop.changed() => return,
                    }
                    retry_ms = std::cmp::min(retry_ms * 2, RETRY_MAX_MS);
                }
            }
        }
    }

    async fn connect_once(
        &mut self,
        stop: &mut watch::Receiver<bool>,
        retry_ms: &mut u64,
    ) -> Result<()> {
        (self.handlers.on_status)(ChannelStatus::Connecting);
        let Some((server_epoch, client_session_id)) = self.session_ctx.borrow_and_update().clone()
        else {
            return Err(ClientError::Negotiation("session not open".into()));
        };

        // 每次连接都取当前凭据：daemon 重启换了端口/token 时，正在重连的
        // 流必须用新值，否则永远打不到活着的 daemon。
        let creds = self.session.credentials();
        let path = format!("/ringing/v1/events/{}", self.channel.as_str());
        let url = format!("{}{path}", creds.base_url);

        Self::reconcile_epoch(&mut self.last_epoch, &mut self.cursor, &server_epoch);

        let mut request = self
            .http
            .get(&url)
            .bearer_auth(&creds.token)
            .header("X-QAQH-Client-Session-Id", &client_session_id)
            .header("Accept", "text/event-stream");
        if self.cursor > 0 {
            request = request.header(
                "Last-Event-ID",
                format!("{server_epoch}:{}:{}", self.channel.as_str(), self.cursor),
            );
        }

        let response = request.send().await?;
        if !response.status().is_success() {
            return Err(ClientError::Http {
                status: response.status().as_u16(),
                path,
            });
        }
        // BUG-2026-09-12-10：连接成功即复位退避——此前 retry_ms 只增不减，
        // 一次抖动后会永久固定在上限（30s），流在长时间窗口内不再补帧。
        *retry_ms = RETRY_BASE_MS;
        (self.handlers.on_status)(ChannelStatus::Open {
            server_epoch: server_epoch.clone(),
            cursor: self.cursor,
        });

        let mut stream = response.bytes_stream();
        let mut decoder = SseDecoder::new();
        let idle = tokio::time::sleep(SSE_IDLE_TIMEOUT);
        tokio::pin!(idle);
        // BUG-2026-09-12-10：监听租约重新协商——旧 cs 的连接会被服务端静默
        // 跳过所有事件（伪健康黑障，状态仍报 Open），必须主动断开并用新 cs
        // 重建过滤，而不是等某个事件才可能被踢。
        let mut ctx = self.session_ctx.clone();

        loop {
            tokio::select! {
                changed = ctx.changed() => {
                    if changed.is_err() {
                        return Ok(()); // session dropped: nothing to reconnect against
                    }
                    return Err(ClientError::Negotiation(
                        "lease re-negotiated; reconnecting with new session".into(),
                    ));
                }
                _ = stop.changed() => {
                    return Ok(()); // stopped: exit loop cleanly
                }
                _ = &mut idle => {
                    return Err(ClientError::Transport("SSE idle timeout".into()));
                }
                chunk = stream.next() => {
                    match chunk {
                        Some(Ok(bytes)) => {
                            idle.as_mut().reset(tokio::time::Instant::now() + SSE_IDLE_TIMEOUT);
                            // 收到字节 = daemon 还活着。**必须在解码之前报**：
                            // keepalive 是注释行，解码器会直接丢掉它（见
                            // `sse_decoder.rs`），而它恰恰是空闲期唯一的存活证据。
                            (self.handlers.on_liveness)();
                            decoder.push(&bytes);
                            self.drain_frames(&mut decoder, &server_epoch)?;
                        }
                        Some(Err(err)) => {
                            return Err(ClientError::Transport(format!("SSE read: {err}")));
                        }
                        None => {
                            return Err(ClientError::Transport("SSE stream ended".into()));
                        }
                    }
                }
            }
        }
    }

    /// epoch 变化（daemon 重启，或租约重协商换了 epoch）→ 旧 cursor 语义失效，
    /// 必须归零后再续传。
    ///
    /// 帧 id 是 `{epoch}:{channel}:{seq}`，两个 epoch 的 seq 是**各自独立**的
    /// 序列。带着旧 cursor 去新 epoch 续传，等于停在一个新 epoch 从未有过的
    /// 位置：daemon 既不补帧也不报错，客户端状态却仍是 `Open`——伪健康黑障
    /// （T-04 / D-3 的不变式）。
    ///
    /// 抽成纯函数是为了可回归测试：真正的 `connect_once` 需要一个活着的 daemon
    /// 才能走到这一步。
    fn reconcile_epoch(last_epoch: &mut Option<String>, cursor: &mut u64, server_epoch: &str) {
        if last_epoch.as_deref() != Some(server_epoch) {
            *cursor = 0;
            *last_epoch = Some(server_epoch.to_string());
        }
    }

    /// 消费解码器中所有已完整的帧并分发。
    fn drain_frames(&mut self, decoder: &mut SseDecoder, server_epoch: &str) -> Result<()> {
        crate::sse_decoder::drain_frames(decoder, server_epoch, |frame, epoch| {
            self.dispatch(frame, epoch)
        })
    }

    fn dispatch(&mut self, frame: SseFrame, server_epoch: &str) -> Result<()> {
        // BUG-2026-09-12-11：服务端因事件缓冲溢出（Lagged）而终止流时发送的
        // 终止帧——归一为传输错误，走退避重连（而不是被当成坏信封）。
        if frame.event_type == "ringing.stream_terminated" {
            let code = serde_json::from_str::<serde_json::Value>(frame.data.trim())
                .ok()
                .and_then(|v| v.get("code").and_then(|c| c.as_str()).map(str::to_string))
                .unwrap_or_else(|| "unknown".into());
            return Err(ClientError::Transport(format!(
                "server terminated stream ({code}); reconnecting"
            )));
        }
        if frame.event_type == "ringing.reset_required" {
            let reset: crate::types::ResetRequired = serde_json::from_str(frame.data.trim())
                .map_err(|e| ClientError::Protocol(format!("bad reset_required: {e}")))?;
            if let Some(on_reset) = &self.handlers.on_reset {
                on_reset(reset);
            }
            return Ok(());
        }

        let envelope: crate::types::RingingEventEnvelope = serde_json::from_str(frame.data.trim())
            .map_err(|e| ClientError::Protocol(format!("bad envelope: {e}")))?;
        crate::types::validate_envelope(&envelope, self.channel).map_err(ClientError::Protocol)?;

        // Cursor must match the frame id exactly; only accepted envelopes advance it.
        if let Some(frame_cursor) = crate::types::cursor_from_sse_id(&frame.id, self.channel) {
            if envelope.stream_seq != frame_cursor {
                return Err(ClientError::Protocol(format!(
                    "cursor mismatch: envelope stream_seq {} != SSE id seq {frame_cursor}",
                    envelope.stream_seq
                )));
            }
            self.cursor = frame_cursor;
        }
        let batch =
            crate::types::envelope_to_batch(self.channel, envelope, server_epoch.to_string());
        (self.handlers.on_batch)(batch);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    //! SSE 终止帧归一（BUG-2026-09-12-11 遗留 / issue #35）。
    //!
    //! daemon 侧因 live 广播 `Lagged` 发出的 `ringing.stream_terminated`
    //! 必须被**归一为 `Transport` 错误**（走退避重连），而不是被当成坏信封
    //! 落成 `Protocol`——后者会让客户端在实现不变的情况下永远重连不上。

    use super::*;
    use crate::types::Channel;

    fn stream() -> ChannelStream {
        ChannelStream::new(
            Channel::Conversation,
            reqwest::Client::new(),
            StreamHandlers {
                on_batch: std::sync::Arc::new(|_| {}),
                on_status: std::sync::Arc::new(|_| {}),
                on_reset: None,
                on_liveness: std::sync::Arc::new(|| {}),
            },
            Arc::new(RingingSession::new(
                "http://127.0.0.1:1".into(),
                "t".into(),
                reqwest::Client::new(),
            )),
        )
    }

    fn frame(event_type: &str, data: &str) -> SseFrame {
        SseFrame {
            id: String::new(),
            event_type: event_type.into(),
            data: data.into(),
        }
    }

    /// daemon Lagged 终止帧 → Transport（可重连），不是 Protocol。
    #[test]
    fn lagged_termination_frame_normalizes_to_transport() {
        let mut s = stream();
        let err = s
            .dispatch(
                frame(
                    "ringing.stream_terminated",
                    r#"{"code":"lagged","channel":"conversation","skipped":7,"message":"overflow"}"#,
                ),
                "epoch-1",
            )
            .expect_err("termination frame must end the stream");
        assert!(
            matches!(err, ClientError::Transport(_)),
            "must normalize to Transport: {err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("lagged"),
            "message must carry the server code: {msg}"
        );
    }

    /// 载荷缺 `code` 时退化为 unknown，仍按 Transport 处理（不得 panic）。
    #[test]
    fn termination_frame_without_code_still_transport() {
        let mut s = stream();
        let err = s
            .dispatch(frame("ringing.stream_terminated", "{}"), "epoch-1")
            .expect_err("termination frame must end the stream");
        assert!(matches!(err, ClientError::Transport(_)), "{err:?}");
        assert!(err.to_string().contains("unknown"), "{err}");
    }

    /// **epoch 变化必须归零 cursor**（T-04 / D-3 不变式在频道流上的落点）。
    ///
    /// 此前 `ChannelStream` 根本不比较 epoch：daemon 重启后带着旧 cursor 去新
    /// epoch 续传，停在一个该 epoch 从未有过的位置，daemon 不补帧也不报错，
    /// 客户端状态仍报 `Open`——伪健康黑障。`TimelineStream` 早已这么做
    /// （`timeline.rs` 的 re-baseline），频道流是漏掉的一半。
    #[test]
    fn epoch_change_resets_cursor() {
        let mut last = Some("ep-1".to_string());
        let mut cursor = 42;
        ChannelStream::reconcile_epoch(&mut last, &mut cursor, "ep-2");
        assert_eq!(cursor, 0, "epoch 变化后旧 cursor 语义失效，必须归零");
        assert_eq!(last.as_deref(), Some("ep-2"));
    }

    /// 同 epoch 内重连（网络抖动、空闲判死）**必须保留** cursor——否则每次
    /// 抖动都从 0 重放整条频道，越抖越慢。
    #[test]
    fn same_epoch_keeps_cursor() {
        let mut last = Some("ep-1".to_string());
        let mut cursor = 42;
        ChannelStream::reconcile_epoch(&mut last, &mut cursor, "ep-1");
        assert_eq!(cursor, 42, "同 epoch 内重连不得丢弃续传位置");
    }

    /// 首次连接：cursor 本就是 0，记为「已见 ep-1」。之后再遇到 ep-1 不重置。
    #[test]
    fn first_connect_records_epoch_without_side_effects() {
        let mut last: Option<String> = None;
        let mut cursor = 0;
        ChannelStream::reconcile_epoch(&mut last, &mut cursor, "ep-1");
        assert_eq!(cursor, 0);
        assert_eq!(last.as_deref(), Some("ep-1"));

        let mut cursor = 7;
        ChannelStream::reconcile_epoch(&mut last, &mut cursor, "ep-1");
        assert_eq!(cursor, 7, "已见过的 epoch 不得反复归零");
    }

    /// 终止帧不得推进 cursor：服务端缓冲溢出后 cursor 可能已跨过丢弃区间，
    /// 推进它会让下一次 `Last-Event-ID` 停在一个未覆盖的位置。
    #[test]
    fn termination_frame_does_not_advance_cursor() {
        let mut s = stream();
        s.cursor = 42;
        let _ = s.dispatch(
            frame(
                "ringing.stream_terminated",
                r#"{"code":"lagged","channel":"conversation","skipped":7}"#,
            ),
            "epoch-1",
        );
        assert_eq!(s.cursor, 42, "终止帧不得推进 cursor");
    }

    /// 对照：终止帧分支**先于**信封解析——即便 data 不是合法信封也不得
    /// 落成 Protocol("bad envelope")。
    #[test]
    fn termination_frame_is_not_misparsed_as_bad_envelope() {
        let mut s = stream();
        let err = s
            .dispatch(
                frame("ringing.stream_terminated", "not json at all\n"),
                "epoch-1",
            )
            .expect_err("must error");
        assert!(matches!(err, ClientError::Transport(_)), "{err:?}");
        assert!(!err.to_string().contains("bad envelope"), "{err}");
    }
}
