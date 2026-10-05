//! Ringing v2 session negotiation and lease renewal.

use std::sync::Arc;

use tokio::sync::{Mutex, watch};

use crate::discovery::DiscoveryExt;
use crate::error::{ClientError, Result};

/// 当前 daemon 端点与 Bearer token。
///
/// 二者在 daemon 每次启动时都会变（端口 0 → OS 临时端口，token 随机，
/// 见 `qaqh-daemon/src/server.rs`），因此**不能**在构造时烘死。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Credentials {
    pub base_url: String,
    pub token: String,
}

/// Ringing v2 session: open + background lease renewal.
pub struct RingingSession {
    /// 可热更新：daemon 重启会换端口与随机 token，靠 [`Self::refresh_discovery`]
    /// 原地换值。旧的不可变实现只会拿着死端点/旧 token 永久重试，客户端再也
    /// 回不去（TUI 侧 BUG-2026-09-14-01 的同款根因）。
    credentials: std::sync::RwLock<Credentials>,
    /// 是否允许从本地 `daemon.json` 刷新凭据。
    ///
    /// **默认关闭**，由 [`crate::Client::connect_async`] 仅在「本地发现模式」
    /// 下显式打开。默认关闭是安全默认值：任何直接构造 `RingingSession` 并指向
    /// 明确端点（远端直连、测试里的回环 mock）的调用方，都不会被本机恰好存在
    /// 的 `daemon.json` 悄悄改道到另一个 daemon。
    local_discovery: std::sync::atomic::AtomicBool,
    http: reqwest::Client,
    state: Arc<Mutex<Option<crate::v2::ClientV2SessionState>>>,
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
            credentials: std::sync::RwLock::new(Credentials {
                base_url: base_url.trim_end_matches('/').to_string(),
                token,
            }),
            local_discovery: std::sync::atomic::AtomicBool::new(false),
            http,
            state: Arc::new(Mutex::new(None)),
            renew_failures: Arc::new(Mutex::new(0)),
            session_ctx,
        }
    }

    pub fn client_instance_id(&self) -> String {
        uuid::Uuid::new_v4().to_string()
    }

    /// 当前凭据快照。**拷贝出锁**——调用方常在 `await` 前使用，持有
    /// `RwLockReadGuard` 会让 future 变成 `!Send`。
    pub(crate) fn credentials(&self) -> Credentials {
        let guard = self.credentials.read().expect("credentials lock");
        Credentials {
            base_url: guard.base_url.clone(),
            token: guard.token.clone(),
        }
    }

    /// 重读 `daemon.json` 并**原地换值**（daemon 重启换端口/token 的唯一自愈
    /// 路径）。返回是否真的变了。
    ///
    /// 只接受 pid 存活的记录：daemon 被强杀后遗留的 `daemon.json` 会把客户端
    /// 引向死端口（与 [`crate::discovery::read_discovery`] 的调用方同判据）。
    ///
    /// 调用方在「连接/续租失败」时调用它，而不是周期性轮询——稳态下不产生
    /// 任何文件 IO。
    pub fn refresh_discovery(&self) -> bool {
        if !self
            .local_discovery
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return false;
        }
        let Some(d) = crate::discovery::read_discovery()
            .ok()
            .filter(|d| crate::discovery::process_is_running(d.pid))
        else {
            return false;
        };
        let Ok(base_url) = d.base_url() else {
            return false;
        };
        let base_url = base_url.trim_end_matches('/').to_string();
        let mut guard = self.credentials.write().expect("credentials lock");
        if guard.base_url == base_url && guard.token == d.token {
            return false;
        }
        guard.base_url = base_url;
        guard.token = d.token;
        true
    }

    /// `POST /ringing/v2/clients/open` — capability negotiation（纯 v2：唯一握手）。
    ///
    /// 一次握手填充唯一的会话状态（header 身份 + capability，SSE/命令/服务共用）。
    pub async fn open(&self) -> Result<crate::v2::ClientV2SessionState> {
        let client_instance_id = self.client_instance_id();
        let creds = self.credentials();
        let path = "/ringing/v2/clients/open";
        let response = self
            .http
            .post(format!("{}{path}", creds.base_url))
            .bearer_auth(&creds.token)
            // 请求级超时（不作用于 SSE 长连接）：daemon 冷启动/重启窗口内
            // discovery 已发布但 HTTP 尚未 accept 时，TCP 连接会成功（backlog
            // 排队）而响应迟迟不来——无超时会让 open 永久挂起，进而卡死桥的
            // rebuild 循环（rebuilding 永不复位，所有请求被拒）。
            .timeout(std::time::Duration::from_secs(OPEN_TIMEOUT_SECS))
            .json(&qaqh_ringing::RingingV2OpenRequest::new(
                client_instance_id.clone(),
            ))
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(ClientError::Http {
                status: response.status().as_u16(),
                path: path.into(),
            });
        }
        let result: qaqh_ringing::RingingV2OpenResponse = response.json().await?;
        result
            .validate()
            .map_err(|code| ClientError::Protocol(format!("invalid v2 open response: {code}")))?;
        if !result.accepted {
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
        let state = crate::v2::ClientV2SessionState::from_open(client_instance_id, result)?;
        *self.state.lock().await = Some(state.clone());
        self.session_ctx.send_replace(Some((
            state.server_epoch.clone(),
            state.client_session_id.clone(),
        )));
        Ok(state)
    }

    /// 显式 v2 握手路径（`Client::open_v2`）写入的状态。
    pub(crate) async fn adopt_v2_state(&self, state: crate::v2::ClientV2SessionState) {
        *self.state.lock().await = Some(state);
    }

    /// Subscribe to the current `(server_epoch, client_session_id)`.
    /// The receiver observes re-negotiations (new lease after renewal failure).
    pub fn session_ctx_rx(&self) -> watch::Receiver<Option<(String, String)>> {
        self.session_ctx.subscribe()
    }

    /// 是否允许 [`Self::refresh_discovery`] 从本地 `daemon.json` 取凭据。
    /// 远端直连（`ClientOptions::remote`）必须置 `false`。
    pub fn set_local_discovery(&self, local: bool) {
        self.local_discovery
            .store(local, std::sync::atomic::Ordering::Relaxed);
    }

    /// Adopt a session opened elsewhere (e.g. by a control client in the same process).
    pub async fn adopt(&self, state: crate::v2::ClientV2SessionState) {
        *self.state.lock().await = Some(state.clone());
        self.session_ctx
            .send_replace(Some((state.server_epoch, state.client_session_id)));
    }

    /// Current session state, if negotiated.
    pub async fn state(&self) -> Option<crate::v2::ClientV2SessionState> {
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
                    // 续租失败 = daemon 可能已经不在了。daemon 重启会同时换掉
                    // 端口与随机 token（`server.rs:119`），拿旧值重试是**永远
                    // 不会成功**的——先重读 daemon.json 原地换值，后续的重试/
                    // 重协商才可能命中新 daemon（BUG-2026-09-14-01 同类根因）。
                    if self.refresh_discovery() {
                        log::info!(
                            "[qaqh-client] adopted a new daemon discovery record after renewal failure"
                        );
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
                    //
                    // open 前再刷一次：daemon 重启换端口时 renew 只会得到连接
                    // 失败（而不是 401），上面的刷新判据同样适用；这里再做一次
                    // 是为了覆盖「失败计数已攒够、但换端口发生在上一次刷新之后」
                    // 的窗口。
                    self.refresh_discovery();
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

    /// `POST /ringing/v2/leases/renew` — single renewal attempt.
    async fn renew_once(&self) -> Result<()> {
        let Some(state) = self.state.lock().await.clone() else {
            return Err(ClientError::Negotiation("no session to renew".into()));
        };
        let creds = self.credentials();
        let path = "/ringing/v2/leases/renew";
        let response = self
            .http
            .post(format!("{}{path}", creds.base_url))
            .bearer_auth(&creds.token)
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
                path: path.into(),
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
        let state = crate::v2::ClientV2SessionState {
            client_instance_id,
            client_session_id,
            server_epoch,
            lease_ttl_ms: 30_000,
            renew_interval_ms,
            capabilities: qaqh_ringing::RingingV2Capabilities {
                subscribe: true,
                interact: true,
                drive: true,
                timeline: true,
                service: true,
                content: true,
                single_stream: true,
                pairing: false,
            },
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

#[cfg(test)]
mod tests {
    //! 凭据热更新（daemon 重启自愈）回归。
    //!
    //! daemon 每次启动都换端口与随机 token，构造时烘死的凭据永远打不到新
    //! daemon——`refresh_discovery` 是唯一的自愈路径，它的**安全默认值**与
    //! 「真的换值」两侧都要锁住。

    use super::*;

    fn session(base_url: &str, token: &str) -> RingingSession {
        crate::client::ensure_crypto_provider();
        RingingSession::new(base_url.into(), token.into(), reqwest::Client::new())
    }

    /// 默认**不得**采纳本机 `daemon.json`。
    ///
    /// 锁的是一个真实踩过的坑：`refresh_discovery` 最初默认开启，于是「指向
    /// 回环 mock 的会话」在续租失败后被本机恰好在跑的 daemon 悄悄改道——mock
    /// 收到的失败被真实 daemon 的成功 open 清零，回归测试随即失灵。
    #[test]
    fn refresh_discovery_is_off_by_default() {
        let s = session("http://127.0.0.1:1", "t");
        assert!(
            !s.refresh_discovery(),
            "默认不得从本机 daemon.json 取凭据（会把明确指向 mock/远端直连的会话改道）"
        );
        assert_eq!(s.credentials().base_url, "http://127.0.0.1:1");
        assert_eq!(s.credentials().token, "t");
    }

    /// 尾随斜杠归一化：否则会拼出 `http://host:port//ringing/v2/...`。
    #[test]
    fn constructor_normalizes_trailing_slash() {
        assert_eq!(
            session("http://127.0.0.1:9/", "t").credentials().base_url,
            "http://127.0.0.1:9"
        );
    }

    /// 开启后：重读 `daemon.json` 原地换值；值未变时如实报告「未变化」
    /// （调用方靠它区分「换到新 daemon」与「本来就在正确的 daemon 上」）。
    #[test]
    fn refresh_discovery_adopts_a_changed_record() {
        let dir = std::env::temp_dir().join(format!("qaqh-refresh-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create data dir");
        // pid 必填且必须「存活」——record 的 pid 判活是这条路径的准入条件。
        let record = serde_json::json!({
            "endpoint": "http://127.0.0.1:5555",
            "token": "new-token",
            "pid": std::process::id(),
            "server_epoch": "ep-new",
            "protocol_version": 1,
        });
        std::fs::write(
            dir.join("daemon.json"),
            serde_json::to_vec(&record).expect("encode"),
        )
        .expect("write daemon.json");

        // 环境变量是进程级的：本模块只有这一个测试读 discovery，且先存后还。
        let previous = std::env::var_os("QAQH_DATA_DIR");
        // SAFETY: 单测进程内没有其他线程读这个变量（唯一读它的代码路径
        // `refresh_discovery` 只在下面的断言里被调用），且测试结束即还原。
        unsafe { std::env::set_var("QAQH_DATA_DIR", &dir) };

        let s = session("http://127.0.0.1:1", "old-token");
        s.set_local_discovery(true);

        assert!(
            s.refresh_discovery(),
            "daemon.json 变了（端口与 token 都不同）必须报告变化"
        );
        assert_eq!(s.credentials().base_url, "http://127.0.0.1:5555");
        assert_eq!(s.credentials().token, "new-token");
        assert!(
            !s.refresh_discovery(),
            "第二次不应再报告变化（否则调用方会反复当成「换到新 daemon」）"
        );

        match previous {
            Some(v) => unsafe { std::env::set_var("QAQH_DATA_DIR", v) },
            None => unsafe { std::env::remove_var("QAQH_DATA_DIR") },
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
