//! Per-session Ringing V1 timeline SSE stream.
//!
//! Mirrors the Ringing V1 timeline semantics (the original Electron reference
//! implementation `apps/desktop/electron/timelineClient.ts` was removed): one
//! transcript, one SSE stream, one monotonically increasing cursor
//! (`{epoch}:timeline:{seq}`).
//! Gap recovery re-fetches the authoritative snapshot and advances the
//! cursor to its watermark so `Last-Event-ID` never stalls.

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use tokio::sync::watch;

use crate::error::{ClientError, Result};
use crate::session::RingingSession;
use crate::sse_decoder::SseDecoder;
use crate::types::{SseFrame, TimelineEntry, TimelinePage, TimelineSseFrame, TimelineStatus};

const SSE_IDLE_TIMEOUT: Duration = Duration::from_secs(45);
const RETRY_BASE_MS: u64 = 1_000;
const RETRY_MAX_MS: u64 = 30_000;

/// One per-session timeline stream with independent cursor, reconnect backoff
/// and gap recovery. Created by `Client::activate_timeline` and run as a
/// background task; callbacks fire on the tokio side.
pub struct TimelineStream {
    base_url: String,
    token: String,
    seed: String,
    http: reqwest::Client,
    /// Read on every connect: (server_epoch, client_session_id).
    session: Arc<RingingSession>,
    on_entry: Arc<dyn Fn(String, TimelineEntry) + Send + Sync>,
    on_status: Arc<dyn Fn(TimelineStatus) + Send + Sync>,
    /// Forwarded on gap recovery: the fresh snapshot becomes the new baseline.
    on_snapshot: Arc<dyn Fn(TimelinePage) + Send + Sync>,
    /// Optional sink for `Client::timeline_status()` (also fed on exit).
    status_tx: Option<watch::Sender<Option<TimelineStatus>>>,
    /// Cursor of the last accepted entry (starts at the snapshot watermark).
    cursor: u64,
    /// Epoch of the last successful connect. A lease re-negotiation (renewal
    /// failure -> reopen) changes the server epoch; the old cursor is invalid
    /// against the new epoch (daemon treats a stale `Last-Event-ID` as 0 and
    /// replays from the head, which the cursor guard rejects as Protocol
    /// error — the exact reconnect-death loop this stream must break).
    last_epoch: Option<String>,
}

impl TimelineStream {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        base_url: String,
        token: String,
        seed: String,
        http: reqwest::Client,
        session: Arc<RingingSession>,
        on_entry: Arc<dyn Fn(String, TimelineEntry) + Send + Sync>,
        on_status: Arc<dyn Fn(TimelineStatus) + Send + Sync>,
        on_snapshot: Arc<dyn Fn(TimelinePage) + Send + Sync>,
        initial_cursor: u64,
        status_tx: Option<watch::Sender<Option<TimelineStatus>>>,
    ) -> Self {
        Self {
            base_url,
            token,
            seed,
            http,
            session,
            on_entry,
            on_status,
            on_snapshot,
            status_tx,
            cursor: initial_cursor,
            last_epoch: None,
        }
    }

    fn set_status(&self, status: TimelineStatus) {
        // `send_replace` updates the value even without receivers: the
        // `timeline_status()` query reads the sender-side value, and the
        // receiver may be dropped as soon as `activate_timeline` returns.
        if let Some(tx) = &self.status_tx {
            let _ = tx.send_replace(Some(status.clone()));
        }
        (self.on_status)(status);
    }

    /// Run the connect loop until `stop` (own handle) or `session_stop`
    /// (client-wide close) is signalled. Never returns an error to the caller
    /// unless the stream is stopped.
    pub async fn run(
        &mut self,
        mut stop: watch::Receiver<bool>,
        mut session_stop: watch::Receiver<bool>,
    ) {
        let mut retry_ms = RETRY_BASE_MS;
        while !*stop.borrow() && !*session_stop.borrow() {
            match self
                .connect_once(&mut stop, &mut session_stop, &mut retry_ms)
                .await
            {
                Ok(()) => {
                    // Clean stream end (stop signal): exit.
                    if *stop.borrow() || *session_stop.borrow() {
                        return;
                    }
                }
                Err(err) => {
                    if *stop.borrow() || *session_stop.borrow() {
                        return;
                    }
                    // A gap means the server journal no longer covers our
                    // cursor. Recover by fetching the authoritative snapshot:
                    // its watermark becomes the new cursor so the next
                    // reconnect resumes from a covered position. Without this,
                    // Last-Event-ID never advances and the client reconnects
                    // into the same gap forever (mirrors TS `onGap`).
                    if matches!(err, ClientError::TimelineGap { .. }) {
                        match self.recover_gap().await {
                            Ok(()) => log::info!(
                                "[qaqh-client] timeline {} gap recovered at cursor {}",
                                self.seed,
                                self.cursor
                            ),
                            Err(recovery_err) => log::warn!(
                                "[qaqh-client] timeline {} gap snapshot recovery failed: {recovery_err}",
                                self.seed
                            ),
                        }
                    }
                    self.set_status(TimelineStatus::Reconnecting {
                        seed: self.seed.clone(),
                        retry_ms,
                        cursor: self.cursor,
                    });
                    log::warn!(
                        "[qaqh-client] timeline {} reconnect in {retry_ms}ms: {err}",
                        self.seed
                    );
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_millis(retry_ms)) => {}
                        _ = stop.changed() => return,
                        _ = session_stop.changed() => return,
                    }
                    retry_ms = std::cmp::min(retry_ms * 2, RETRY_MAX_MS);
                }
            }
        }
        self.set_status(TimelineStatus::Closed {
            seed: self.seed.clone(),
            reason: "stopped".into(),
        });
    }

    async fn connect_once(
        &mut self,
        stop: &mut watch::Receiver<bool>,
        session_stop: &mut watch::Receiver<bool>,
        retry_ms: &mut u64,
    ) -> Result<()> {
        self.set_status(TimelineStatus::Connecting {
            seed: self.seed.clone(),
        });
        let state = self
            .session
            .state()
            .await
            .ok_or_else(|| ClientError::Negotiation("session not open".into()))?;

        // Lease re-negotiation swapped the epoch: re-baseline against the
        // authoritative snapshot so the reconnect cursor stays covered, then
        // forward the snapshot so listeners rebuild the transcript.
        if self.last_epoch.as_deref() != Some(state.server_epoch.as_str()) {
            let epoch_changed = self.last_epoch.is_some();
            self.last_epoch = Some(state.server_epoch.clone());
            if epoch_changed {
                match self.recover_gap().await {
                    Ok(()) => log::info!(
                        "[qaqh-client] timeline {} re-baselined after session re-negotiation (cursor {})",
                        self.seed,
                        self.cursor
                    ),
                    Err(recovery_err) => {
                        // 兜底：从 0 全量回放（daemon 按 0 处理旧 epoch 的
                        // Last-Event-ID），避免带着旧 cursor 连进 Protocol
                        // error 死循环。
                        log::warn!(
                            "[qaqh-client] timeline {} re-baseline failed ({recovery_err}); replaying from head",
                            self.seed
                        );
                        self.cursor = 0;
                    }
                }
            }
        }

        let path = format!("/ringing/v1/sessions/{}/timeline/events", self.seed);
        let mut request = self
            .http
            .get(format!("{}{path}", self.base_url))
            .bearer_auth(&self.token)
            .header("X-QAQH-Client-Session-Id", &state.client_session_id)
            .header("Accept", "text/event-stream");
        if self.cursor > 0 {
            request = request.header(
                "Last-Event-ID",
                format!("{}:timeline:{}", state.server_epoch, self.cursor),
            );
        }

        let response = request.send().await?;
        if !response.status().is_success() {
            return Err(ClientError::Http {
                status: response.status().as_u16(),
                path,
            });
        }
        // BUG-2026-09-12-10：连接成功即复位退避（与频道流同款修复）。
        *retry_ms = RETRY_BASE_MS;
        self.set_status(TimelineStatus::Open {
            seed: self.seed.clone(),
            server_epoch: state.server_epoch.clone(),
            cursor: self.cursor,
        });

        let mut stream = response.bytes_stream();
        let mut decoder = SseDecoder::new();
        let idle = tokio::time::sleep(SSE_IDLE_TIMEOUT);
        tokio::pin!(idle);
        // BUG-2026-09-12-10：租约重新协商后本连接携带的旧 cs 已失效（服务端
        // 按 cs 过滤/活跃性检查）——主动断开，重连时走 epoch 变更的 re-baseline。
        let mut ctx = self.session.session_ctx_rx();

        loop {
            tokio::select! {
                changed = ctx.changed() => {
                    if changed.is_err() {
                        return Ok(()); // session dropped: nothing to reconnect against
                    }
                    return Err(ClientError::Negotiation(
                        "lease re-negotiated; re-baselining timeline".into(),
                    ));
                }
                _ = stop.changed() => {
                    return Ok(()); // stopped: exit loop cleanly
                }
                _ = session_stop.changed() => {
                    return Ok(()); // client-wide close
                }
                _ = &mut idle => {
                    return Err(ClientError::Transport("timeline SSE idle timeout".into()));
                }
                chunk = stream.next() => {
                    match chunk {
                        Some(Ok(bytes)) => {
                            idle.as_mut().reset(tokio::time::Instant::now() + SSE_IDLE_TIMEOUT);
                            decoder.push(&bytes);
                            self.drain_frames(&mut decoder, &state.server_epoch)?;
                        }
                        Some(Err(err)) => {
                            return Err(ClientError::Transport(format!("timeline SSE read: {err}")));
                        }
                        None => {
                            return Err(ClientError::Transport("timeline SSE stream ended".into()));
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
        // BUG-2026-09-12-11：服务端 Lagged 终止帧（与频道流同协议）。
        if frame.event_type == "ringing.stream_terminated" {
            let code = serde_json::from_str::<serde_json::Value>(frame.data.trim())
                .ok()
                .and_then(|v| v.get("code").and_then(|c| c.as_str()).map(str::to_string))
                .unwrap_or_else(|| "unknown".into());
            return Err(ClientError::Transport(format!(
                "server terminated timeline stream ({code}); reconnecting"
            )));
        }
        let parsed: TimelineSseFrame = serde_json::from_str(frame.data.trim())
            .map_err(|e| ClientError::Protocol(format!("bad timeline frame: {e}")))?;
        if parsed.schema != qaqh_ringing::RINGING_SCHEMA
            || parsed.version != qaqh_ringing::RINGING_VERSION
            || parsed.seed != self.seed
            || parsed.server_epoch != server_epoch
        {
            return Err(ClientError::Protocol(
                "invalid Ringing V1 timeline SSE frame".into(),
            ));
        }
        if parsed.entry.timeline_seq <= self.cursor {
            return Err(ClientError::Protocol(format!(
                "timeline entry at/below cursor: {} <= {}",
                parsed.entry.timeline_seq, self.cursor
            )));
        }
        let expected_id = format!("{server_epoch}:timeline:{}", parsed.entry.timeline_seq);
        if !frame.id.is_empty() && frame.id != expected_id {
            return Err(ClientError::Protocol(
                "timeline SSE cursor/frame mismatch".into(),
            ));
        }
        if parsed.entry.timeline_seq != self.cursor + 1 {
            return Err(ClientError::TimelineGap {
                expected: self.cursor + 1,
                received: parsed.entry.timeline_seq,
            });
        }
        self.cursor = parsed.entry.timeline_seq;
        (self.on_entry)(self.seed.clone(), parsed.entry);
        Ok(())
    }

    /// Re-fetch the authoritative snapshot; its watermark becomes the new
    /// reconnect cursor. The snapshot is forwarded so the shell can rebuild
    /// the transcript (mirrors TS `onGap`).
    async fn recover_gap(&mut self) -> Result<()> {
        let state = self
            .session
            .state()
            .await
            .ok_or_else(|| ClientError::Negotiation("session not open".into()))?;
        let path = format!("/ringing/v1/sessions/{}/timeline", self.seed);
        let response = self
            .http
            .get(format!("{}{path}", self.base_url))
            .bearer_auth(&self.token)
            .header("X-QAQH-Client-Session-Id", &state.client_session_id)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(ClientError::Http {
                status: response.status().as_u16(),
                path,
            });
        }
        let page: TimelinePage = response.json().await?;
        page.validate_for(&self.seed)
            .map_err(ClientError::Protocol)?;
        self.cursor = page.snapshot.watermark;
        (self.on_snapshot)(page);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    //! Timeline SSE 终止帧归一（BUG-2026-09-12-11 遗留 / issue #35）。
    //!
    //! 与频道流同协议：daemon 的 `ringing.stream_terminated`（Lagged）在
    //! timeline 流上同样归一为 `Transport`，绝不能落成
    //! `Protocol("invalid Ringing V1 timeline SSE frame")`。

    use super::*;
    use std::sync::Arc;

    fn stream() -> TimelineStream {
        TimelineStream::new(
            "http://127.0.0.1:1".into(),
            "t".into(),
            "seed-1".into(),
            reqwest::Client::new(),
            Arc::new(RingingSession::new(
                "http://127.0.0.1:1".into(),
                "t".into(),
                reqwest::Client::new(),
            )),
            Arc::new(|_, _| {}),
            Arc::new(|_| {}),
            Arc::new(|_| {}),
            0,
            None,
        )
    }

    fn frame(event_type: &str, data: &str) -> SseFrame {
        SseFrame {
            id: String::new(),
            event_type: event_type.into(),
            data: data.into(),
        }
    }

    /// 终止帧 → Transport（可重连）。
    #[test]
    fn lagged_termination_frame_normalizes_to_transport() {
        let mut s = stream();
        let err = s
            .dispatch(
                frame(
                    "ringing.stream_terminated",
                    r#"{"code":"lagged","seed":"seed-1","skipped":9}"#,
                ),
                "epoch-1",
            )
            .expect_err("termination frame must end the timeline stream");
        assert!(
            matches!(err, ClientError::Transport(_)),
            "must normalize to Transport: {err:?}"
        );
        assert!(
            err.to_string().contains("lagged"),
            "message must carry the server code: {err}"
        );
    }

    /// 终止帧分支先于 timeline 帧解析：畸形载荷也不能落成 Protocol。
    #[test]
    fn termination_frame_is_not_misparsed_as_protocol() {
        let mut s = stream();
        let err = s
            .dispatch(frame("ringing.stream_terminated", "not-json"), "epoch-1")
            .expect_err("must error");
        assert!(matches!(err, ClientError::Transport(_)), "{err:?}");
        assert!(!err.to_string().contains("timeline SSE frame"), "{err}");
    }

    /// 对照：非终止帧的畸形载荷仍按 Protocol 处理（不误伤原有语义）。
    #[test]
    fn malformed_timeline_frame_stays_protocol() {
        let mut s = stream();
        let err = s
            .dispatch(frame("timeline.entry", "not-json"), "epoch-1")
            .expect_err("must error");
        assert!(
            matches!(err, ClientError::Protocol(_)),
            "malformed timeline frame must stay Protocol: {err:?}"
        );
    }
}
