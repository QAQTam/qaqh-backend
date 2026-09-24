//! Daemon discovery: read `daemon.json` from the platform data directory and
//! derive the HTTP base URL. The on-disk contract (`DaemonDiscovery` /
//! `CONTROL_PROTOCOL_VERSION`) is single-sourced in `qaqh-types` (PR-3-1);
//! this module owns only the client-side filesystem access and URL derivation.

use std::net::ToSocketAddrs;

use crate::error::{ClientError, Result};

pub use qaqh_types::DaemonDiscovery;

/// Client-side discovery extensions: `DaemonDiscovery` 定义于 `qaqh-types`，
/// 固有 impl 无法跨 crate 附加，故 `base_url` 推导落在本扩展 trait。
pub trait DiscoveryExt {
    /// HTTP base URL derived from discovery endpoint.
    /// Supports both legacy `ws://` (→ `http://`) and new `http://`/`https://`.
    fn base_url(&self) -> Result<String>;
}

impl DiscoveryExt for DaemonDiscovery {
    fn base_url(&self) -> Result<String> {
        let (rest, scheme) = if let Some(r) = self.endpoint.strip_prefix("ws://") {
            (r, "http")
        } else if let Some(r) = self.endpoint.strip_prefix("wss://") {
            (r, "https")
        } else if let Some(r) = self.endpoint.strip_prefix("http://") {
            (r, "http")
        } else if let Some(r) = self.endpoint.strip_prefix("https://") {
            (r, "https")
        } else {
            return Err(ClientError::Discovery(format!(
                "unexpected endpoint: {}",
                self.endpoint
            )));
        };
        let host = rest.split('/').next().unwrap_or("");
        if host.is_empty() {
            return Err(ClientError::Discovery("endpoint has no host".into()));
        }
        Ok(format!("{scheme}://{host}"))
    }
}

/// Platform data directory — single-sourced in `qaqh_types::platform::data_dir`
/// (daemon and client must resolve the same data root).
pub use qaqh_types::platform::data_dir;

/// Path to the discovery file — re-export of the disk-contract helper.
pub use qaqh_types::platform::daemon_discovery_path as discovery_path;

/// Read and parse the discovery file.
pub fn read_discovery() -> Result<DaemonDiscovery> {
    let path = discovery_path();
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| ClientError::Discovery(format!("cannot read {}: {e}", path.display())))?;
    let discovery: DaemonDiscovery = serde_json::from_str(&raw)
        .map_err(|e| ClientError::Discovery(format!("invalid {}: {e}", path.display())))?;
    Ok(discovery)
}

/// Ensure a daemon is running and publish its discovery.
///
/// Synchronous (no tokio runtime needed): spawns `qaqh-daemon run` detached
/// when no discovery file exists, then polls for up to `timeout`. Reuses an
/// existing discovery when the daemon process is alive.
pub fn ensure_daemon_running(timeout: std::time::Duration) -> Result<DaemonDiscovery> {
    if let Ok(discovery) = read_discovery()
        && discovery_is_live(&discovery)
    {
        return Ok(discovery);
    }
    // 已有 daemon 实例正在启动（lock 持有者存活但 discovery 尚未发布——
    // daemon 冷启动初始化可达数十秒，discovery 延迟到 HTTP 就绪后才写）：
    // 不重复 spawn，直接轮询等待其发布。
    if !lock_holder_alive() {
        spawn_daemon_detached()?;
    }
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match read_discovery() {
            Ok(discovery) if discovery_is_live(&discovery) => return Ok(discovery),
            Ok(_) => {}
            Err(_) => {}
        }
        if std::time::Instant::now() >= deadline {
            return Err(ClientError::Discovery(
                "daemon did not publish discovery in time".into(),
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(120));
    }
}

/// 一条 discovery 记录是否指向**真正可用**的 daemon。
///
/// 两道判据缺一不可：
/// 1. **记录的 pid 存活** —— 挡住 daemon 崩溃/被 SIGKILL/重启后留下的陈旧
///    记录（此前非 Windows 的判活是恒 `true` 的 stub，等于完全没挡）；
/// 2. **端点真的在监听** —— 挡住 pid 判活的两处漏网：**pid 复用**（记录里的
///    pid 被无关进程占用）与 **daemon 活着但没在听**（启动中卡住、监听 socket
///    已关闭）。少了这一条，客户端会拿一个死端点去连，`Connection refused`
///    之后**不会**回退到拉起 daemon。
///
/// 判据 2 是本地回环的一次 TCP 连接（≤[`ENDPOINT_PROBE_TIMEOUT`]），只在
/// 「已有 discovery 记录」的连接路径上发生，不进热路径。
pub(crate) fn discovery_is_live(discovery: &DaemonDiscovery) -> bool {
    process_is_running(discovery.pid) && endpoint_reachable(discovery)
}

/// 端点探测超时。本地回环，300ms 已是极大宽限。
const ENDPOINT_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(300);

/// 端点是否连得上。**判不了就返回 `true`**——宁可放行，也不要因误判「陈旧」
/// 而对一个其实健康的 daemon 重复拉起。
fn endpoint_reachable(discovery: &DaemonDiscovery) -> bool {
    // 非 http（如远端 https）不做此检查：那是另一套信任与超时模型。
    let Some(authority) = discovery
        .base_url()
        .ok()
        .and_then(|url| url.strip_prefix("http://").map(str::to_owned))
    else {
        return true;
    };
    let authority = authority.split('/').next().unwrap_or_default();
    if authority.is_empty() {
        return true;
    }
    let Ok(mut addrs) = authority.to_socket_addrs() else {
        return true;
    };
    let Some(addr) = addrs.next() else {
        return true;
    };
    std::net::TcpStream::connect_timeout(&addr, ENDPOINT_PROBE_TIMEOUT).is_ok()
}

/// 检查 `daemon.lock` 持有者进程是否存活（daemon 单实例锁，见
/// `qaqh-daemon::server::acquire_single_instance`）。lock 持有者活着即
/// 意味着有 daemon 正在启动/运行，即使 `daemon.json` 尚未发布。
/// `pub(crate)`：`client::wait_for_daemon` 在 spawn 前据此避免重复拉起。
pub(crate) fn lock_holder_alive() -> bool {
    // 曾经非 Windows 直接 `false`（因为判活是恒 true 的 stub，读了也没用），
    // 代价是「daemon 正在冷启动、discovery 尚未发布」时会被重复 spawn。判活
    // 修好后两个平台同构，不再分叉。
    let lock = data_dir().join("daemon.lock");
    match std::fs::read_to_string(&lock)
        .ok()
        .and_then(|value| value.trim().parse::<u32>().ok())
    {
        Some(pid) => process_is_running(pid),
        None => false,
    }
}

/// Resolve the daemon executable.
///
/// Candidate order (first hit wins):
///   1. `QAQH_BACKEND_ROOT/target/debug/qaqh-daemon` — dev
///   2. `<cwd>/target/debug/qaqh-daemon` — dev
///   3. `<exe_dir>/resources/qaqh-daemon` — packaged layout (installer keeps
///      the daemon inside the shell's resources dir; mirrors Electron sidecar)
///   4. `<exe_dir>/qaqh-daemon` — side-by-side layout
///   5. bare name (PATH lookup)
pub fn daemon_executable() -> std::path::PathBuf {
    let exe = if cfg!(windows) {
        "qaqh-daemon.exe"
    } else {
        "qaqh-daemon"
    };

    for base in [
        std::env::var("QAQH_BACKEND_ROOT").ok(),
        std::env::current_dir()
            .ok()
            .map(|p| p.display().to_string()),
    ]
    .into_iter()
    .flatten()
    {
        let p = std::path::PathBuf::from(base)
            .join("target")
            .join("debug")
            .join(exe);
        if p.exists() {
            return p;
        }
    }

    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
    {
        for base in [dir.join("resources"), dir.clone()] {
            let p = base.join(exe);
            if p.exists() {
                return p;
            }
        }
    }

    std::path::PathBuf::from(exe)
}

/// 以「脱离当前 shell」的方式拉起 daemon 进程。**daemon 的唯一 spawn 出口**
/// （此前 `discovery::spawn_daemon_detached` 与 `client::spawn_detached` 各写一份，
/// 加保护极易只改一处）。
///
/// **为什么必须脱离**（BUG-2026-09-15-04）：daemon 与 shell 同进程组、同会话时
/// （实测：daemon 的 PGID == 起它的 TUI 的 PGID，PPID 即 TUI），终端一收尾——
/// 关窗、Ctrl+C 打到前台组、父进程被 SIGTERM——daemon 会被一并收走。它来不及
/// 清理 `daemon.json`，于是留下一条陈旧记录；判活若不可靠
/// （BUG-2026-09-15-03），下一个 shell 就被那条记录永久卡死。设计上 daemon 本就
/// 该跨 shell 复用（`daemon.lock` 单实例锁、`/control/v1/stop-if-idle` 都指向
/// 这一点），原先的行为与设计相反。
///
/// Unix 用 `process_group(0)` 让 daemon 自成一个进程组（pgid = 自身 pid）——终端
/// 关闭时的 SIGHUP 只发给**前台进程组**，故此一步即足以保命；std 自带，无需引
/// libc。Windows 仍用 `CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW`。
pub(crate) fn spawn_daemon_process(executable: &std::path::Path) -> Result<()> {
    log::info!("[qaqh-client] spawning daemon: {}", executable.display());
    let mut command = std::process::Command::new(executable);
    command
        .arg("run")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    configure_detached(&mut command);
    let _ = command.spawn()?;
    Ok(())
}

/// 让 `command` 拉起的子进程脱离调用方的进程组。抽出来是为了可测——这是
/// 「daemon 会不会随 shell 一起死」的唯一开关。
fn configure_detached(command: &mut std::process::Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
}

fn spawn_daemon_detached() -> Result<()> {
    spawn_daemon_process(&daemon_executable())
}

/// Process liveness probe — client-side implementation, deliberately distinct
/// from [`qaqh_types::platform::process_is_running`]: on Windows this one uses
/// the Win32 API directly (no `tasklist` subprocess latency). On non-Windows it
/// **delegates** to that same function (`kill -0`).
///
/// 曾经这里在非 Windows 上是 `true` 的 stub（注释写「discovery 文件存在即视为
/// 存活」）。那不是占位符无伤大雅，而是**把一个陈旧 `daemon.json` 变成永久
/// 砖**：daemon 被 SIGKILL/崩溃/重启后记录仍在，于是
/// [`ensure_daemon_running`] 与 `Client::connect_async` 的「pid 判活过滤」
/// 双双通过，客户端拿死端点去连 → `Connection refused`，
/// **且永不回退到拉起 daemon**。任何 shell 都得手工删文件才恢复（真机复现：
/// 记录 pid 已死、端口无人监听，TUI 仍直连该端口报错）。
#[cfg(windows)]
pub fn process_is_running(pid: u32) -> bool {
    let handle = unsafe {
        windows_sys::Win32::System::Threading::OpenProcess(
            windows_sys::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            pid,
        )
    };
    if handle.is_null() {
        return false;
    }
    let exit_code = unsafe {
        let mut code: u32 = 0;
        windows_sys::Win32::System::Threading::GetExitCodeProcess(handle, &mut code);
        code
    };
    unsafe {
        let _ = windows_sys::Win32::Foundation::CloseHandle(handle);
    }
    exit_code == 259 // STILL_ACTIVE
}

#[cfg(not(windows))]
pub fn process_is_running(pid: u32) -> bool {
    qaqh_types::platform::process_is_running(pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    // 前端契约冻结面锚点：`docs/current/architecture.md`
    // （原注释指向的 `frontend-contract.md` 从不存在，已改指该文件）。discovery endpoint 的兼容解析。
    // 旧形态 ws://host:port/control/v1 必须无损转 http://，新形态原样通过；
    // 破坏任一分支即破坏已发布客户端的 discovery 兼容。
    #[test]
    fn base_url_accepts_legacy_ws_and_new_http_forms() {
        let legacy = DaemonDiscovery {
            endpoint: "ws://127.0.0.1:9101/control/v1".into(),
            token: String::new(),
            pid: 0,
            server_epoch: String::new(),
            protocol_version: 1,
            daemon_version: String::new(),
            build_id: String::new(),
            channel: String::new(),
            executable: String::new(),
        };
        assert_eq!(legacy.base_url().unwrap(), "http://127.0.0.1:9101");

        let modern = DaemonDiscovery {
            endpoint: "http://127.0.0.1:9101".into(),
            ..legacy
        };
        assert_eq!(modern.base_url().unwrap(), "http://127.0.0.1:9101");
    }

    // PR-3-1 兼容红线：旧格式 daemon.json（无 build_id/channel/executable 三字段）
    // 必须可解析，且解析 → 序列化 → 再解析字段逐字段保全。
    #[test]
    fn legacy_discovery_json_roundtrip_preserves_fields() {
        let legacy = r#"{
            "endpoint": "http://127.0.0.1:41831",
            "token": "tok-123",
            "pid": 4242,
            "server_epoch": "epoch-1",
            "protocol_version": 1,
            "daemon_version": "0.8.9"
        }"#;
        let parsed: DaemonDiscovery =
            serde_json::from_str(legacy).expect("legacy 6-field sample must parse");
        assert_eq!(parsed.endpoint, "http://127.0.0.1:41831");
        assert_eq!(parsed.token, "tok-123");
        assert_eq!(parsed.pid, 4242);
        assert_eq!(parsed.server_epoch, "epoch-1");
        assert_eq!(parsed.protocol_version, 1);
        assert_eq!(parsed.daemon_version, "0.8.9");
        // 新增字段缺省（pre-0.9 兼容语义）
        assert_eq!(parsed.build_id, "");
        assert_eq!(parsed.channel, "");
        assert_eq!(parsed.executable, "");

        let json = serde_json::to_string(&parsed).expect("serialize");
        let reparsed: DaemonDiscovery = serde_json::from_str(&json).expect("reparse");
        assert_eq!(reparsed, parsed);
    }

    // ── 陈旧 discovery 判活（真机事故回归）────────────────────────────────
    //
    // 曾经非 Windows 的 `process_is_running` 是恒 `true` 的 stub，于是 daemon
    // 被 SIGKILL 后留下的 `daemon.json` 成了**永久砖**：判活过滤全部放行，
    // 客户端拿死端点去连 → `Connection refused`，且永不回退到拉起 daemon。
    // 实测现场：记录 pid 已死、端口无人监听，TUI 仍直连该端口报错。

    /// 死 pid 必须判死。stub 恒 `true` 时这条会红。
    #[test]
    fn process_is_running_is_false_for_a_reaped_child() {
        let mut child = std::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .spawn()
            .expect("spawn sh");
        let pid = child.id();
        child.wait().expect("wait child");
        assert!(
            !process_is_running(pid),
            "已回收的子进程 pid {pid} 必须判死"
        );
    }

    #[test]
    fn process_is_running_is_true_for_self() {
        assert!(process_is_running(std::process::id()));
    }

    fn discovery_at(endpoint: String, pid: u32) -> DaemonDiscovery {
        DaemonDiscovery {
            endpoint,
            token: String::new(),
            pid,
            server_epoch: String::new(),
            protocol_version: 1,
            daemon_version: String::new(),
            build_id: String::new(),
            channel: String::new(),
            executable: String::new(),
        }
    }

    /// 取一个「刚被释放、几乎必然无人在听」的回环端口。
    fn free_port() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        listener.local_addr().expect("local_addr").port()
    }

    /// pid 判活的漏网之一：**端口没人听**（daemon 启动中卡住、监听已关）。
    /// 只判 pid 会让客户端拿死端点去连且不回退。
    #[test]
    fn discovery_is_dead_when_endpoint_refuses() {
        let port = free_port();
        let d = discovery_at(format!("http://127.0.0.1:{port}"), std::process::id());
        assert!(
            !discovery_is_live(&d),
            "端口 {port} 无人监听，即便 pid 存活也必须判死"
        );
    }

    /// pid 判活的漏网之二：**pid 复用**——记录里的 pid 被无关进程占用。
    /// 这里用「stub 恒 true 时唯一会放行的组合」来逼近：存活 pid + 死端口。
    #[test]
    fn discovery_is_dead_when_pid_is_gone_even_if_endpoint_listens() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("local_addr").port();
        let mut child = std::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .spawn()
            .expect("spawn sh");
        let pid = child.id();
        child.wait().expect("wait child");
        let d = discovery_at(format!("http://127.0.0.1:{port}"), pid);
        assert!(
            !discovery_is_live(&d),
            "记录里的 pid 已死 → 判死（这正是真机那条）"
        );
    }

    /// BUG-2026-09-15-04：daemon 必须**自成进程组**，否则终端收尾时会被连同
    /// shell 的前台进程组一起收走（实测：修复前 daemon 的 PGID == 起它的 TUI 的
    /// PGID，shell 退出后 daemon 消失并留下陈旧 `daemon.json`）。
    ///
    /// 限定 Linux：读 `/proc/<pid>/stat` 的 pgrp 字段。
    /// 破坏验证：去掉 `configure_detached` 里的 `process_group(0)` → 本测试红。
    #[cfg(target_os = "linux")]
    #[test]
    fn detached_spawn_lands_in_its_own_process_group() {
        let mut command = std::process::Command::new("sleep");
        command.arg("30");
        configure_detached(&mut command);
        let mut child = command.spawn().expect("spawn sleep");
        let pid = child.id();
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).expect("read stat");
        // `pid (comm) state ppid pgrp session ...`；comm 可能含空格与括号，
        // 故从 `) ` 之后开始数：state / ppid / pgrp。
        let pgrp: u32 = stat
            .rsplit_once(") ")
            .expect("comm 以 `) ` 收尾")
            .1
            .split_whitespace()
            .nth(2)
            .expect("pgrp 字段")
            .parse()
            .expect("pgrp 是数字");
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(pgrp, pid, "daemon 必须自成一个进程组（pgid == 自身 pid）");
    }

    /// 反向闸：健康的 daemon 不能被误判为陈旧，否则会对在跑的实例重复拉起。
    #[test]
    fn healthy_discovery_is_live() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("local_addr").port();
        let d = discovery_at(format!("http://127.0.0.1:{port}"), std::process::id());
        assert!(discovery_is_live(&d), "有人在听 + pid 存活 → 判活");
    }
}
