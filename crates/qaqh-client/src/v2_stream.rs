//! Per-seed canonical v2 single-stream reader.
//!
//! 2026-09-24 硬切：v1 的三条 `/ringing/v1/events/{channel}` 全局流被
//! `/ringing/v2/sessions/{seed}/events` **每 seed 一条**取代。本模块是客户端侧的
//! 常驻读循环：连接 / 解码 / cursor 推进 / reset / 退避重连，语义对齐
//! [`crate::timeline::TimelineStream`] 与 [`crate::v2::ClientV2Subscription`]。
//!
//! 与 v1 通道流的差异：
//! - **按 seed 起流**（attach 时启动、detach 时停止），不再是三条全局流；
//! - cursor 是 canonical `CursorToken`（`since_cursor` 查询参数），不是
//!   `Last-Event-ID`；只有 reliable 事件推进 cursor；
//! - `ResetRequired` 是一条独立帧，送达后服务端主动断流，客户端重连前由上层
//!   重新 bootstrap。

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

use crate::client::Client;
use crate::error::{ClientError, Result};
use crate::types::ReconnectReason;
use crate::v2::{ClientV2Event, ClientV2Reset, ClientV2SubscriptionEvent};

const SSE_IDLE_TIMEOUT: Duration = Duration::from_secs(45);
const RETRY_BASE_MS: u64 = 1_000;
const RETRY_MAX_MS: u64 = 30_000;

/// v2 单流的状态迁移（每个 seed 一条）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum V2StreamStatus {
    Connecting,
    Open {
        server_epoch: String,
        cursor: Option<String>,
    },
    Reconnecting {
        retry_ms: u64,
        reason: Option<ReconnectReason>,
        last_cursor: Option<String>,
    },
    Closed {
        reason: String,
    },
}

/// 单条 v2 流的回调。
pub struct V2StreamHandlers {
    pub on_event: Arc<dyn Fn(String, ClientV2Event) + Send + Sync>,
    pub on_reset: Arc<dyn Fn(String, ClientV2Reset) + Send + Sync>,
    pub on_status: Arc<dyn Fn(String, V2StreamStatus) + Send + Sync>,
    pub on_liveness: Arc<dyn Fn() + Send + Sync>,
}

/// 一条 seed 的常驻 v2 单流。
pub struct V2Stream {
    seed: String,
    client: Client,
    handlers: V2StreamHandlers,
    /// 已接受的 canonical cursor（只有 reliable 事件推进）。
    cursor: Option<qaqh_ringing::CursorToken>,
    /// 上一次成功连接的 epoch。daemon 重启换 epoch 后 cursor 语义失效，必须归零。
    last_epoch: Option<String>,
}

impl V2Stream {
    pub fn new(seed: impl Into<String>, client: Client, handlers: V2StreamHandlers) -> Self {
        Self {
            seed: seed.into(),
            client,
            handlers,
            cursor: None,
            last_epoch: None,
        }
    }

    /// 运行直到 `stop` 或 `session_stop` 被置位。除停止外不把错误抛给调用方。
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
                Ok(()) => {}
                Err(err) => {
                    if *stop.borrow() || *session_stop.borrow() {
                        return;
                    }
                    let not_ready = err.is_session_not_ready();
                    if not_ready {
                        // 会话尚未物化（新建会话在首个 canonical 事实落盘前
                        // 404/409）是**正常瞬态**：报 `Connecting`（不抬流告警），
                        // 短退避重试到物化为止。
                        (self.handlers.on_status)(self.seed.clone(), V2StreamStatus::Connecting);
                        log::debug!(
                            "[qaqh-client] v2 stream {} waiting for session to materialize: {err}",
                            self.seed
                        );
                    } else {
                        let reason = err.reconnect_reason();
                        (self.handlers.on_status)(
                            self.seed.clone(),
                            V2StreamStatus::Reconnecting {
                                retry_ms,
                                reason,
                                last_cursor: self.cursor.as_ref().map(|c| c.as_str().to_string()),
                            },
                        );
                        log::warn!(
                            "[qaqh-client] v2 stream {} reconnect in {retry_ms}ms: {err}",
                            self.seed
                        );
                    }
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_millis(retry_ms)) => {}
                        _ = stop.changed() => return,
                        _ = session_stop.changed() => return,
                    }
                    // 会话尚未物化保持短退避，别把指数退避拖到 30s。
                    retry_ms = if not_ready {
                        RETRY_BASE_MS
                    } else {
                        std::cmp::min(retry_ms * 2, RETRY_MAX_MS)
                    };
                }
            }
        }
    }

    async fn connect_once(
        &mut self,
        stop: &mut watch::Receiver<bool>,
        session_stop: &mut watch::Receiver<bool>,
        retry_ms: &mut u64,
    ) -> Result<()> {
        (self.handlers.on_status)(self.seed.clone(), V2StreamStatus::Connecting);
        let server_epoch = self
            .client
            .v2_session_state()
            .await
            .map(|state| state.server_epoch)
            .ok_or_else(|| ClientError::Negotiation("session not open".into()))?;
        // epoch 变化 → 旧 cursor 语义失效，归零后从 snapshot 重新续传。
        if self.last_epoch.as_deref() != Some(server_epoch.as_str()) {
            self.cursor = None;
            self.last_epoch = Some(server_epoch.clone());
        }
        // 首次（或 epoch 变化后）没有 cursor：先取 bootstrap 的 canonical
        // snapshot cursor，再从它订阅。**不能**裸订阅——服务端把「无 cursor」
        // 解释为「只看 live」，会静默丢掉 snapshot 与订阅之间的所有事实。
        if self.cursor.is_none() {
            let bootstrap = self.client.bootstrap_v2(&self.seed).await?;
            self.cursor = Some(bootstrap.snapshot_cursor.clone());
        }

        let mut subscription = self
            .client
            .subscribe_v2(&self.seed, self.cursor.as_ref())
            .await?;
        *retry_ms = RETRY_BASE_MS;
        (self.handlers.on_status)(
            self.seed.clone(),
            V2StreamStatus::Open {
                server_epoch,
                cursor: self.cursor.as_ref().map(|c| c.as_str().to_string()),
            },
        );

        let idle = tokio::time::sleep(SSE_IDLE_TIMEOUT);
        tokio::pin!(idle);
        // 租约重协商换了 client_session_id：旧连接会被服务端静默过滤，必须重连。
        let mut ctx = self.client.inner.session.session_ctx_rx();

        loop {
            tokio::select! {
                changed = ctx.changed() => {
                    if changed.is_err() {
                        return Ok(());
                    }
                    return Err(ClientError::Negotiation(
                        "lease re-negotiated; reconnecting v2 stream".into(),
                    ));
                }
                _ = stop.changed() => return Ok(()),
                _ = session_stop.changed() => return Ok(()),
                _ = &mut idle => {
                    return Err(ClientError::Transport("v2 SSE idle timeout".into()));
                }
                item = subscription.next() => {
                    idle.as_mut().reset(tokio::time::Instant::now() + SSE_IDLE_TIMEOUT);
                    match item? {
                        Some(ClientV2SubscriptionEvent::Event(event)) => {
                            // 只有 reliable 事件带 canonical cursor，也只有它推进 cursor。
                            if event.delivery == qaqh_ringing::RingingV2Delivery::Reliable
                                && let Some(cursor) = event.cursor.clone()
                            {
                                self.cursor = Some(cursor);
                            }
                            (self.handlers.on_event)(self.seed.clone(), *event);
                            (self.handlers.on_liveness)();
                        }
                        Some(ClientV2SubscriptionEvent::Reset(reset)) => {
                            // 服务端发完 reset 即断流；把 cursor 对齐到 reset 携带的
                            // snapshot（若没有就归零，由上层重新 bootstrap）。
                            self.cursor = reset.snapshot_cursor.clone();
                            (self.handlers.on_reset)(self.seed.clone(), reset);
                            return Ok(());
                        }
                        None => {
                            return Err(ClientError::Transport("v2 SSE stream ended".into()));
                        }
                    }
                }
            }
        }
    }
}
