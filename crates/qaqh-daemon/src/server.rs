use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use qaqh_runtime::QaqhService;
use qaqh_runtime::RingingHub;
use qaqh_session::canonical::{
    CANONICAL_IDENTITY_FILE, CanonicalLog, CanonicalSessionIdentity, EVENTS_FILE, WriterId,
};
use qaqh_types::{CONTROL_PROTOCOL_VERSION, DaemonDiscovery};
use tokio::net::TcpListener;
use tokio::sync::watch;

/// hub-fact-bus spec 阶段 2.3：canonical fact 投影链上的命令回执折叠 sink。
/// 先折叠回执，再委托 `V2ProjectionHub`——durable-before-publish 语义不变，
/// 折叠只看见已落盘事实。
struct FoldingSink {
    v2: Arc<qaqh_runtime::ringing::V2ProjectionHub>,
    pending: Arc<Mutex<qaqh_runtime::ringing::PendingCommandStore>>,
}

impl qaqh_session::projection::ProjectionSink for FoldingSink {
    fn publish(
        &self,
        session_dir: &std::path::Path,
        fact: &qaqh_session::session_fact_v2::SessionFact,
        events: &[qaqh_session::session_fact_v2::ProjectionEvent],
    ) {
        self.pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .observe_projection_events(events);
        qaqh_session::projection::ProjectionSink::publish(&*self.v2, session_dir, fact, events);
    }
}

fn daemon_channel() -> String {
    std::env::var("QAQH_CHANNEL").unwrap_or_else(|_| {
        if cfg!(debug_assertions) {
            "dev".into()
        } else {
            "stable".into()
        }
    })
}

/// `qaqh-daemon server` 的网络配置。
///
/// 审计 H3（2026-10-01）：这是**明文 HTTP + Bearer token** 的控制面，token
/// 一旦被截获即可完全接管 agent（读任意会话、驱动工具执行、直接设 L4）。
/// 因此：默认只绑 loopback；非 loopback 绑定必须显式给出 token——这是唯一
/// 的「我知道我在做什么」开关，启动横幅还会再警告一次传输无加密。
#[derive(Debug, Clone)]
pub struct ServerNetworkConfig {
    /// 监听 IP；`0.0.0.0` = 局域网可访问。
    pub bind_ip: std::net::IpAddr,
    /// 固定端口（远端客户端需要可预测的地址）。
    pub port: u16,
    /// Bearer token；缺省时随机生成并打印到 stderr。
    pub token: Option<String>,
}

impl Default for ServerNetworkConfig {
    fn default() -> Self {
        Self {
            bind_ip: std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            port: 0,
            token: None,
        }
    }
}

impl ServerNetworkConfig {
    /// `server --bind <ip> --port <port> --token <token>`；token 也接受
    /// `QAQH_SERVER_TOKEN` 环境变量（避免出现在进程命令行里）。
    pub fn parse(args: &[String]) -> Result<Self, String> {
        let mut config = Self {
            // 审计 H3：默认 loopback。跨端模式必须显式 `--bind`，不给 LAN
            // 侧留下被动嗅探 Bearer token 的默认面。
            bind_ip: std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            port: 64413,
            token: std::env::var("QAQH_SERVER_TOKEN")
                .ok()
                .filter(|v| !v.is_empty()),
        };
        let mut index = 0;
        while index < args.len() {
            match args[index].as_str() {
                "--bind" => {
                    index += 1;
                    let value = args.get(index).ok_or("--bind requires a value")?;
                    config.bind_ip = value
                        .parse()
                        .map_err(|_| format!("invalid --bind ip: {value}"))?;
                }
                "--port" => {
                    index += 1;
                    let value = args.get(index).ok_or("--port requires a value")?;
                    config.port = value
                        .parse()
                        .map_err(|_| format!("invalid --port: {value}"))?;
                }
                "--token" => {
                    index += 1;
                    let value = args.get(index).ok_or("--token requires a value")?;
                    config.token = Some(value.clone());
                }
                other => return Err(format!("unknown server flag: {other}")),
            }
            index += 1;
        }
        // 审计 H3：非 loopback 绑定必须显式 token。没有它，随机 token 会打
        // 到 stderr 并以明文 HTTP 暴露完整 agent 控制面——把「确认危险」的
        // 动作交还给显式传参这一步。
        if !config.bind_ip.is_loopback() && config.token.is_none() {
            return Err(
                "refusing to bind a non-loopback address without an explicit token: \
                 pass --token <token> (or set QAQH_SERVER_TOKEN). The control plane is \
                 plain-text HTTP; anything on the network can read the bearer token. \
                 Bind loopback (default) if the remote peer does not truly need LAN access."
                    .into(),
            );
        }
        Ok(config)
    }
}

/// 尽力猜测本机局域网 IP：UDP connect 不真正发包，只让内核选出口网卡。
/// 连的是 TEST-NET 保留地址，不会产生网络流量。
fn guess_lan_ip() -> Option<std::net::IpAddr> {
    let socket = std::net::UdpSocket::bind((std::net::Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket
        .connect((std::net::Ipv4Addr::new(192, 0, 2, 1), 9))
        .ok()?;
    socket.local_addr().ok().map(|addr| addr.ip())
}

pub async fn run() -> Result<(), String> {
    run_with(ServerNetworkConfig::default()).await
}

/// `run` 模式的端口解析：显式 `--port` > `QAQH_SERVER_PORT` 环境变量 > 随机。
///
/// 固定端口是 webUI 开发体验的前置：浏览器书签 / PWA / 反向代理都要求
/// 可预测的地址；随机端口下每次重启 daemon 都要重读 daemon.json。
fn resolve_run_port(configured: u16) -> u16 {
    if configured != 0 {
        return configured;
    }
    std::env::var("QAQH_SERVER_PORT")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0)
}

pub async fn run_with(config: ServerNetworkConfig) -> Result<(), String> {
    let data_root = qaqh_types::platform::ensure_data_root().map_err(stringify)?;
    // PR-3-1：SessionManager::init 收敛到 daemon main 装配点，全进程经注入
    // 句柄访问会话存储（hub / service / registry 均在此注入）。
    qaqh_session::SessionManager::init(qaqh_types::platform::data_dir());
    let sessions = qaqh_session::SessionManager::global();
    let _lock = acquire_single_instance()?;
    // Single-instance lock is held: any canonical writer fence still on disk
    // belongs to a previous daemon process or an ordered-exit gap, never to a
    // live worker of this process. Rotate it before the registry can spawn.
    rotate_stale_writer_fences();
    let token = config.token.clone().unwrap_or_else(random_hex);
    if config.token.is_none() && !config.bind_ip.is_loopback() {
        // 临时跨端模式：没显式给 key 时把生成值打出来，方便手动填写。
        eprintln!("[qaqh-daemon] generated server token: {token}");
    }
    let epoch = random_hex();
    let listener = TcpListener::bind((config.bind_ip, resolve_run_port(config.port)))
        .await
        .map_err(stringify)?;
    let address = listener.local_addr().map_err(stringify)?;
    // 0.0.0.0 不可被远端直连：discovery 与 display_host 换成可路由的局域网 IP。
    let advertise_ip = if config.bind_ip.is_unspecified() {
        guess_lan_ip().unwrap_or(config.bind_ip)
    } else {
        config.bind_ip
    };
    let discovery = DaemonDiscovery {
        endpoint: format!("http://{advertise_ip}:{}", address.port()),
        token: token.clone(),
        pid: std::process::id(),
        server_epoch: epoch.clone(),
        protocol_version: CONTROL_PROTOCOL_VERSION,
        daemon_version: env!("CARGO_PKG_VERSION").into(),
        build_id: env!("QAQH_BUILD_ID").into(),
        channel: daemon_channel(),
        executable: std::env::current_exe()
            .ok()
            .and_then(|path| path.canonicalize().ok().or(Some(path)))
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default(),
    };
    if !config.bind_ip.is_loopback() {
        log::warn!(
            "[qaqh-daemon] lan server mode on {advertise_ip}:{} — temporary build, no transport security",
            address.port()
        );
    }
    let hub = Arc::new(
        RingingHub::with_persistence(epoch.clone(), data_root.join("ringing"))
            .with_sessions(sessions.clone()),
    );
    let v2_hub = Arc::new(qaqh_runtime::ringing::V2ProjectionHub::new(epoch.clone()));
    let pending_commands = Arc::new(Mutex::new(
        qaqh_runtime::ringing::PendingCommandStore::new_persistent(),
    ));
    // hub-fact-bus spec 阶段 2.3：回执折叠挂在 canonical fact 投影链上
    // （durable-before-publish），不再订阅 v1 事件总线。
    if let Err(existing) = qaqh_session::projection::install_projection_sink(Arc::new(
        FoldingSink {
            v2: v2_hub.clone(),
            pending: pending_commands.clone(),
        },
    )) {
        log::warn!(
            "[ringing-v2] projection sink already installed; keeping existing sink ({existing:p})"
        );
    }
    let service = QaqhService::init(sessions);
    service.attach_ringing(hub.clone());
    service.attach_v2_projection(v2_hub.clone());
    // 宿主直连：`spawn_subagent` 工具此后经进程内宿主句柄运行，不再回连
    // daemon HTTP/SSE（Knife-1 step-2 收尾）。service 已含 registry 与 hub。
    qaqh_subagent::install_host(Arc::new(service.clone()));
    qaqh_subagent::install_task_host(Arc::new(service.clone()));
    qaqh_subagent::install_board_host(Arc::new(service.clone()));
    let ringing_leases = Arc::new(Mutex::new(qaqh_runtime::ringing::RingingLeaseStore::new()));
    // Persisted so a restart still reclaims seats whose holder lease expired
    // while the daemon was down.
    let driver_watch = Arc::new(Mutex::new(
        qaqh_runtime::ringing::RingingDriverWatch::new_persistent(),
    ));
    let (shutdown, _) = watch::channel(false);
    // L1 durability: the daemon previously had NO OS signal handling — Ctrl+C
    // or a terminal close killed the process without running the graceful
    // path below, so in-flight turns never reached messages.jsonl (the whole
    // turn's rounds lived only in the worker's in-memory persist queue).
    // Forward SIGINT/SIGTERM into the same watch channel the /control/v1/stop
    // endpoint uses, so every death mode runs the graceful shutdown:
    // service.shutdown() (cancel + SessionShutdown + join) → seal orphans →
    // flush timeline persistence.
    spawn_signal_shutdown(shutdown.clone());
    let app_state = crate::axum_server::AppState {
        hub: hub.clone(),
        v2_hub: v2_hub.clone(),
        leases: ringing_leases.clone(),
        driver_watch: driver_watch.clone(),
        pending: pending_commands.clone(),
        service: service.clone(),
        token: token.clone(),
        epoch: epoch.clone(),
        shutdown: shutdown.clone(),
        test_hooks: Arc::new(crate::axum_server::TestHooks::from_env()),
    };
    // E: idle 会话卸载周期任务（docs/current/architecture.md）。
    // 每 60s 读一次 config（热生效），对空闲超过阈值的 Session worker 走
    // 优雅 close（join + bundle drop + final flush/drain）。registry.close
    // 是阻塞 join，必须放 spawn_blocking，避免卡死 tokio worker 线程。
    {
        let service = service.clone();
        let mut shutdown_rx = shutdown.subscribe();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        let idle_secs = qaqh_config::watch::authoritative()
                            .map(|config| config.session_idle_unload_secs)
                            .unwrap_or(0);
                        if idle_secs > 0 {
                            let service = service.clone();
                            let _ = tokio::task::spawn_blocking(move || {
                                let unloaded = service.unload_idle_sessions(idle_secs);
                                if !unloaded.is_empty() {
                                    log::info!(
                                        "[daemon] idle-unloaded {} session(s): {:?}",
                                        unloaded.len(),
                                        unloaded
                                    );
                                }
                            })
                            .await;
                        }
                    }
                    changed = shutdown_rx.changed() => {
                        if changed.is_err() || *shutdown_rx.borrow() {
                            break;
                        }
                    }
                }
            }
        });
    }

    // F4: worker reader 线程 panic/崩溃时，registry 会把死实例标记为可重生；
    // 此周期任务负责真正重新拉起，避免单条事件流故障永久饿死会话。
    // 冻结事故（2026-09-02）P0：同一 tick 顺带巡检僵尸 receipt（accepted/running
    // 无终态），让冻结路径在 30s 内于日志可见，不再依赖人肉轮询。
    {
        let service = service.clone();
        let pending_commands = pending_commands.clone();
        let app_state = app_state.clone();
        let mut shutdown_rx = shutdown.subscribe();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(3));
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        service.respawn_dead_agents();
                        pending_commands
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .warn_stale_running(
                                std::time::Duration::from_secs(30),
                                std::time::Duration::from_secs(60),
                            );
                        // Driver seat reclamation: a holder whose lease expired
                        // must not keep the seat (spec §9.3 自动移交).
                        crate::axum_server::reclaim_dead_driver_seats(&app_state);
                    }
                    changed = shutdown_rx.changed() => {
                        if changed.is_err() || *shutdown_rx.borrow() {
                            break;
                        }
                    }
                }
            }
        });
    }
    // 发布 discovery：HTTP accept 循环即将开始（listener 早已 bind，但
    // 端口在 service 初始化完成前不可服务）。延迟发布避免客户端拿到
    // "已写 daemon.json 但 HTTP 未就绪"的假端口而导航失败（白屏/错误页），
    // 也让 ensure_daemon_running 的轮询与真实就绪时刻对齐。
    write_discovery(&discovery)?;
    let app = crate::axum_server::build_router(app_state);
    let mut shutdown_rx = shutdown.subscribe();
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        let _ = shutdown_rx.changed().await;
    })
    .await
    .map_err(stringify)?;
    service.shutdown();
    // 退出前主动收尾孤儿（stop 协议已在 handler 做过；此处兜底其他退出
    // 路径，如生命周期接管/信号退出。幂等：已 seal 的 turn 跳过）。
    hub.seal_all_orphans();
    // F2: timeline 持久化是异步合并 checkpoint；退出前同步落盘全部 pending
    // seed，缩小子进程被杀时 transcript 尾部的丢失窗口。
    hub.flush_timeline_persistence();
    let _ = std::fs::remove_file(qaqh_types::platform::daemon_discovery_path());
    let _ = std::fs::remove_file(qaqh_types::platform::daemon_lock_path());
    Ok(())
}

pub fn random_hex() -> String {
    rand::random::<[u8; 32]>()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Install SIGINT/SIGTERM handlers that forward into the graceful-shutdown
/// watch channel (same path as `POST /control/v1/stop`). Without this, the
/// default OS disposition killed the daemon instantly and every un-drained
/// turn in the in-process workers was lost.
fn spawn_signal_shutdown(shutdown: watch::Sender<bool>) {
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let ctrl_c = tokio::signal::ctrl_c();
            let sigterm = async move {
                match signal(SignalKind::terminate()) {
                    Ok(mut stream) => {
                        stream.recv().await;
                    }
                    Err(error) => {
                        log::error!("[daemon] SIGTERM handler install failed: {error}");
                        // Degrade to Ctrl+C only: park forever on this branch.
                        std::future::pending::<()>().await;
                    }
                }
            };
            tokio::select! {
                _ = ctrl_c => {}
                _ = sigterm => {}
            }
        }
        #[cfg(not(unix))]
        {
            if tokio::signal::ctrl_c().await.is_err() {
                log::error!("[daemon] ctrl_c handler unavailable");
                return;
            }
        }
        log::info!("[daemon] termination signal received — entering graceful shutdown");
        let _ = shutdown.send(true);
    });
}

fn rotate_stale_writer_fences() {
    rotate_stale_writer_fences_in(&qaqh_types::platform::sessions_dir());
}

/// Rotate every canonical writer fence under `sessions_dir` and immediately
/// release the rotated lease with `i64::MIN`.
///
/// This is a startup recovery step: the daemon has just acquired the
/// single-instance lock and no worker exists yet, so a fence on disk cannot be
/// a live writer of this process. Waiting for its TTL would block the first
/// tool/intent/interaction write after a crash for up to `tool_ledger_lease_ms`.
fn rotate_stale_writer_fences_in(sessions_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(sessions_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir()
            || !dir.join(CANONICAL_IDENTITY_FILE).is_file()
            || !dir.join(EVENTS_FILE).is_file()
        {
            continue;
        }
        let session_id = entry.file_name().to_string_lossy().into_owned();
        let result = (|| -> Result<(), String> {
            let identity = CanonicalSessionIdentity::open_or_create(&dir)
                .map_err(|error| error.to_string())?;
            let mut log = CanonicalLog::open(&dir, identity.session_id, identity.log_id)
                .map_err(|error| error.to_string())?;
            let Some(fence) = log.writer_fence().map_err(|error| error.to_string())? else {
                return Ok(());
            };
            let Some(generation_epoch) = fence.generation_epoch.checked_add(1) else {
                return Err("writer fence generation exhausted".into());
            };
            let Some(fencing_token) = fence.fencing_token.checked_add(1) else {
                return Err("writer fence token exhausted".into());
            };
            let now_ms = system_time_ms();
            let lease = log
                .rotate_writer_fence(
                    WriterId::new(format!(
                        "daemon-startup-{}-{}",
                        std::process::id(),
                        session_id
                    )),
                    generation_epoch,
                    fencing_token,
                    now_ms,
                    60_000,
                )
                .map_err(|error| error.to_string())?;
            log.release_writer(&lease, i64::MIN)
                .map_err(|error| error.to_string())?;
            log::info!(
                "[ringing-v2] rotated stale writer fence for {session_id}: generation {} -> {generation_epoch}",
                fence.generation_epoch
            );
            Ok(())
        })();
        if let Err(error) = result {
            log::warn!("[ringing-v2] writer fence rotation skipped for {session_id}: {error}");
        }
    }
}

fn system_time_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

fn stringify(error: impl std::fmt::Display) -> String {
    error.to_string()
}

/// OS 级咨询锁（std `File::try_lock`：Windows `LockFileEx` / Unix `flock`）
/// 保证单实例：持有者是打开的 File 句柄，进程退出（含崩溃）时由内核释放。
/// 因此无需 pid 判活与 stale 锁接管——`WouldBlock` 即代表存在活实例。
fn acquire_single_instance() -> Result<File, String> {
    let path = qaqh_types::platform::daemon_lock_path();
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| format!("open daemon lock {path:?}: {e}"))?;
    file.try_lock()
        .map_err(|_| "another daemon instance is already running".to_string())?;
    // pid 仅作诊断记录，不参与判活。
    writeln!(&file, "{}", std::process::id()).map_err(stringify)?;
    Ok(file)
}
fn write_discovery(discovery: &DaemonDiscovery) -> Result<(), String> {
    let target = qaqh_types::platform::daemon_discovery_path();
    let temp = target.with_extension("json.tmp");
    let mut file = File::create(&temp).map_err(stringify)?;
    serde_json::to_writer_pretty(&mut file, discovery).map_err(stringify)?;
    file.flush().map_err(stringify)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600))
            .map_err(stringify)?;
    }
    if target.exists() {
        std::fs::remove_file(&target).map_err(stringify)?;
    }
    std::fs::rename(temp, &target).map_err(stringify)?;
    restrict_discovery_permissions(&target)
}

#[cfg(windows)]
fn restrict_discovery_permissions(path: &std::path::Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let output = std::process::Command::new("whoami")
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|error| format!("resolve current Windows identity: {error}"))?;
    if !output.status.success() {
        return Err("resolve current Windows identity: whoami failed".into());
    }
    let identity = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let status = std::process::Command::new("icacls")
        .arg(path)
        .args(["/inheritance:r", "/grant:r", &format!("{identity}:(F)")])
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .map_err(|error| format!("restrict daemon discovery ACL: {error}"))?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| "restrict daemon discovery ACL: icacls failed".into())
}

#[cfg(not(windows))]
fn restrict_discovery_permissions(_path: &std::path::Path) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_session::canonical::CanonicalSessionStore;

    #[test]
    fn server_parse_defaults_to_loopback_and_requires_token_for_lan() {
        // 审计 H3：默认面必须收敛到 loopback，LAN 明文 HTTP 必须显式 --token。
        if std::env::var("QAQH_SERVER_TOKEN")
            .map(|value| !value.is_empty())
            .unwrap_or(false)
        {
            // 环境注入了 token 时，「无 token 拒绝」分支无法构造，跳过。
            return;
        }
        let config = ServerNetworkConfig::parse(&[]).expect("default parse");
        assert!(config.bind_ip.is_loopback(), "default bind must be loopback");
        assert_eq!(config.port, 64413);

        let error = ServerNetworkConfig::parse(&["--bind".into(), "0.0.0.0".into()])
            .expect_err("non-loopback without explicit token must be refused");
        assert!(error.contains("--token"), "error: {error}");

        let config = ServerNetworkConfig::parse(&[
            "--bind".into(),
            "0.0.0.0".into(),
            "--token".into(),
            "explicit".into(),
        ])
        .expect("explicit token unlocks LAN bind");
        assert!(!config.bind_ip.is_loopback());
        assert_eq!(config.token.as_deref(), Some("explicit"));

        let config =
            ServerNetworkConfig::parse(&["--bind".into(), "127.0.0.1".into()]).expect("loopback");
        assert!(config.bind_ip.is_loopback());
    }

    #[test]
    fn startup_rotation_releases_a_crashed_writer_fence() {
        let root = tempfile::tempdir().expect("tempdir");
        let dir = root.path().join("seed-crash");
        std::fs::create_dir_all(&dir).expect("session dir");
        let identity = CanonicalSessionIdentity::open_or_create(&dir).expect("identity");
        let mut store =
            CanonicalSessionStore::open(&dir, identity.session_id.clone(), identity.log_id.clone())
                .expect("store");
        let _crashed = store
            .acquire_writer(WriterId::new("crashed-agent"), 1_000, 600_000)
            .expect("crashed lease");
        drop(store);

        rotate_stale_writer_fences_in(root.path());

        let log = CanonicalLog::open(&dir, identity.session_id.clone(), identity.log_id.clone())
            .expect("log");
        let fence = log.writer_fence().expect("fence").expect("rotated fence");
        assert_eq!(fence.lease_expires_at_ms, i64::MIN);
        assert!(fence.writer_id.as_str().starts_with("daemon-startup-"));

        // Even a reader with an ancient clock can take over after rotation.
        let mut store = CanonicalSessionStore::open(&dir, identity.session_id, identity.log_id)
            .expect("reopen");
        let lease = store
            .acquire_writer(WriterId::new("new-agent"), 0, 600_000)
            .expect("new writer");
        store.release_writer(&lease, i64::MIN).expect("release");
    }
}
