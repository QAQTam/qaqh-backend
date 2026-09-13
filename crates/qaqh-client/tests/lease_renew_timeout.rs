//! BUG-2026-09-13-09 回归：renew 请求无超时 → 自愈循环永久卡死。
//!
//! `RingingSession::renew_once` 此前与 `open` 不同，没有请求级超时。daemon
//! TCP 可达但 HTTP 不 accept（冷启动/重启/挂起窗口，请求在 backlog 排队）时
//! `send()` 永不返回：`run_renewal` 的 `tokio::select!` 永久停在 tick 分支，
//! ticker 不再触发 → 失败计数不增长 → lease 过期 → keepalive 闸门关流，而
//! 重新 open 的自愈路径永远走不到。
//!
//! 本测试用「接受连接但永不回包的本地回环监听器」模拟该窗口，确定性断言两条
//! 行为（修复前两条都红）：
//!  1. 挂起的 renew 必须在客户端请求超时内让出并计入失败（`renew_returns_…`）；
//!  2. 自愈循环必须在租约 TTL 内推进到「重新 open」（`renewal_loop_…`）。
//!
//! 不需要 daemon 二进制，随 `cargo test -p qaqh-client` 一起跑（约 40s）。

use std::sync::Arc;
use std::time::Duration;

use qaqh_client::RingingSession;
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;

/// daemon 端续期间隔（见 qaqh-daemon `RENEW_INTERVAL_MS`）。
const RENEW_INTERVAL_MS: u64 = 10_000;
/// 测试驱动的短续期间隔（远小于租约 TTL，保证租约未过期）。
const TEST_TICK: Duration = Duration::from_millis(200);
/// 客户端请求级超时（qaqh-client `OPEN_TIMEOUT_SECS`）。
const CLIENT_TIMEOUT_SECS: u64 = 10;

struct HangingDaemon {
    base_url: String,
    requests: tokio::sync::mpsc::UnboundedReceiver<String>,
    seen: usize,
}

/// 回环监听器：接受连接、读走请求后不回包也不关闭（模拟 daemon 挂起）。
/// 已收到的请求文本（含请求行）通过 channel 暴露给测试。
async fn spawn_hanging_daemon() -> HangingDaemon {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback listener");
    let addr = listener.local_addr().expect("local_addr");
    let (tx, requests) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let mut acc = Vec::new();
                while let Ok(n) = socket.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    acc.extend_from_slice(&buf[..n]);
                    if acc.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let _ = tx.send(String::from_utf8_lossy(&acc).into_owned());
                std::future::pending::<()>().await; // 挂住连接：不回包
            });
        }
    });
    HangingDaemon {
        base_url: format!("http://{addr}"),
        requests,
        seen: 0,
    }
}

impl HangingDaemon {
    /// 当前累计收到的请求条数（非阻塞：先把已投递的请求收进内部计数）。
    fn count_requests(&mut self, pattern: &str) -> usize {
        while let Ok(req) = self.requests.try_recv() {
            if req.contains(pattern) {
                self.seen += 1;
            }
        }
        self.seen
    }

    /// 阻塞等待累计请求数达到 `n`。
    async fn wait_requests(&mut self, pattern: &str, n: usize, within: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + within;
        while tokio::time::Instant::now() < deadline {
            if self.count_requests(pattern) >= n {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        self.count_requests(pattern) >= n
    }
}

fn new_session(base_url: &str) -> Arc<RingingSession> {
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .build()
        .expect("build reqwest client");
    Arc::new(RingingSession::new(
        base_url.to_string(),
        "test-token".into(),
        http,
    ))
}

async fn adopt_session(session: &RingingSession) {
    // 唯一需要的最小测试接线：直接注入已协商的 lease（daemon 用挂起监听器模拟）。
    session
        .adopt_for_test(
            "ci-test-1".into(),
            "cs-test-1".into(),
            "epoch-test-1".into(),
            RENEW_INTERVAL_MS,
        )
        .await;
}

/// [红→绿] 挂起的 renew 必须在请求超时内让出，且失败被计数。
///
/// 修复前：`renew_once` 无 `.timeout(..)`，`send()` 永久挂起（本测试超时失败）。
/// 修复后：~`OPEN_TIMEOUT_SECS`（10s）后返回 transport 错误 → failures 从 0 变 1。
#[tokio::test]
async fn renew_returns_within_client_timeout_when_daemon_hangs() {
    let mut daemon = spawn_hanging_daemon().await;
    let session = new_session(&daemon.base_url);
    adopt_session(&session).await;

    let renewal = tokio::spawn({
        let session = session.clone();
        async move { session.run_renewal_for_test(TEST_TICK).await }
    });

    // 首个 interval 后 renew 发出并挂在无响应的 daemon 上。
    assert!(
        daemon
            .wait_requests("/ringing/v1/leases/renew", 1, Duration::from_secs(5))
            .await,
        "first renewal was never issued"
    );
    assert_eq!(
        session.renew_failures_for_test().await,
        0,
        "renew must still be in flight (the request is hanging)"
    );

    // 修复前：这里永远等不到（send 无超时 → 失败计数停在 0）。
    let observed = tokio::time::timeout(Duration::from_secs(CLIENT_TIMEOUT_SECS + 5), async {
        loop {
            let failures = session.renew_failures_for_test().await;
            if failures > 0 {
                return failures;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;

    renewal.abort();
    assert!(
        observed.is_ok(),
        "the hanging renewal never timed out: renewal loop is permanently stuck \
         (BUG-2026-09-13-09); failures={}",
        session.renew_failures_for_test().await
    );
}

/// [红→绿] renew 挂起让路后，自愈循环必须推进到「重新 open」。
///
/// 修复前：`tokio::select!` 永久停在 tick 分支，ticker 停摆——测试等不到
/// 第二次 `/ringing/v1/clients/open`（永远停在第一次挂起的 renew 上）。
/// 修复后：每次超时都计入失败，达到 `MAX_RENEW_FAILURES`(2) 后跳过注定失败的
/// renew 直接重新 open（open 同样带超时，失败则下个 interval 重试）。
#[tokio::test]
async fn renewal_loop_self_heals_after_renew_timeout() {
    let mut daemon = spawn_hanging_daemon().await;
    let session = new_session(&daemon.base_url);
    adopt_session(&session).await;

    let renewal = tokio::spawn({
        let session = session.clone();
        async move { session.run_renewal_for_test(TEST_TICK).await }
    });

    assert!(
        daemon
            .wait_requests("/ringing/v1/leases/renew", 1, Duration::from_secs(5))
            .await,
        "first renewal was never issued"
    );

    // 时间预算：renew 的超时由客户端计时（OPEN_TIMEOUT_SECS=10s）；
    // 修复前挂起的 renew 会被重新 open 的流量掩盖，故先断言失败计数增长
    // （renew 必须真的超时让路），再看自愈是否推进。
    let failures = tokio::time::timeout(Duration::from_secs(CLIENT_TIMEOUT_SECS + 5), async {
        loop {
            let failures = session.renew_failures_for_test().await;
            if failures > 0 {
                return failures;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(
        failures.is_ok(),
        "the hanging renewal never timed out (failures stayed 0): renewal loop is permanently \
         stuck (BUG-2026-09-13-09)"
    );

    // 时间预算：2 次 renew 超时（2 × 10s）+ 重新 open 的重试窗口。
    let healed = tokio::time::timeout(Duration::from_secs(3 * CLIENT_TIMEOUT_SECS + 10), async {
        loop {
            if daemon
                .wait_requests("/ringing/v1/clients/open", 1, Duration::from_secs(1))
                .await
            {
                return true;
            }
            if renewal.is_finished() {
                return false;
            }
        }
    })
    .await;

    renewal.abort();
    assert!(
        matches!(healed, Ok(true)),
        "renewal loop never reached the re-negotiation path after the renew hung: lease \
         expiry can no longer self-heal (BUG-2026-09-13-09); renew failures={}",
        session.renew_failures_for_test().await
    );
}
