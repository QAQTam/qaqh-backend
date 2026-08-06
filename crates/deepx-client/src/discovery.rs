//! Daemon discovery: read `daemon.json` from the platform data directory and
//! derive the HTTP base URL. Mirrors `electron/controlClient.ts` + `deepx-proto`.

use std::path::PathBuf;

use serde::Deserialize;

use crate::error::{ClientError, Result};

/// Contents of `<data-dir>/daemon.json` (see `deepx-proto::DaemonDiscovery`).
#[derive(Debug, Clone, Deserialize)]
pub struct DaemonDiscovery {
    /// `ws://<host>:<port>/control/v1`
    pub endpoint: String,
    pub token: String,
    pub pid: u32,
    pub server_epoch: String,
    pub protocol_version: u16,
    #[serde(default)]
    pub daemon_version: String,
}

impl DaemonDiscovery {
    /// HTTP base URL derived from the WS endpoint (`ws://` → `http://`).
    pub fn base_url(&self) -> Result<String> {
        let rest = self
            .endpoint
            .strip_prefix("ws://")
            .or_else(|| self.endpoint.strip_prefix("wss://"))
            .ok_or_else(|| ClientError::Discovery(format!("unexpected endpoint: {}", self.endpoint)))?;
        let host = rest
            .split('/')
            .next()
            .unwrap_or("");
        if host.is_empty() {
            return Err(ClientError::Discovery("endpoint has no host".into()));
        }
        let scheme = if self.endpoint.starts_with("wss://") { "https" } else { "http" };
        Ok(format!("{scheme}://{host}"))
    }
}

/// Platform data directory. `DEEPX_DATA_DIR` overrides when set (used by test
/// harnesses and multi-instance shells); otherwise Windows:
/// `%USERPROFILE%\.deepx`; Unix: `$XDG_CONFIG_HOME/deepx` or `~/.config/deepx`.
pub fn data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("DEEPX_DATA_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    if cfg!(windows) {
        if let Some(profile) = std::env::var_os("USERPROFILE") {
            return PathBuf::from(profile).join(".deepx");
        }
    } else if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(xdg).join("deepx");
    } else if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".config").join("deepx");
    }
    PathBuf::from(".deepx")
}

/// Path to the discovery file.
pub fn discovery_path() -> PathBuf {
    data_dir().join("daemon.json")
}

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
/// Synchronous (no tokio runtime needed): spawns `deepx-daemon run` detached
/// when no discovery file exists, then polls for up to `timeout`. Reuses an
/// existing discovery when the daemon process is alive.
pub fn ensure_daemon_running(timeout: std::time::Duration) -> Result<DaemonDiscovery> {
    if let Ok(discovery) = read_discovery() {
        if process_is_running(discovery.pid) {
            return Ok(discovery);
        }
    }
    spawn_daemon_detached()?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match read_discovery() {
            Ok(discovery) if process_is_running(discovery.pid) => return Ok(discovery),
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

/// Resolve the daemon executable.
///
/// Candidate order (first hit wins):
///   1. `DEEPX_BACKEND_ROOT/target/debug/deepx-daemon` — dev
///   2. `<cwd>/target/debug/deepx-daemon` — dev
///   3. `<exe_dir>/resources/deepx-daemon` — packaged layout (installer keeps
///      the daemon inside the shell's resources dir; mirrors Electron sidecar)
///   4. `<exe_dir>/deepx-daemon` — side-by-side layout
///   5. bare name (PATH lookup)
pub fn daemon_executable() -> std::path::PathBuf {
    let exe = if cfg!(windows) { "deepx-daemon.exe" } else { "deepx-daemon" };

    for base in [
        std::env::var("DEEPX_BACKEND_ROOT").ok(),
        std::env::current_dir().ok().map(|p| p.display().to_string()),
    ]
    .into_iter()
    .flatten()
    {
        let p = std::path::PathBuf::from(base).join("target").join("debug").join(exe);
        if p.exists() {
            return p;
        }
    }

    if let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf())) {
        for base in [dir.join("resources"), dir.clone()] {
            let p = base.join(exe);
            if p.exists() {
                return p;
            }
        }
    }

    std::path::PathBuf::from(exe)
}

fn spawn_daemon_detached() -> Result<()> {
    let executable = daemon_executable();
    log::info!("[deepx-client] spawning daemon: {}", executable.display());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        let _ = std::process::Command::new(&executable)
            .arg("run")
            .creation_flags(CREATE_NEW_PROCESS_GROUP)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new(&executable)
            .arg("run")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        Ok(())
    }
}

#[cfg(windows)]
fn process_is_running(pid: u32) -> bool {
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
fn process_is_running(_pid: u32) -> bool {
    true // discovery presence is the check on non-Windows for now
}
