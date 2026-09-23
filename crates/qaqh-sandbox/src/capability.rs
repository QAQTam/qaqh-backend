//! Platform capability discovery.
//!
//! This is intentionally descriptive, not a startup gate. A platform with no
//! OS sandbox reports that fact and the caller chooses a preset or an explicit
//! bypass; it does not make the daemon unstartable.

use serde::{Deserialize, Serialize};

/// Operating-system family used by policy presets and diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    Linux,
    Macos,
    Windows,
    Other,
}

/// Enforcing backend available on the current host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxBackend {
    /// Linux Landlock + seccomp helper.
    LinuxLandlockSeccomp,
    /// Process-tree/resource hardening only; no filesystem/network sandbox.
    ProcessHardening,
    /// No enforcing backend is currently wired.
    None,
}

/// Capability matrix exposed to policy and diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxCapabilities {
    pub platform: Platform,
    pub backend: SandboxBackend,
    pub filesystem_write_isolation: bool,
    pub network_isolation: bool,
    pub process_hardening: bool,
    pub detail: String,
}

impl SandboxCapabilities {
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
    if landlock && seccomp {
        SandboxCapabilities {
            platform: Platform::Linux,
            backend: SandboxBackend::LinuxLandlockSeccomp,
            filesystem_write_isolation: true,
            network_isolation: true,
            process_hardening: true,
            detail: "Landlock LSM + seccomp-bpf available".into(),
        }
    } else {
        SandboxCapabilities {
            platform: Platform::Linux,
            backend: SandboxBackend::ProcessHardening,
            filesystem_write_isolation: false,
            network_isolation: false,
            process_hardening: true,
            detail: format!(
                "Landlock unavailable (landlock={landlock}, seccomp={seccomp}); process hardening only"
            ),
        }
    }
}
