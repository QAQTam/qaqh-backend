//! End-to-end lease re-negotiation test.
//!
//! Spawns a *private* daemon instance in an isolated data root (temporary
//! home + `QAQH_DATA_DIR`), with a shortened lease TTL
//! (`QAQH_TEST_LEASE_TTL_MS` = 3s, shorter than the client's 5s renewal
//! interval) so renewals are guaranteed to fail and the lease to expire.
//! Regression coverage for the reconnect-death loop: the client must re-open
//! (new lease) and the SSE streams must recover to `Open` instead of pinning
//! a stale session that the daemon's keepalive gate keeps closing.
//!
//! Isolation: discovery file, single-instance lock and data root all live in
//! the temporary directory — the test never touches (and never kills) any
//! daemon the user may be running.
//!
//! Run with:  cargo test -p qaqh-client --test lease_renegotiation -- --ignored
//! Requires a compiled daemon binary: cargo build -p qaqh-daemon

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use qaqh_client::{
    Channel, ChannelStatus, Client, ClientHandlers, ClientOptions, CommandOptions, ControlCommand,
    QueryRequest, RingingCommand, RingingCommandAckStatus,
};

// ── 进程级环境变量的互斥 ─────────────────────────────────────────────────

/// 本文件的两个测试都会设置**进程级**的 `QAQH_DATA_DIR`（各自隔离的 data root），
/// 而 cargo 默认**并行**跑同一文件里的测试。不加互斥，两边的客户端会互相把
/// 对方的 `daemon.json` 读进来 —— 指向另一个 daemon 的端口/token，表现为
/// 随机失败。加锁后并行度设置不再影响正确性。
///
/// （原文件只有一个测试时不存在这个问题；新增第二个测试时必须补上。）
///
/// 用 **async** 互斥锁而非 `std::sync::Mutex`：环境变量必须在整个测试期间保持
/// 稳定（客户端全程要按它解析 discovery），所以锁会被持有**跨 await**——`std`
/// 锁那样做是 clippy 的 `await_holding_lock` 违规，在 multi_thread 运行时上
/// 也是真实的死锁风险。
static ENV_GUARD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn lock_env() -> tokio::sync::MutexGuard<'static, ()> {
    ENV_GUARD.lock().await
}

// ── isolated test home ───────────────────────────────────────────────────

/// Create a private `home\.qaqh` data root (Windows-validated layout:
/// parent == home, dir name == ".qaqh") for this test's daemon + client.
fn make_isolated_home() -> PathBuf {
    let base = std::env::temp_dir().join(format!(
        "qaqh-lease-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    ));
    let data = base.join(".qaqh");
    std::fs::create_dir_all(&data).expect("create isolated data root");
    base
}

fn cleanup_isolated_home(base: &PathBuf) {
    // Best effort: daemon may still hold handles right after kill.
    let _ = std::fs::remove_dir_all(base);
}

/// 平台对应的 daemon 可执行文件名。
///
/// 原先两处查找都硬编码 `qaqh-daemon.exe`，于是本测试在 Linux/macOS 上**永远**
/// 找不到二进制、必然 panic —— 「需要编译好的 daemon」这条说明掩盖了「在非
/// Windows 上根本跑不起来」。修好后本测试才真的可用。
fn daemon_exe_name() -> &'static str {
    if cfg!(windows) {
        "qaqh-daemon.exe"
    } else {
        "qaqh-daemon"
    }
}

fn find_daemon_binary() -> PathBuf {
    let test_exe = std::env::current_exe().expect("current_exe in test");
    let test_dir = test_exe.parent().expect("exe parent dir");
    let exe = daemon_exe_name();

    // target/debug/deps/ → target/debug/  (or release)
    let rel = test_dir.join("../").join(exe);
    if rel.exists() {
        return rel.canonicalize().unwrap_or(rel);
    }

    if let Ok(dir) = std::env::var("CARGO_BUILD_TARGET_DIR") {
        for profile in &["debug", "release"] {
            let c = std::path::PathBuf::from(&dir).join(profile).join(exe);
            if c.exists() {
                return c;
            }
        }
    }

    panic!(
        "daemon binary not found at {:?}; build with: cargo build -p qaqh-daemon",
        rel
    );
}

/// Spawn a private daemon with `QAQH_TEST_LEASE_TTL_MS=3000` (lease expires
/// in 3s, faster than the client's renewal cadence — forces the
/// re-negotiation path) inside the isolated data root. Never touches the
/// user's own daemon (separate discovery/lock/data dir).
fn spawn_isolated_daemon(home: &PathBuf) -> Child {
    let daemon = find_daemon_binary();
    let data = home.join(".qaqh");

    let child = Command::new(&daemon)
        .arg("run")
        .env("QAQH_DATA_DIR", &data)
        .env("USERPROFILE", home)
        .env("HOME", home)
        .env("QAQH_TEST_LEASE_TTL_MS", "3000")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn daemon");

    let discovery_path = data.join("daemon.json");
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if let Ok(raw) = std::fs::read_to_string(&discovery_path)
            && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&raw)
            && let Some(ep) = parsed.get("endpoint").and_then(|v| v.as_str())
            && !ep.is_empty()
        {
            return child;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let mut child = child;
    let _ = child.kill();
    panic!("isolated daemon discovery file did not appear within 15s");
}

/// Drop guard: kill the spawned daemon and remove the isolated home.
struct TestDaemon {
    child: Option<Child>,
    home: PathBuf,
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        cleanup_isolated_home(&self.home);
    }
}

/// Poll until a seed-scoped timeline read succeeds (BUG-2026-09-12-10: the
/// client must replay `attach` after every lease re-negotiation) or `within`
/// elapses. A 401 here means the replay is broken.
async fn assert_timeline_readable(client: &Client, seed: &str, within: Duration) {
    let deadline = Instant::now() + within;
    loop {
        match client.fetch_timeline_page(seed, None, None).await {
            Ok(_) => return,
            Err(err) => {
                if Instant::now() >= deadline {
                    panic!(
                        "timeline fetch for {seed} kept failing after lease re-negotiation \
                         (attach replay broken?): {err:?}"
                    );
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
}

// ── test ─────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires compiled daemon binary; run with -- --ignored"]
async fn lease_expiry_triggers_renegotiation_and_streams_recover() {
    // 先拿环境锁：本测试的客户端靠 QAQH_DATA_DIR 找 discovery，不能被同文件的
    // 另一个测试改道。
    let _env = lock_env().await;
    let home = make_isolated_home();
    let data = home.join(".qaqh");

    // Point this process's client at the same isolated data root (must be set
    // before any discovery read; the daemon child gets it via env too).
    // SAFETY: 已持 ENV_GUARD，本进程内无其它并发读写。
    unsafe {
        std::env::set_var("QAQH_DATA_DIR", &data);
    }

    let daemon = TestDaemon {
        child: Some(spawn_isolated_daemon(&home)),
        home: home.clone(),
    };

    // Per-channel counters: open vs reconnecting transitions.
    let open_counts: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(vec![0, 0, 0]));
    let reconnect_counts: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(vec![0, 0, 0]));

    let handlers = ClientHandlers {
        on_liveness: std::sync::Arc::new(|| {}),
        on_batch: Arc::new(|_| {}),
        on_status: {
            let open_counts = open_counts.clone();
            let reconnect_counts = reconnect_counts.clone();
            Arc::new(move |channel: Channel, status: ChannelStatus| {
                let idx = match channel {
                    Channel::Control => 0,
                    Channel::Conversation => 1,
                    Channel::Tool => 2,
                };
                match &status {
                    ChannelStatus::Open { .. } => {
                        open_counts.lock().unwrap()[idx] += 1;
                    }
                    ChannelStatus::Reconnecting { .. } => {
                        reconnect_counts.lock().unwrap()[idx] += 1;
                    }
                    _ => {}
                }
            })
        },
        on_reset: None,
        on_timeline_entry: Arc::new(|_, _| {}),
        on_timeline_status: Arc::new(|_| {}),
        on_timeline_snapshot: Arc::new(|_| {}),
    };

    let client = Client::connect_async(ClientOptions {
        handlers,
        launch_daemon_if_missing: false,
        daemon_path: None,
        start_timeout: Duration::from_secs(8),
        remote: None,
    })
    .await
    .expect("client connect to isolated daemon");

    // BUG-2026-09-12-10 回归：创建会话并 attach——租约重新协商后客户端必须
    // 重放 attach，否则新 client_session_id 没有 seed 归属，timeline 等读路径
    // 会持续 401 lease_required（「切会话被拒」的主因之一）。
    let create_ack = client
        .send_command(
            None,
            RingingCommand::Control(ControlCommand::SessionCreate {
                close_current: false,
                cwd: None,
                tool_mode: None,
                custom_tools: Vec::new(),
            }),
            CommandOptions::default(),
        )
        .await
        .expect("session_create");
    assert_eq!(
        create_ack.status,
        RingingCommandAckStatus::Accepted,
        "session_create must be accepted"
    );
    let seed = {
        let mut found: Option<String> = None;
        for _ in 0..20 {
            tokio::time::sleep(Duration::from_millis(300)).await;
            let sessions = client
                .query(QueryRequest::SessionList)
                .await
                .expect("session.list");
            let arr = sessions
                .as_array()
                .or_else(|| sessions.get("sessions").and_then(|s| s.as_array()));
            if let Some(seed) = arr
                .and_then(|a| a.first())
                .and_then(|s| s.get("seed"))
                .and_then(|s| s.as_str())
            {
                found = Some(seed.to_string());
                break;
            }
        }
        found.expect("created session seed appears in session.list")
    };
    client.attach(&seed).await.expect("attach created session");

    // Phase 1: wait for the initial Open on all three channels.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let all_open = open_counts.lock().unwrap().iter().all(|&c| c >= 1);
        if all_open || Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let initial = open_counts.lock().unwrap().clone();
    assert!(
        initial.iter().all(|&c| c >= 1),
        "channels did not reach Open initially: {initial:?}"
    );

    // Phase 2: with TTL (3s) < renewal interval (5s), the lease keeps
    // expiring. Renewal fails -> the client must re-open and re-broadcast the
    // session; streams proactively reconnect on that re-negotiation and must
    // come back to Open instead of pinning the dead session (the
    // reconnect-death loop this test guards against).
    tokio::time::sleep(Duration::from_secs(45)).await;

    let open = open_counts.lock().unwrap().clone();
    let reconnects = reconnect_counts.lock().unwrap().clone();

    // The lease must have actually expired (reconnects observed)…
    assert!(
        reconnects.iter().any(|&c| c >= 1),
        "lease never expired (TTL override not active?): reconnects={reconnects:?}"
    );
    // …and the streams must have recovered at least once after the initial
    // Open (re-negotiation worked; no permanent disconnect).
    assert!(
        open.iter().any(|&c| c >= 2),
        "streams never recovered after lease expiry: open={open:?} reconnects={reconnects:?}"
    );

    // BUG-2026-09-12-10：重新协商已发生（上面断言过）——此刻读取 seed 的
    // timeline 必须成功（客户端已重放 attach）。旧行为：cs2 无归属 → 持续
    // 401，本轮询会超时失败。
    assert_timeline_readable(&client, &seed, Duration::from_secs(20)).await;

    // Phase 3: sanity — after another window the streams are still cycling
    // Open (self-healing continues; not permanently stuck in Reconnecting).
    tokio::time::sleep(Duration::from_secs(20)).await;
    let open = open_counts.lock().unwrap().clone();
    assert!(
        open.iter().any(|&c| c >= 3),
        "self-healing stopped after initial recovery: open={open:?}"
    );

    // 归属重放必须持续有效（窗口内至少一次重新协商已经/正在发生）。
    assert_timeline_readable(&client, &seed, Duration::from_secs(15)).await;

    client.close();
    drop(daemon);
}

/// `session.list` 当前的 seed 集合。
async fn session_seeds(client: &Client) -> Vec<String> {
    let sessions = client
        .query(QueryRequest::SessionList)
        .await
        .expect("session.list");
    sessions
        .as_array()
        .or_else(|| sessions.get("sessions").and_then(|s| s.as_array()))
        .map(|arr| {
            arr.iter()
                .filter_map(|s| s.get("seed").and_then(|v| v.as_str()).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// 创建一个会话并返回其 seed——靠「列表里新出现的那一个」识别，不能取
/// `last()`（第二次调用时会拿到上一个会话）。
async fn create_attached_session(client: &Client) -> String {
    let before = session_seeds(client).await;
    let ack = client
        .send_command(
            None,
            RingingCommand::Control(ControlCommand::SessionCreate {
                close_current: false,
                cwd: None,
                tool_mode: None,
                custom_tools: Vec::new(),
            }),
            CommandOptions::default(),
        )
        .await
        .expect("session_create");
    assert_eq!(ack.status, RingingCommandAckStatus::Accepted);

    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let now = session_seeds(client).await;
        if let Some(seed) = now.iter().find(|s| !before.contains(s)) {
            let seed = seed.clone();
            client.attach(&seed).await.expect("attach");
            return seed;
        }
        assert!(
            Instant::now() < deadline,
            "created session never appeared in session.list"
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

/// **多 seed timeline 并行**：激活第二个会话的 timeline 不得停掉第一个。
///
/// 旧实现是单槽（`Mutex<Option<TimelineHandle>>`），`activate_timeline` 会
/// 无条件停掉上一条流。对一次只显示一个 transcript 的壳（WinUI）这是可接受
/// 的取舍，但对同时跟踪多个会话的壳（TUI 的多标签 + 子代理）是数据丢失：
/// 父会话的流被停掉后，后续事件不再到达，该会话的子代理**永远发现不了**。
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires compiled daemon binary; run with -- --ignored"]
async fn activating_one_timeline_does_not_stop_another() {
    // 见 ENV_GUARD：同文件的另一个测试也用这个进程级变量。
    let _env = lock_env().await;
    let home = make_isolated_home();
    let data = home.join(".qaqh");
    // SAFETY: 已持 ENV_GUARD，本进程内无其它并发读写。
    unsafe {
        std::env::set_var("QAQH_DATA_DIR", &data);
    }

    let daemon = TestDaemon {
        child: Some(spawn_isolated_daemon(&home)),
        home: home.clone(),
    };

    let statuses: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let handlers = ClientHandlers {
        on_liveness: std::sync::Arc::new(|| {}),
        on_batch: Arc::new(|_| {}),
        on_status: Arc::new(|_, _| {}),
        on_reset: None,
        on_timeline_entry: Arc::new(|_, _| {}),
        on_timeline_status: {
            let statuses = statuses.clone();
            Arc::new(move |status| {
                let (seed, kind) = match &status {
                    qaqh_client::TimelineStatus::Connecting { seed } => (seed, "connecting"),
                    qaqh_client::TimelineStatus::Open { seed, .. } => (seed, "open"),
                    qaqh_client::TimelineStatus::Reconnecting { seed, .. } => {
                        (seed, "reconnecting")
                    }
                    qaqh_client::TimelineStatus::Closed { seed, .. } => (seed, "closed"),
                };
                statuses
                    .lock()
                    .unwrap()
                    .push((seed.clone(), kind.to_string()));
            })
        },
        on_timeline_snapshot: Arc::new(|_| {}),
    };

    let client = Client::connect_async(ClientOptions {
        handlers,
        launch_daemon_if_missing: false,
        daemon_path: None,
        start_timeout: Duration::from_secs(8),
        remote: None,
    })
    .await
    .expect("client connect to isolated daemon");

    let seed_a = create_attached_session(&client).await;
    let seed_b = create_attached_session(&client).await;
    assert_ne!(seed_a, seed_b, "must have two distinct sessions");

    client
        .activate_timeline(&seed_a)
        .await
        .expect("activate timeline A");
    client
        .activate_timeline(&seed_b)
        .await
        .expect("activate timeline B");

    // 两条流必须同时存在——这正是旧单槽实现失败的地方。
    let mut active = client.active_timelines().await;
    active.sort();
    let mut expected = vec![seed_a.clone(), seed_b.clone()];
    expected.sort();
    assert_eq!(
        active, expected,
        "activating B must not evict A (single-slot regression)"
    );

    // A 不得被停掉：既不能出现 Closed 状态，查询到的状态也不得是 Closed。
    let closed_a = statuses
        .lock()
        .unwrap()
        .iter()
        .any(|(s, k)| s == &seed_a && k == "closed");
    assert!(
        !closed_a,
        "seed A's timeline was closed by activating seed B: {:?}",
        statuses.lock().unwrap()
    );

    // 显式停用只影响目标 seed。
    client.deactivate_timeline(&seed_a).await;
    assert_eq!(client.active_timelines().await, vec![seed_b.clone()]);
    assert!(
        client.timeline_status_for(&seed_a).await.is_none(),
        "deactivated seed must have no timeline handle"
    );
    assert!(
        client.timeline_status_for(&seed_b).await.is_some(),
        "deactivating A must leave B's timeline alone"
    );

    client.close();
    drop(daemon);
}
