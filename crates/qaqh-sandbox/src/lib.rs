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

/// Result of applying a sandbox policy to a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxLaunch {
    /// Backend actually selected after capability resolution.
    pub backend: SandboxBackend,
    /// Helper request to write to child stdin; `None` for direct launchers
    /// such as bubblewrap.
    pub request: Option<Vec<u8>>,
}

/// Configure the current daemon binary as the Linux helper when the kernel
/// supports the required primitives. This function never returns an error and
/// never prevents daemon startup.
pub fn configure_from_current_exe() -> SandboxCapabilities {
    let capabilities = SandboxCapabilities::detect();
    if capabilities.landlock && capabilities.seccomp {
        match std::env::current_exe() {
            Ok(current) => {
                // SAFETY: daemon startup is single-threaded at this point.
                unsafe {
                    std::env::set_var(HELPER_ENV, current);
                }
            }
            Err(error) => {
                log::warn!(
                    "sandbox: current executable unavailable; Landlock fallback helper disabled: {error}"
                );
            }
        }
    }
    capabilities
}

/// Wrap a target command with the strongest configured backend.
///
/// `Auto` prefers bubblewrap, then the Landlock/seccomp helper. Explicitly
/// requested backends fail closed when their required primitive is absent.
pub fn wrap_command(
    command: &mut Command,
    argv: &[String],
    cwd: Option<&str>,
    spec: &SandboxSpec,
) -> Result<SandboxLaunch, String> {
    if !spec.enabled {
        return Ok(SandboxLaunch {
            backend: SandboxBackend::None,
            request: None,
        });
    }
    if !cfg!(target_os = "linux") {
        return Ok(SandboxLaunch {
            backend: SandboxBackend::None,
            request: None,
        });
    }

    let capabilities = SandboxCapabilities::detect();
    let resolved_spec = canonicalize_spec(spec)?;
    let backend = resolve_backend(spec.backend, &capabilities)?;
    log::debug!(
        target: "qaqh_sandbox",
        "{}",
        serde_json::json!({
            "event": "sandbox_backend_selected",
            "requested": spec.backend,
            "resolved": backend,
            "bubblewrap": capabilities.bubblewrap,
            "landlock": capabilities.landlock,
            "seccomp": capabilities.seccomp,
            "network": spec.network,
            "writable_roots": resolved_spec.writable_roots,
        })
    );

    match backend {
        SandboxBackend::LinuxBubblewrap => {
            configure_bubblewrap(command, argv, cwd, &resolved_spec)?;
            Ok(SandboxLaunch {
                backend,
                request: None,
            })
        }
        SandboxBackend::LinuxLandlockSeccomp => {
            let Some(helper) = std::env::var_os(HELPER_ENV) else {
                if spec.backend == SandboxBackend::Auto {
                    return Ok(SandboxLaunch {
                        backend: SandboxBackend::None,
                        request: None,
                    });
                }
                return Err("Landlock/seccomp helper is not configured".into());
            };
            if helper.is_empty() {
                return Err("Landlock/seccomp helper path is empty".into());
            }
            let request = SandboxRequest {
                spec: resolved_spec,
                argv: argv.to_vec(),
            };
            let payload = serde_json::to_vec(&request)
                .map_err(|error| format!("serialize sandbox request: {error}"))?;
            *command = Command::new(helper);
            command.arg(HELPER_SUBCOMMAND);
            Ok(SandboxLaunch {
                backend,
                request: Some(payload),
            })
        }
        SandboxBackend::ProcessHardening | SandboxBackend::None => Ok(SandboxLaunch {
            backend,
            request: None,
        }),
        SandboxBackend::Auto => unreachable!("resolve_backend removes Auto"),
    }
}

fn canonicalize_spec(spec: &SandboxSpec) -> Result<SandboxSpec, String> {
    let mut resolved = spec.clone();
    resolved.writable_roots = spec
        .writable_roots
        .iter()
        .map(|root| {
            std::fs::canonicalize(root).map_err(|error| {
                format!(
                    "canonicalize sandbox writable root {}: {error}",
                    root.display()
                )
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(resolved)
}

fn resolve_backend(
    requested: SandboxBackend,
    capabilities: &SandboxCapabilities,
) -> Result<SandboxBackend, String> {
    match requested {
        SandboxBackend::Auto => {
            if capabilities.bubblewrap {
                Ok(SandboxBackend::LinuxBubblewrap)
            } else if capabilities.landlock && capabilities.seccomp {
                Ok(SandboxBackend::LinuxLandlockSeccomp)
            } else {
                Ok(SandboxBackend::ProcessHardening)
            }
        }
        SandboxBackend::LinuxBubblewrap if capabilities.bubblewrap => {
            Ok(SandboxBackend::LinuxBubblewrap)
        }
        SandboxBackend::LinuxBubblewrap => Err(format!(
            "bubblewrap requested but unavailable: {}",
            capabilities.detail
        )),
        SandboxBackend::LinuxLandlockSeccomp if capabilities.landlock && capabilities.seccomp => {
            Ok(SandboxBackend::LinuxLandlockSeccomp)
        }
        SandboxBackend::LinuxLandlockSeccomp => Err(format!(
            "Landlock/seccomp requested but unavailable: {}",
            capabilities.detail
        )),
        other => Ok(other),
    }
}

fn configure_bubblewrap(
    command: &mut Command,
    argv: &[String],
    cwd: Option<&str>,
    spec: &SandboxSpec,
) -> Result<(), String> {
    if argv.is_empty() {
        return Err("bubblewrap target argv is empty".into());
    }
    let mut bwrap = Command::new("bwrap");
    bwrap.args(["--die-with-parent", "--unshare-all"]);
    if spec.network == NetworkPolicy::Allow {
        bwrap.arg("--share-net");
    }
    bwrap.args(["--ro-bind", "/", "/"]);
    for root in &spec.writable_roots {
        let root = root.to_string_lossy();
        bwrap.arg("--bind").arg(root.as_ref()).arg(root.as_ref());
    }
    bwrap.args(["--dev", "/dev", "--proc", "/proc"]);
    if let Some(cwd) = cwd {
        bwrap.arg("--chdir").arg(cwd);
    }
    bwrap.arg("--").args(argv);
    *command = bwrap;
    Ok(())
}

/// Normalized reason for a filesystem sandbox denial.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxDenialReason {
    PermissionDenied,
    OperationNotPermitted,
    ReadOnlyFileSystem,
    PolicyDenied,
}

impl SandboxDenialReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PermissionDenied => "permission_denied",
            Self::OperationNotPermitted => "operation_not_permitted",
            Self::ReadOnlyFileSystem => "read_only_file_system",
            Self::PolicyDenied => "policy_denied",
        }
    }
}

/// Structured denial event derived from a sandboxed command result.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SandboxDenial {
    pub backend: SandboxBackend,
    pub reason: SandboxDenialReason,
    pub exit_code: Option<i32>,
    pub output_snippet: String,
}

/// Classify a sandboxed command result without claiming every failure is a
/// sandbox denial. This intentionally mirrors Codex's conservative keyword
/// heuristic: quick shell reject codes are not treated as policy denials.
pub fn classify_denial(
    backend: SandboxBackend,
    exit_code: Option<i32>,
    output: &str,
) -> Option<SandboxDenial> {
    if !matches!(
        backend,
        SandboxBackend::LinuxBubblewrap | SandboxBackend::LinuxLandlockSeccomp
    ) {
        return None;
    }
    if exit_code == Some(0) || matches!(exit_code, Some(2 | 126 | 127)) {
        return None;
    }

    let lower = output.to_ascii_lowercase();
    let reason = if lower.contains("read-only file system") {
        SandboxDenialReason::ReadOnlyFileSystem
    } else if lower.contains("operation not permitted") {
        SandboxDenialReason::OperationNotPermitted
    } else if lower.contains("permission denied") {
        SandboxDenialReason::PermissionDenied
    } else if lower.contains("seccomp") || lower.contains("landlock") || lower.contains("sandbox") {
        SandboxDenialReason::PolicyDenied
    } else {
        return None;
    };

    let output_snippet = output.chars().take(512).collect();
    Some(SandboxDenial {
        backend,
        reason,
        exit_code,
        output_snippet,
    })
}

/// Emit a structured denial event when a sandboxed command failed for a
/// recognized policy reason.
pub fn record_denial_if_any(
    backend: SandboxBackend,
    exit_code: Option<i32>,
    output: &str,
    tool_call_id: &str,
    command: &str,
) {
    let Some(denial) = classify_denial(backend, exit_code, output) else {
        return;
    };
    log::warn!(
        target: "qaqh_sandbox",
        "{}",
        serde_json::json!({
            "event": "sandbox_denial",
            "backend": denial.backend,
            "reason": denial.reason.as_str(),
            "exit_code": denial.exit_code,
            "tool_call_id": tool_call_id,
            "command": command,
            "output_snippet": denial.output_snippet,
        })
    );
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_prefers_bubblewrap_then_landlock() {
        let bwrap = SandboxCapabilities {
            platform: Platform::Linux,
            backend: SandboxBackend::LinuxBubblewrap,
            landlock: true,
            seccomp: true,
            bubblewrap: true,
            filesystem_write_isolation: true,
            network_isolation: true,
            process_hardening: true,
            detail: "test".into(),
        };
        assert_eq!(
            resolve_backend(SandboxBackend::Auto, &bwrap).expect("bwrap"),
            SandboxBackend::LinuxBubblewrap
        );

        let landlock = SandboxCapabilities {
            bubblewrap: false,
            backend: SandboxBackend::LinuxLandlockSeccomp,
            ..bwrap
        };
        assert_eq!(
            resolve_backend(SandboxBackend::Auto, &landlock).expect("landlock"),
            SandboxBackend::LinuxLandlockSeccomp
        );
    }

    #[test]
    fn explicit_missing_backend_fails_closed() {
        let capabilities = SandboxCapabilities {
            platform: Platform::Linux,
            backend: SandboxBackend::ProcessHardening,
            landlock: false,
            seccomp: false,
            bubblewrap: false,
            filesystem_write_isolation: false,
            network_isolation: false,
            process_hardening: true,
            detail: "test".into(),
        };
        assert!(resolve_backend(SandboxBackend::LinuxBubblewrap, &capabilities).is_err());
        assert!(resolve_backend(SandboxBackend::LinuxLandlockSeccomp, &capabilities).is_err());
    }

    #[test]
    fn denial_classifier_recognizes_read_only_filesystem() {
        let denial = classify_denial(
            SandboxBackend::LinuxBubblewrap,
            Some(1),
            "sh: /tmp/out: Read-only file system",
        )
        .expect("denial");
        assert_eq!(denial.reason, SandboxDenialReason::ReadOnlyFileSystem);
        assert_eq!(denial.backend, SandboxBackend::LinuxBubblewrap);
    }
}
