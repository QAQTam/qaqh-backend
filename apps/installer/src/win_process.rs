// Windows 进程检测与终止
//   同用户进程无需管理员权限

use std::thread;
use std::time::Duration;

/// 已知的 DeepX 进程名
const DEEPX_PROCESSES: &[&str] = &[
    "DeepX.exe",
    "deepx-daemon.exe",
    // The workspace service can hold files in the install directory during an
    // upgrade, so it must use the same warning-and-confirmed-termination flow.
    "deepx-workspace.exe",
];

/// 进程信息
#[derive(Clone, Debug)]
pub struct ProcInfo {
    pub pid: u32,
    pub name: String,
    pub closed: bool,
}

// ============================================================
// 进程检测
// ============================================================

pub fn find_deepx_processes() -> Vec<ProcInfo> {
    find_via_toolhelp().unwrap_or_else(|| find_via_tasklist())
}

/// 通过 Toolhelp 快照检测（主要方法）
fn find_via_toolhelp() -> Option<Vec<ProcInfo>> {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
            TH32CS_SNAPPROCESS,
        };

        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
        let mut pe = PROCESSENTRY32W::default();
        pe.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;

        let mut result = Vec::new();
        if Process32FirstW(snap, &mut pe).is_ok() {
            loop {
                let len = pe
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(pe.szExeFile.len());
                let name = String::from_utf16_lossy(&pe.szExeFile[..len]);
                if DEEPX_PROCESSES
                    .iter()
                    .any(|p| p.eq_ignore_ascii_case(&name))
                {
                    result.push(ProcInfo {
                        pid: pe.th32ProcessID,
                        name,
                        closed: false,
                    });
                }
                if Process32NextW(snap, &mut pe).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
        Some(result)
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// fallback：tasklist
fn find_via_tasklist() -> Vec<ProcInfo> {
    let mut result = Vec::new();
    for name in DEEPX_PROCESSES {
        if let Ok(out) = std::process::Command::new("tasklist")
            .args([
                "/fo",
                "csv",
                "/nh",
                "/fi",
                &format!("imagename eq {}", name),
            ])
            .output()
        {
            let text = String::from_utf8_lossy(&out.stdout);
            for line in text.lines() {
                let parts: Vec<&str> = line
                    .split(',')
                    .map(|s| s.trim_matches('"').trim())
                    .collect();
                if parts.len() >= 2 {
                    if let Ok(pid) = parts[1].parse() {
                        result.push(ProcInfo {
                            pid,
                            name: name.to_string(),
                            closed: false,
                        });
                    }
                }
            }
        }
    }
    result
}

// ============================================================
// 进程关闭
// ============================================================

/// 关闭 DeepX 运行时进程。
///
/// 策略（解决安装时 ~30s 卡顿问题）：
/// - **DeepX.exe（WinUI3 原生壳）**：直接 `TerminateProcess` 强杀。
///   原因：`taskkill` 不带 `/f` 发 WM_CLOSE 后，壳的退出路径会执行
///   daemon 连接清理（含 detach 等待），如果 daemon 正处于连接故障态，
///   该回调可阻塞到系统 ~30s 的"程序未响应"超时才返回。对安装场景而言
///   这个延迟不可接受。
///
/// - **deepx-daemon.exe（后台 daemon）**：先发 HTTP `/control/v1/stop`
///   优雅关闭（daemon 没有窗口消息泵，WM_CLOSE 对它无效），等 2s，
///   仍未退出则强杀。给 daemon 一个短窗口做 WS 关闭/文件 flush，
///   但不无限等待。
pub fn graceful_close(procs: &mut [ProcInfo]) {
    // ── 第一步：daemon 优雅关闭（HTTP stop，等 200 = 收尾完成确认）──
    let _confirmed = stop_daemon_via_http();

    // ── 第二步：DeepX.exe 直接强杀（不等 WM_CLOSE / before-quit）──
    for p in procs.iter_mut() {
        if p.name.eq_ignore_ascii_case("DeepX.exe") && is_process_running(p.pid) {
            force_terminate(p.pid);
            p.closed = !is_process_running(p.pid);
        }
    }

    // ── 第三步：daemon 给 2s 优雅退出窗口 ──
    let daemon_deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < daemon_deadline {
        let all_daemon_gone = procs
            .iter()
            .all(|p| p.name.eq_ignore_ascii_case("DeepX.exe") || !is_process_running(p.pid));
        if all_daemon_gone {
            break;
        }
        thread::sleep(Duration::from_millis(200));
    }

    // ── 第四步：daemon 仍未退出 → 强杀 ──
    for p in procs.iter_mut() {
        if !p.name.eq_ignore_ascii_case("DeepX.exe") && is_process_running(p.pid) {
            force_terminate(p.pid);
        }
        p.closed = !is_process_running(p.pid);
    }
}

/// 通过 daemon 的 HTTP 端点发送优雅停止请求并等待收尾确认。
///
/// Windows 95 语义：daemon 完成收尾（worker 优雅退出 + stdout 管道排空 +
/// seal 孤儿 + flush timeline）后才返回 200——读到 200 即"可以安全关闭"。
/// 10s 超时未确认返回 false（调用方走强杀兜底）。
fn stop_daemon_via_http() -> bool {
    use std::io::{Read, Write};

    let home = std::env::var("USERPROFILE").unwrap_or_default();
    let discovery_path = std::path::PathBuf::from(&home)
        .join(".deepx")
        .join("daemon.json");

    let content = match std::fs::read_to_string(&discovery_path) {
        Ok(c) => c,
        Err(_) => return false, // 没有 discovery 文件，daemon 未运行
    };

    // 用 serde_json 解析（已在 Cargo.toml 依赖中）
    let discovery: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return false,
    };

    let endpoint = discovery
        .get("endpoint")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let token = discovery
        .get("token")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if endpoint.is_empty() || token.is_empty() {
        return false;
    }

    // 从 "ws://127.0.0.1:PORT/control/v1" 提取 socket 地址
    let addr = endpoint
        .trim_start_matches("ws://")
        .split('/')
        .next()
        .unwrap_or("");

    let Ok(socket_addr) = addr.parse::<std::net::SocketAddr>() else {
        return false;
    };

    let Ok(mut stream) =
        std::net::TcpStream::connect_timeout(&socket_addr, std::time::Duration::from_secs(2))
    else {
        return false;
    };
    // 收尾确认等待上限：worker 优雅退出（≤5s）+ seal/flush 均在其内。
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(10)));

    let request = format!(
        "POST /control/v1/stop HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        addr, token
    );

    let _ = stream.write_all(request.as_bytes());
    // 读响应直到 200（daemon 收尾完成信号）或超时。
    let mut buf = [0_u8; 256];
    match stream.read(&mut buf) {
        Ok(n) if n > 0 => {
            let resp = String::from_utf8_lossy(&buf[..n]);
            let confirmed = resp.starts_with("HTTP/1.1 200");
            if !confirmed {
                eprintln!("[installer] daemon stop not confirmed: {}", resp.lines().next().unwrap_or("?"));
            }
            confirmed
        }
        _ => false,
    }
}

/// 强制终止：TerminateProcess（内核强杀）+ taskkill /f /t 兜底。
/// 保证子进程树也被关闭。
pub fn force_terminate(pid: u32) -> bool {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
        if let Ok(h) = OpenProcess(PROCESS_TERMINATE, false, pid) {
            let ok = TerminateProcess(h, 0).is_ok();
            let _ = CloseHandle(h);
            if ok {
                return true;
            }
        }
    }
    // fallback: taskkill /f /t 确保子进程树也被杀
    std::process::Command::new("taskkill")
        .args(["/f", "/t", "/pid", &pid.to_string()])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// 等待进程退出
pub fn wait_for_exit(procs: &[ProcInfo], timeout_secs: u64) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs);
    let pids: Vec<u32> = procs.iter().map(|p| p.pid).collect();
    while std::time::Instant::now() < deadline {
        if pids.iter().all(|&pid| !is_process_running(pid)) {
            return true;
        }
        thread::sleep(Duration::from_millis(300));
    }
    false
}

/// 检查进程是否仍在运行
pub fn is_alive(pid: u32) -> bool {
    is_process_running(pid)
}

#[cfg(test)]
mod tests {
    use super::DEEPX_PROCESSES;

    #[test]
    fn workspace_service_is_part_of_the_installer_shutdown_allowlist() {
        assert!(DEEPX_PROCESSES
            .iter()
            .any(|name| name.eq_ignore_ascii_case("deepx-workspace.exe")));
    }
}

fn is_process_running(pid: u32) -> bool {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        if let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            let mut code: u32 = 259; // STILL_ACTIVE
            let _ = GetExitCodeProcess(h, &mut code);
            let _ = CloseHandle(h);
            return code == 259;
        }
        false
    }
    #[cfg(not(windows))]
    {
        let _ = pid;
        false
    }
}
