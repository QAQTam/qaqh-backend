//! SSE stream reader: frame parsing, cursor tracking, idle timeout, reconnect.

use std::time::Duration;

use futures_util::StreamExt;
use tokio::sync::watch;

use crate::error::{ClientError, Result};
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
}

/// One SSE channel with independent cursor, reconnect backoff and idle timeout.
/// Cursor/epoch come from the shared session so `Last-Event-ID` stays coherent.
pub struct ChannelStream {
    url: String,
    token: String,
    channel: Channel,
    http: reqwest::Client,
    handlers: StreamHandlers,
    /// (server_epoch, client_session_id) — read on each connect. `None` until
    /// the session is negotiated; updated by lease re-negotiation.
    session_ctx: watch::Receiver<Option<(String, String)>>,
    /// Cursor of the last accepted frame (per channel).
    cursor: u64,
}

impl ChannelStream {
    pub fn new(
        url: String,
        token: String,
        channel: Channel,
        http: reqwest::Client,
        handlers: StreamHandlers,
        session_ctx: watch::Receiver<Option<(String, String)>>,
    ) -> Self {
        Self {
            url,
            token,
            channel,
            http,
            handlers,
            session_ctx,
            cursor: 0,
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

        let mut request = self
            .http
            .get(&self.url)
            .bearer_auth(&self.token)
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
                path: self.url.clone(),
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
        let (_tx, rx) = watch::channel(Some(("epoch-1".into(), "cs-1".into())));
        ChannelStream::new(
            "http://127.0.0.1:1/ringing/v1/events/conversation".into(),
            "t".into(),
            Channel::Conversation,
            reqwest::Client::new(),
            StreamHandlers {
                on_batch: std::sync::Arc::new(|_| {}),
                on_status: std::sync::Arc::new(|_| {}),
                on_reset: None,
            },
            rx,
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
