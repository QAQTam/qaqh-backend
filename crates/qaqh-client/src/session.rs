//! Ringing V1 session negotiation and lease renewal.

use std::sync::Arc;

use tokio::sync::{Mutex, watch};

use crate::error::{ClientError, Result};

use crate::types::{OpenRequest, OpenResponse};

/// Negotiated session state (mirrors `RingingSessionOpen` in TS).
#[derive(Debug, Clone)]
pub struct SessionState {
    pub client_instance_id: String,
    pub client_session_id: String,
    pub server_epoch: String,
    pub lease_ttl_ms: u64,
    pub renew_interval_ms: u64,
}

/// Ringing V1 session: open + background lease renewal.
pub struct RingingSession {
    base_url: String,
    token: String,
    http: reqwest::Client,
    state: Arc<Mutex<Option<SessionState>>>,
    /// Consecutive renewal failures; `>= 2` marks the lease unhealthy.
    renew_failures: Arc<Mutex<u32>>,
    /// 广播当前 `(server_epoch, client_session_id)` 给所有 SSE 流。
    /// 重新协商（renew 连续失败后重新 open）时 `send_replace` 新值，
    /// 流重连即读到新 lease——否则流永远复用已过期的 session 死循环
    /// （daemon 的 keepalive 闸门持续关闭旧 session 的流）。
    session_ctx: watch::Sender<Option<(String, String)>>,
}

const MAX_RENEW_FAILURES: u32 = 2;

/// open 请求超时（秒）：daemon 冷启动/重启窗口内 TCP 可达但 HTTP 未 accept
/// 时，请求会排队不响应——无超时则 open 永久挂起，卡死桥的 rebuild 循环。
const OPEN_TIMEOUT_SECS: u64 = 10;

/// renew 请求超时（秒）：与 open 同一 daemon 挂起场景。无超时则
/// [`RingingSession::run_renewal`] 的 `select!` 永久停在 tick 分支——ticker
/// 停摆、失败计数不增长、重新 open 的自愈路径永远走不到，lease 过期后
/// keepalive 闸门关流形成死循环（BUG-2026-09-13-09）。
/// 取值遵循 issue 建议（略小于 lease TTL）：默认 TTL 30s > 10s，保证超时
/// 先于租约过期发生，自愈在同一个 TTL 窗口内即可完成。
const RENEW_TIMEOUT_SECS: u64 = 10;

impl RingingSession {
    pub fn new(base_url: String, token: String, http: reqwest::Client) -> Self {
        let (session_ctx, _) = watch::channel(None);
        Self {
            base_url,
            token,
            http,
            state: Arc::new(Mutex::new(None)),
            renew_failures: Arc::new(Mutex::new(0)),
            session_ctx,
        }
    }

    pub fn client_instance_id(&self) -> String {
        uuid::Uuid::new_v4().to_string()
    }

    /// `POST /ringing/v1/clients/open` — capability negotiation.
    pub async fn open(&self) -> Result<SessionState> {
        let client_instance_id = self.client_instance_id();
        let response = self
            .http
            .post(format!("{}/ringing/v1/clients/open", self.base_url))
            .bearer_auth(&self.token)
            // 请求级超时（不作用于 SSE 长连接）：daemon 冷启动/重启窗口内
            // discovery 已发布但 HTTP 尚未 accept 时，TCP 连接会成功（backlog
            // 排队）而响应迟迟不来——无超时会让 open 永久挂起，进而卡死桥的
            // rebuild 循环（rebuilding 永不复位，所有请求被拒）。
            .timeout(std::time::Duration::from_secs(OPEN_TIMEOUT_SECS))
            .json(&OpenRequest::new(client_instance_id.clone()))
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(ClientError::Http {
                status: response.status().as_u16(),
                path: "/ringing/v1/clients/open".into(),
            });
        }
        let result: OpenResponse = response.json().await?;
        if result.schema != qaqh_ringing::RINGING_SCHEMA
            || result.version != qaqh_ringing::RINGING_VERSION
            || !result.accepted
        {
            return Err(ClientError::Negotiation(
                "open not accepted by daemon".into(),
            ));
        }
        if result.client_session_id.is_empty()
            || result.server_epoch.is_empty()
            || result.lease_ttl_ms == 0
            || result.renew_interval_ms == 0
        {
            return Err(ClientError::Negotiation(
                "open returned an incomplete session".into(),
            ));
        }
        let state = SessionState {
            client_instance_id,
            client_session_id: result.client_session_id,
            server_epoch: result.server_epoch,
            lease_ttl_ms: result.lease_ttl_ms,
            renew_interval_ms: result.renew_interval_ms,
        };
        *self.state.lock().await = Some(state.clone());
        self.session_ctx.send_replace(Some((
            state.server_epoch.clone(),
            state.client_session_id.clone(),
        )));
        Ok(state)
    }

    /// Subscribe to the current `(server_epoch, client_session_id)`.
    /// The receiver observes re-negotiations (new lease after renewal failure).
    pub fn session_ctx_rx(&self) -> watch::Receiver<Option<(String, String)>> {
        self.session_ctx.subscribe()
    }

    /// Adopt a session opened elsewhere (e.g. by a control client in the same process).
    pub async fn adopt(&self, state: SessionState) {
        *self.state.lock().await = Some(state.clone());
        self.session_ctx
            .send_replace(Some((state.server_epoch, state.client_session_id)));
    }

    /// Current session state, if negotiated.
    pub async fn state(&self) -> Option<SessionState> {
        self.state.lock().await.clone()
    }

    /// Start the background renewal loop. Returns when the loop exits (stop flag).
    ///
    /// 连续失败 >= [`MAX_RENEW_FAILURES`] 时判定 lease 已死（renew 反查不到
    /// 过期 session 必然 401），立即重新 open 换新 lease 并广播新 session——
    /// 否则 daemon 的 keepalive 闸门会持续关闭所有 SSE 流，客户端重连又
    /// 复用失效 session，形成无法自愈的死循环。
    pub async fn run_renewal(&self, stop: tokio::sync::watch::Receiver<bool>) {
        let Some(state) = self.state.lock().await.clone() else {
            return;
        };
        let interval =
            std::time::Duration::from_millis(std::cmp::max(1000, state.renew_interval_ms / 2));
        self.run_renewal_every(interval, stop).await;
    }

    /// [`Self::run_renewal`] 的显式间隔版本（测试可注入短 interval 驱动同一实现）。
    async fn run_renewal_every(
        &self,
        interval: std::time::Duration,
        mut stop: tokio::sync::watch::Receiver<bool>,
    ) {
        let mut ticker = tokio::time::interval(interval);
        // First tick fires immediately; skip it so the first renewal happens after
        // one interval (mirrors TS `setInterval` semantics).
        ticker.tick().await;
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if self.renew_once().await.is_ok() {
                        *self.renew_failures.lock().await = 0;
                        continue;
                    }
                    let failures = {
                        let mut f = self.renew_failures.lock().await;
                        *f += 1;
                        *f
                    };
                    if failures < MAX_RENEW_FAILURES {
                        continue;
                    }
                    // 达到阈值：跳过注定失败的 renew，直接重新协商 lease。
                    // open 带超时（OPEN_TIMEOUT_SECS），失败时保持 failures
                    // 计数，下个 interval 重试 open。
                    match self.open().await {
                        Ok(new_state) => {
                            log::warn!(
                                "[qaqh-client] lease expired; re-negotiated session {} (epoch {})",
                                new_state.client_session_id,
                                new_state.server_epoch
                            );
                            *self.renew_failures.lock().await = 0;
                        }
                        Err(err) => {
                            log::warn!(
                                "[qaqh-client] lease re-negotiation failed: {err}; will retry"
                            );
                        }
                    }
                }
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() {
                        return;
                    }
                }
            }
        }
    }

    /// `POST /ringing/v1/leases/renew` — single renewal attempt.
    async fn renew_once(&self) -> Result<()> {
        let Some(state) = self.state.lock().await.clone() else {
            return Err(ClientError::Negotiation("no session to renew".into()));
        };
        let response = self
            .http
            .post(format!("{}/ringing/v1/leases/renew", self.base_url))
            .bearer_auth(&self.token)
            .header("X-QAQH-Client-Session-Id", &state.client_session_id)
            // 请求级超时：daemon TCP 可达但 HTTP 不 accept（冷启动/重启/挂起
            // 窗口）时 renew 会排队不响应——无超时则本 future 永久挂起，
            // run_renewal 的 select! 再也回不到 tick 分支，失败计数/重新 open
            // 的自愈逻辑全部失效（BUG-2026-09-13-09）。
            .timeout(std::time::Duration::from_secs(RENEW_TIMEOUT_SECS))
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(ClientError::Http {
                status: response.status().as_u16(),
                path: "/ringing/v1/leases/renew".into(),
            });
        }
        Ok(())
    }

    /// 测试：直接注入一个已协商 lease（等价于 `open()` 成功后的状态），
    /// 供集成测试在无 daemon 二进制时驱动续期循环。
    #[doc(hidden)]
    pub async fn adopt_for_test(
        &self,
        client_instance_id: String,
        client_session_id: String,
        server_epoch: String,
        renew_interval_ms: u64,
    ) {
        let state = SessionState {
            client_instance_id,
            client_session_id,
            server_epoch,
            lease_ttl_ms: 30_000,
            renew_interval_ms,
        };
        self.adopt(state).await;
    }

    /// 测试：以显式（短）interval 驱动与 [`Self::run_renewal`] 完全相同的循环。
    #[doc(hidden)]
    pub async fn run_renewal_for_test(&self, interval: std::time::Duration) {
        let (_tx, rx) = tokio::sync::watch::channel(false);
        self.run_renewal_every(interval, rx).await;
    }

    /// 测试：当前连续续期失败计数。
    #[doc(hidden)]
    pub async fn renew_failures_for_test(&self) -> u32 {
        *self.renew_failures.lock().await
    }
}
