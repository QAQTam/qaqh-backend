//! Linux enforcement helper.
//!
//! Order matters:
//! 1. read the complete request from stdin;
//! 2. apply rlimits and Landlock;
//! 3. install seccomp;
//! 4. redirect target stdin to `/dev/null`;
//! 5. `execvp` the target.
//!
//! Any failure before `execvp` returns a non-zero helper status and the target
//! never starts.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::io::Read;
use std::os::fd::AsRawFd;

use landlock::{
    ABI, Access, AccessFs, AccessNet, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset,
    RulesetAttr, RulesetCreatedAttr, RulesetStatus,
};
use seccompiler::{
    SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter, SeccompRule,
    TargetArch,
};

use crate::{NetworkPolicy, protocol::SandboxRequest};

#[derive(Debug)]
pub enum SandboxError {
    Io(std::io::Error),
    Json(serde_json::Error),
    Unsupported(String),
    Landlock(String),
    Seccomp(String),
    Exec(String),
}

impl std::fmt::Display for SandboxError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "sandbox io error: {error}"),
            Self::Json(error) => write!(formatter, "sandbox request decode failed: {error}"),
            Self::Unsupported(message) => write!(formatter, "sandbox unsupported: {message}"),
            Self::Landlock(message) => write!(formatter, "landlock setup failed: {message}"),
            Self::Seccomp(message) => write!(formatter, "seccomp setup failed: {message}"),
            Self::Exec(message) => write!(formatter, "sandbox exec failed: {message}"),
        }
    }
}

impl std::error::Error for SandboxError {}

impl From<std::io::Error> for SandboxError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for SandboxError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

pub(crate) fn run_helper() -> Result<(), SandboxError> {
    let mut input = Vec::new();
    std::io::stdin().read_to_end(&mut input)?;
    let request: SandboxRequest = serde_json::from_slice(&input)?;
    if request.argv.is_empty() {
        return Err(SandboxError::Unsupported("empty target argv".into()));
    }
    if !request.spec.enabled {
        return Err(SandboxError::Unsupported(
            "helper invoked with disabled sandbox spec".into(),
        ));
    }

    set_resource_limits(&request)?;
    apply_landlock(&request)?;
    apply_seccomp(&request)?;
    redirect_stdin_to_dev_null()?;
    exec_target(&request.argv)
}

fn set_resource_limits(request: &SandboxRequest) -> Result<(), SandboxError> {
    let core = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: the pointer is valid for the duration of the call.
    if unsafe { libc::setrlimit(libc::RLIMIT_CORE, &core) } != 0 {
        return Err(SandboxError::Io(std::io::Error::last_os_error()));
    }

    if let Some(max_open_files) = request.spec.max_open_files {
        let nofile = libc::rlimit {
            rlim_cur: max_open_files,
            rlim_max: max_open_files,
        };
        // SAFETY: the pointer is valid for the duration of the call.
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &nofile) } != 0 {
            return Err(SandboxError::Io(std::io::Error::last_os_error()));
        }
    }
    Ok(())
}

fn apply_landlock(request: &SandboxRequest) -> Result<(), SandboxError> {
    let abi = ABI::V4;
    let fs_access = AccessFs::from_write(abi);
    let net_access = AccessNet::from_all(abi);

    let mut ruleset = Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(fs_access)
        .map_err(|error| SandboxError::Landlock(error.to_string()))?;
    if request.spec.network == NetworkPolicy::Deny {
        ruleset = ruleset
            .handle_access(net_access)
            .map_err(|error| SandboxError::Landlock(error.to_string()))?;
    }
    let mut created = ruleset
        .create()
        .map_err(|error| SandboxError::Landlock(error.to_string()))?;

    for root in &request.spec.writable_roots {
        let path = PathFd::new(root)
            .map_err(|error| SandboxError::Landlock(format!("{}: {error}", root.display())))?;
        created = created
            .add_rule(PathBeneath::new(path, fs_access))
            .map_err(|error| SandboxError::Landlock(error.to_string()))?;
    }

    let status = created
        .restrict_self()
        .map_err(|error| SandboxError::Landlock(error.to_string()))?;
    if status.ruleset != RulesetStatus::FullyEnforced {
        return Err(SandboxError::Landlock(format!(
            "requested policy was not fully enforced: {:?}",
            status.ruleset
        )));
    }
    Ok(())
}

fn apply_seccomp(request: &SandboxRequest) -> Result<(), SandboxError> {
    let mut rules = BTreeMap::new();

    for syscall in dangerous_syscalls() {
        rules.insert(syscall, Vec::new());
    }

    if request.spec.network == NetworkPolicy::Deny {
        let socket_rules = [libc::AF_INET, libc::AF_INET6, libc::AF_PACKET]
            .into_iter()
            .map(|domain| {
                SeccompRule::new(vec![
                    SeccompCondition::new(
                        0,
                        SeccompCmpArgLen::Dword,
                        SeccompCmpOp::Eq,
                        domain as u64,
                    )
                    .map_err(|error| SandboxError::Seccomp(error.to_string()))?,
                ])
                .map_err(|error| SandboxError::Seccomp(error.to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        rules.insert(libc::SYS_socket, socket_rules);
    }

    let arch = TargetArch::try_from(std::env::consts::ARCH)
        .map_err(|error| SandboxError::Seccomp(error.to_string()))?;
    let filter: seccompiler::BpfProgram = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM as u32),
        arch,
    )
    .map_err(|error| SandboxError::Seccomp(error.to_string()))?
    .try_into()
    .map_err(|error: seccompiler::BackendError| SandboxError::Seccomp(error.to_string()))?;

    seccompiler::apply_filter(&filter).map_err(|error| SandboxError::Seccomp(error.to_string()))
}

fn dangerous_syscalls() -> Vec<i64> {
    vec![
        libc::SYS_bpf,
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_register,
        libc::SYS_kexec_load,
        libc::SYS_mount,
        libc::SYS_open_by_handle_at,
        libc::SYS_perf_event_open,
        libc::SYS_pivot_root,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
        libc::SYS_ptrace,
        libc::SYS_reboot,
        libc::SYS_setns,
        libc::SYS_umount2,
        libc::SYS_unshare,
        libc::SYS_userfaultfd,
    ]
}

fn redirect_stdin_to_dev_null() -> Result<(), SandboxError> {
    let null = std::fs::OpenOptions::new().read(true).open("/dev/null")?;
    // SAFETY: dup2 receives valid file descriptors and does not retain pointers.
    if unsafe { libc::dup2(null.as_raw_fd(), libc::STDIN_FILENO) } < 0 {
        return Err(SandboxError::Io(std::io::Error::last_os_error()));
    }
    Ok(())
}

fn exec_target(argv: &[String]) -> Result<(), SandboxError> {
    let program =
        CString::new(argv[0].as_bytes()).map_err(|error| SandboxError::Exec(error.to_string()))?;
    let args = argv
        .iter()
        .map(|arg| {
            CString::new(arg.as_bytes()).map_err(|error| SandboxError::Exec(error.to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut raw_args: Vec<*const libc::c_char> = args.iter().map(|arg| arg.as_ptr()).collect();
    raw_args.push(std::ptr::null());

    // SAFETY: `program` and every `raw_args` pointer remain valid until execvp
    // returns. On success this function does not return.
    unsafe {
        libc::execvp(program.as_ptr(), raw_args.as_ptr());
    }
    Err(SandboxError::Exec(
        std::io::Error::last_os_error().to_string(),
    ))
}
