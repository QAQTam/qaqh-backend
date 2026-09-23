//! Cross-platform sandbox capability boundary.
//!
//! The first production backend is Linux-only: Landlock for filesystem/TCP
//! policy and seccomp-bpf for syscall/network denial. Other platforms still
//! start normally and report a degraded capability set; they must not turn a
//! missing Linux primitive into a daemon-startup failure.
//!
//! Enforcement happens in a short-lived helper process. The daemon never calls
//! `restrict_self()` in-process: Landlock is irreversible and `pre_exec` in a
//! multithreaded daemon is not an acceptable place to build policy.

use std::path::PathBuf;
use std::process::Command;

#[cfg(target_os = "linux")]
mod linux;

pub mod capability;
pub mod protocol;

pub use capability::{Platform, SandboxBackend, SandboxCapabilities};
pub use protocol::SandboxRequest;
pub use qaqh_policy::{NetworkPolicy, SandboxSpec};

/// Hidden subcommand used when the daemon binary acts as its own helper.
pub const HELPER_SUBCOMMAND: &str = "__qaqh-sandbox-exec";

/// Absolute path to the helper executable. When absent, callers use the
/// unsandboxed process path and rely on platform capability reporting.
pub const HELPER_ENV: &str = "QAQH_SANDBOX_EXEC";

/// Configure the current daemon binary as the Linux helper when the kernel
/// supports the required primitives. This function never returns an error and
/// never prevents daemon startup.
pub fn configure_from_current_exe() -> SandboxCapabilities {
    let capabilities = SandboxCapabilities::detect();
    if capabilities.backend == SandboxBackend::LinuxLandlockSeccomp {
        match std::env::current_exe() {
            Ok(current) => {
                // SAFETY: daemon startup is single-threaded at this point.
                unsafe {
                    std::env::set_var(HELPER_ENV, current);
                }
            }
            Err(error) => {
                log::warn!(
                    "sandbox: current executable unavailable; exec will use process hardening only: {error}"
                );
            }
        }
    }
    capabilities
}

/// Wrap a target command with the configured helper.
///
/// Returns the serialized request that must be written to the child's stdin.
/// `None` means no helper is configured or the active platform does not have
/// an enforcing backend. Serialization failures are returned as errors so the
/// caller can fail closed instead of silently running unsandboxed.
pub fn wrap_command(
    command: &mut Command,
    argv: &[String],
    spec: &SandboxSpec,
) -> Result<Option<Vec<u8>>, String> {
    if !spec.enabled || spec.backend != SandboxBackend::LinuxLandlockSeccomp {
        return Ok(None);
    }
    if !cfg!(target_os = "linux") {
        return Ok(None);
    }
    let Some(helper) = std::env::var_os(HELPER_ENV) else {
        return Ok(None);
    };
    if helper.is_empty() {
        return Ok(None);
    }
    let request = SandboxRequest {
        spec: spec.clone(),
        argv: argv.to_vec(),
    };
    let payload = serde_json::to_vec(&request)
        .map_err(|error| format!("serialize sandbox request: {error}"))?;

    *command = Command::new(helper);
    command.arg(HELPER_SUBCOMMAND);
    Ok(Some(payload))
}

/// Entry point for the hidden daemon subcommand.
pub fn helper_main() -> i32 {
    #[cfg(target_os = "linux")]
    {
        match linux::run_helper() {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("qaqh-sandbox: {error}");
                1
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        eprintln!("qaqh-sandbox: Linux sandbox helper is unavailable on this platform");
        1
    }
}

/// Platform-specific capability snapshot for diagnostics and policy presets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxSupport {
    pub capabilities: SandboxCapabilities,
    pub helper_path: Option<PathBuf>,
}

/// Snapshot the current process's helper configuration.
pub fn support() -> SandboxSupport {
    SandboxSupport {
        capabilities: SandboxCapabilities::detect(),
        helper_path: std::env::var_os(HELPER_ENV).map(PathBuf::from),
    }
}
