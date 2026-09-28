//! Per-session Ringing v2 timeline SSE stream.
//!
//! Mirrors the Ringing v2 timeline semantics (the original Electron reference
//! implementation `apps/desktop/electron/timelineClient.ts` was removed): one
//! SSE stream per session, one monotonically increasing cursor
//! (`{epoch}:timeline:{seq}`). Streams are keyed by seed, so a shell may hold
//! several at once (tabs, subagent sessions).
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

/// 观测到 daemon 当前 epoch：更新记录，并回答「是否需要回快照重新定基」。
///
/// - 从未连过（`None`）：cursor 本来就取自刚拉到的快照 watermark，已对齐，
///   不需要再定基；
/// - epoch 未变：cursor 属于同一个 seq 序列，续传即可；
/// - epoch 变了（daemon 重启 / 租约重协商）：旧 cursor 属于**另一个** seq
///   序列，必须回权威快照重定基——否则会带着旧位置连进 `Protocol` error
///   重连死循环。
///
/// 抽成纯函数是为了可回归测试：真正的定基路径需要一个活着的 daemon。
fn observe_epoch(last_epoch: &mut Option<String>, server_epoch: &str) -> bool {
    if last_epoch.as_deref() == Some(server_epoch) {
        return false;
    }
    let had_previous = last_epoch.is_some();
    *last_epoch = Some(server_epoch.to_string());
    had_previous
}

/// One per-session timeline stream with independent cursor, reconnect backoff
/// and gap recovery. Created by `Client::activate_timeline` and run as a
/// background task; callbacks fire on the tokio side.
pub struct TimelineStream {
    session_id: String,
    http: reqwest::Client,
    /// Read on every connect: endpoint + Bearer token + (server_epoch,
    /// client_session_id). A daemon restart swaps the endpoint and token, so
    /// neither may be baked in at construction.
    session: Arc<RingingSession>,
    on_entry: Arc<dyn Fn(String, TimelineEntry) + Send + Sync>,
    on_status: Arc<dyn Fn(TimelineStatus) + Send + Sync>,
    /// Forwarded on gap recovery: the fresh snapshot becomes the new baseline.
    on_snapshot: Arc<dyn Fn(TimelinePage) + Send + Sync>,
    /// 见 `crate::ClientHandlers::on_liveness`。
    on_liveness: Arc<dyn Fn() + Send + Sync>,
    /// Optional sink for `Client::timeline_status_for()` (also fed on exit).
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
        session_id: String,
        http: reqwest::Client,
        session: Arc<RingingSession>,
        on_entry: Arc<dyn Fn(String, TimelineEntry) + Send + Sync>,
        on_status: Arc<dyn Fn(TimelineStatus) + Send + Sync>,
        on_snapshot: Arc<dyn Fn(TimelinePage) + Send + Sync>,
        on_liveness: Arc<dyn Fn() + Send + Sync>,
        initial_cursor: u64,
        status_tx: Option<watch::Sender<Option<TimelineStatus>>>,
    ) -> Self {
        Self {
            session_id,
            http,
            session,
            on_entry,
            on_status,
            on_snapshot,
            on_liveness,
            status_tx,
            cursor: initial_cursor,
            last_epoch: None,
        }
    }

    fn set_status(&self, status: TimelineStatus) {
        // `send_replace` updates the value even without receivers: the
        // `timeline_status_for()` query reads the sender-side value, and the
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
                                self.session_id,
                                self.cursor
                            ),
                            Err(recovery_err) => log::warn!(
                                "[qaqh-client] timeline {} gap snapshot recovery failed: {recovery_err}",
                                self.session_id
                            ),
                        }
                    }
                    self.set_status(TimelineStatus::Reconnecting {
                        session_id: self.session_id.clone(),
                        retry_ms,
                        cursor: self.cursor,
                        reason: err.reconnect_reason(),
                    });
                    log::warn!(
                        "[qaqh-client] timeline {} reconnect in {retry_ms}ms: {err}",
                        self.session_id
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
            session_id: self.session_id.clone(),
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
            session_id: self.session_id.clone(),
        });
        let state = self
            .session
            .state()
            .await
            .ok_or_else(|| ClientError::Negotiation("session not open".into()))?;

        // Lease re-negotiation swapped the epoch: re-baseline against the
        // authoritative snapshot so the reconnect cursor stays covered, then
        // forward the snapshot so listeners rebuild the transcript.
        {
            let needs_rebaseline = observe_epoch(&mut self.last_epoch, &state.server_epoch);
            if needs_rebaseline {
                match self.recover_gap().await {
                    Ok(()) => log::info!(
                        "[qaqh-client] timeline {} re-baselined after session re-negotiation (cursor {})",
                        self.session_id,
                        self.cursor
                    ),
                    Err(recovery_err) => {
                        // 兜底：从 0 全量回放（daemon 按 0 处理旧 epoch 的
                        // Last-Event-ID），避免带着旧 cursor 连进 Protocol
                        // error 死循环。
                        log::warn!(
                            "[qaqh-client] timeline {} re-baseline failed ({recovery_err}); replaying from head",
                            self.session_id
                        );
                        self.cursor = 0;
                    }
                }
            }
        }

        let path = format!("/ringing/v2/sessions/{}/timeline/events", self.session_id);
        let creds = self.session.credentials();
        let mut request = self
            .http
            .get(format!("{}{path}", creds.base_url))
            .bearer_auth(&creds.token)
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
            session_id: self.session_id.clone(),
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
                            // 收到字节 = daemon 还活着。**必须在解码之前报**：
                            // keepalive 是注释行，解码器会直接丢掉它（见
                            // `sse_decoder.rs`），而它恰恰是空闲期唯一的存活证据。
                            (self.on_liveness)();
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
            return Err(ClientError::stream_terminated(frame.data.trim()));
        }
        let parsed: TimelineSseFrame = serde_json::from_str(frame.data.trim())
            .map_err(|e| ClientError::Protocol(format!("bad timeline frame: {e}")))?;
        if parsed.schema != qaqh_ringing::RINGING_SCHEMA
            || parsed.version != qaqh_ringing::RINGING_V2_VERSION
            || parsed.session_id != self.session_id
            || parsed.server_epoch != server_epoch
        {
            return Err(ClientError::Protocol(
                "invalid timeline SSE frame".into(),
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
        (self.on_entry)(self.session_id.clone(), parsed.entry);
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
        let path = format!("/ringing/v2/sessions/{}/timeline", self.session_id);
        let creds = self.session.credentials();
        let response = self
            .http
            .get(format!("{}{path}", creds.base_url))
            .bearer_auth(&creds.token)
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
        page.validate_for(&self.session_id)
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
    //! timeline 流上同样归一为结构化 `StreamTerminated`，绝不能落成
    //! `Protocol("invalid timeline SSE frame")`。

    use super::*;
    use crate::types::ReconnectReason;
    use std::sync::Arc;

    fn stream() -> TimelineStream {
        TimelineStream::new(
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
            Arc::new(|| {}),
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

    /// 终止帧 → 结构化可重连原因。
    #[test]
    fn lagged_termination_frame_carries_structured_reason() {
        let mut s = stream();
        let err = s
            .dispatch(
                frame(
                    "ringing.stream_terminated",
                    r#"{"code":"lagged","session_id":"session_id-1","skipped":9}"#,
                ),
                "epoch-1",
            )
            .expect_err("termination frame must end the timeline stream");
        assert!(
            matches!(
                err,
                ClientError::StreamTerminated {
                    ref code,
                    skipped: Some(9)
                } if code == "lagged"
            ),
            "must retain structured termination: {err:?}"
        );
        assert_eq!(
            err.reconnect_reason(),
            Some(ReconnectReason::Lagged { skipped: 9 })
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
        assert!(
            matches!(err, ClientError::StreamTerminated { ref code, .. } if code == "unknown"),
            "{err:?}"
        );
        assert!(!err.to_string().contains("timeline SSE frame"), "{err}");
    }

    /// **epoch 变化必须触发重新定基**（另一个 seq 序列，旧 cursor 不可用）。
    #[test]
    fn epoch_change_requires_rebaseline() {
        let mut last = Some("ep-1".to_string());
        assert!(
            observe_epoch(&mut last, "ep-2"),
            "epoch 变化必须回快照重定基，否则带旧 cursor 连进 Protocol 死循环"
        );
        assert_eq!(last.as_deref(), Some("ep-2"), "记录必须跟进到新 epoch");
    }

    /// 同 epoch 内重连：cursor 仍属于同一序列，**不得**重定基（否则每次抖动
    /// 都要重拉整页快照并重摆整条 transcript）。
    #[test]
    fn same_epoch_does_not_rebaseline() {
        let mut last = Some("ep-1".to_string());
        assert!(!observe_epoch(&mut last, "ep-1"));
        assert_eq!(last.as_deref(), Some("ep-1"));
    }

    /// 首次连接不算「变化」：cursor 取自刚拉到的快照 watermark，本就对齐。
    #[test]
    fn first_connect_does_not_rebaseline() {
        let mut last: Option<String> = None;
        assert!(!observe_epoch(&mut last, "ep-1"), "首连不得多拉一次快照");
        assert_eq!(last.as_deref(), Some("ep-1"));
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
