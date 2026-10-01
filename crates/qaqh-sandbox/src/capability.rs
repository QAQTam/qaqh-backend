//! Platform capability discovery.
//!
//! This is intentionally descriptive, not a startup gate. A platform with no
//! OS sandbox reports that fact and the caller chooses a preset or an explicit
//! bypass; it does not make the daemon unstartable.

use serde::{Deserialize, Serialize};
#[cfg(target_os = "linux")]
use std::process::Command;
#[cfg(target_os = "linux")]
use std::sync::OnceLock;

pub use qaqh_policy::SandboxBackend;

#[cfg(target_os = "linux")]
const USER_NAMESPACE_FAILURES: [&str; 4] = [
    "loopback: Failed RTM_NEWADDR",
    "loopback: Failed RTM_NEWLINK",
    "setting up uid map: Permission denied",
    "No permissions to create a new namespace",
];

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct BubblewrapSupport {
    available: bool,
    detail: String,
}

#[cfg(target_os = "linux")]
static BUBBLEWRAP_SUPPORT: OnceLock<BubblewrapSupport> = OnceLock::new();

/// Operating-system family used by policy presets and diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    Linux,
    Macos,
    Windows,
    Other,
}

/// Capability matrix exposed to policy and diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxCapabilities {
    pub platform: Platform,
    pub backend: SandboxBackend,
    pub landlock: bool,
    pub seccomp: bool,
    pub bubblewrap: bool,
    pub filesystem_write_isolation: bool,
    pub network_isolation: bool,
    pub process_hardening: bool,
    pub detail: String,
}

impl SandboxCapabilities {
    /// Human-readable platform name for warnings and banners.
    pub fn platform_str(&self) -> &'static str {
        match self.platform {
            Platform::Linux => "linux",
            Platform::Macos => "macOS",
            Platform::Windows => "Windows",
            Platform::Other => std::env::consts::OS,
        }
    }

    /// Detect the current platform without failing startup.
    pub fn detect() -> Self {
        #[cfg(target_os = "linux")]
        {
            return detect_linux();
        }
        #[cfg(target_os = "macos")]
        {
            return Self {
                platform: Platform::Macos,
                backend: SandboxBackend::None,
                landlock: false,
                seccomp: false,
                bubblewrap: false,
                filesystem_write_isolation: false,
                network_isolation: false,
                process_hardening: true,
                detail: "macOS Seatbelt backend is not wired in the Linux-first cut".into(),
            };
        }
        #[cfg(target_os = "windows")]
        {
            return Self {
                platform: Platform::Windows,
                backend: SandboxBackend::None,
                landlock: false,
                seccomp: false,
                bubblewrap: false,
                filesystem_write_isolation: false,
                network_isolation: false,
                process_hardening: true,
                detail: "Windows AppContainer/restricted-token backend is not wired yet".into(),
            };
        }
        #[allow(unreachable_code)]
        Self {
            platform: Platform::Other,
            backend: SandboxBackend::None,
            landlock: false,
            seccomp: false,
            bubblewrap: false,
            filesystem_write_isolation: false,
            network_isolation: false,
            process_hardening: false,
            detail: "unsupported platform".into(),
        }
    }
}

#[cfg(target_os = "linux")]
fn detect_linux() -> SandboxCapabilities {
    let lsm = std::fs::read_to_string("/sys/kernel/security/lsm").unwrap_or_default();
    let landlock = lsm.split(',').any(|entry| entry.trim() == "landlock");
    // Seccomp is a kernel facility available on all supported Linux targets;
    // installation is verified by the helper itself and remains fail-closed.
    let seccomp = std::path::Path::new("/proc/self/status").is_file();
    let bubblewrap = bubblewrap_support();

    let (backend, detail) = if bubblewrap.available {
        (
            SandboxBackend::LinuxBubblewrap,
            format!("bubblewrap available: {}", bubblewrap.detail),
        )
    } else if landlock && seccomp {
        (
            SandboxBackend::LinuxLandlockSeccomp,
            format!(
                "Landlock LSM + seccomp-bpf available; bubblewrap unavailable: {}",
                bubblewrap.detail
            ),
        )
    } else {
        (
            SandboxBackend::ProcessHardening,
            format!(
                "Landlock unavailable (landlock={landlock}, seccomp={seccomp}); bubblewrap unavailable: {}",
                bubblewrap.detail
            ),
        )
    };

    SandboxCapabilities {
        platform: Platform::Linux,
        backend,
        landlock,
        seccomp,
        bubblewrap: bubblewrap.available,
        filesystem_write_isolation: backend != SandboxBackend::ProcessHardening,
        network_isolation: backend != SandboxBackend::ProcessHardening,
        process_hardening: true,
        detail,
    }
}

#[cfg(target_os = "linux")]
fn bubblewrap_support() -> &'static BubblewrapSupport {
    BUBBLEWRAP_SUPPORT.get_or_init(|| {
        let version = match Command::new("bwrap").arg("--version").output() {
            Ok(output) if output.status.success() => {
                String::from_utf8_lossy(&output.stdout).trim().to_string()
            }
            Ok(output) => {
                return BubblewrapSupport {
                    available: false,
                    detail: format!(
                        "bwrap --version failed: {}",
                        String::from_utf8_lossy(&output.stderr).trim()
                    ),
                };
            }
            Err(error) => {
                return BubblewrapSupport {
                    available: false,
                    detail: format!("bwrap not found on PATH: {error}"),
                };
            }
        };

        match Command::new("bwrap")
            .args([
                "--unshare-user",
                "--unshare-net",
                "--ro-bind",
                "/",
                "/",
                "/bin/true",
            ])
            .output()
        {
            Ok(output) if output.status.success() => BubblewrapSupport {
                available: true,
                detail: format!("{version}; user namespace probe passed"),
            },
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let failure = USER_NAMESPACE_FAILURES
                    .iter()
                    .find(|failure| stderr.contains(**failure));
                BubblewrapSupport {
                    available: false,
                    detail: match failure {
                        Some(failure) => {
                            format!("{version}; user namespace probe failed: {failure}")
                        }
                        None => format!("{version}; probe failed: {}", stderr.trim()),
                    },
                }
            }
            Err(error) => BubblewrapSupport {
                available: false,
                detail: format!("{version}; probe spawn failed: {error}"),
            },
        }
    })
}
