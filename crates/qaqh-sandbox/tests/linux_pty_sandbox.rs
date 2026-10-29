//! PTY × 沙箱组合集成测试（Linux 专属）。
//!
//! 验证 `prepare_pty_launch` → `qaqh-pty`（PTY 后端）→ helper
//! `__qaqh-sandbox-exec-pty` 的全链路：
//! - Landlock 写隔离照常生效；
//! - seccomp 网络拒绝照常生效；
//! - helper **不**重定向 stdin（目标进程看到的是终端）；
//! - payload 文件读取后自删。

#![cfg(target_os = "linux")]

use std::io::Read;
use std::time::{Duration, Instant};

use qaqh_pty::{spawn_pty, wait_session};
use qaqh_sandbox::{NetworkPolicy, SandboxBackend, SandboxSpec};

fn helper_env() -> Vec<(String, String)> {
    vec![
        ("PATH".into(), "/usr/bin:/bin".into()),
        ("HOME".into(), std::env::temp_dir().to_string_lossy().into_owned()),
    ]
}

fn spec(workspace: &std::path::Path) -> SandboxSpec {
    SandboxSpec {
        enabled: true,
        backend: SandboxBackend::LinuxLandlockSeccomp,
        writable_roots: vec![workspace.to_path_buf()],
        writable_files: Vec::new(),
        deny_write_paths: Vec::new(),
        network: NetworkPolicy::Deny,
        max_open_files: Some(1024),
        redirect: false,
        workspace_root: None,
    }
}

fn configure_helper_exe() {
    // SAFETY: 测试进程单线程运行测试体（workspace 强制 RUST_TEST_THREADS=1），
    // 且只写自有环境变量，无并发读取方。
    unsafe {
        std::env::set_var(
            qaqh_sandbox::HELPER_ENV,
            env!("CARGO_BIN_EXE_qaqh-sandbox-exec"),
        );
    }
}

/// 看门狗：防止测试卡死拖住单线程测试池。
struct Watchdog {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Watchdog {
    fn arm(label: &'static str, pid: u32) -> Self {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop2 = stop.clone();
        let handle = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(60);
            while Instant::now() < deadline {
                if stop2.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            // SAFETY: kill(2) 无指针参与。
            unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGKILL) };
            panic!("pty sandbox test watchdog fired: {label}");
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[test]
fn landlock_write_isolation_survives_pty_helper() {
    configure_helper_exe();
    let tmp = tempfile::tempdir().expect("tempdir");
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let inside = workspace.join("inside.txt");
    let outside = tmp.path().join("outside.txt");
    let argv = vec![
        "sh".into(),
        "-c".into(),
        "printf inside > \"$1\"; printf outside > \"$2\"".into(),
        "qaqh-pty-sandbox-test".into(),
        inside.to_string_lossy().into_owned(),
        outside.to_string_lossy().into_owned(),
    ];

    let launch = qaqh_sandbox::prepare_pty_launch(&argv, None, &spec(&workspace))
        .expect("prepare pty launch");
    assert_eq!(launch.backend, SandboxBackend::LinuxLandlockSeccomp);
    let payload_path = launch
        .payload_path
        .clone()
        .expect("helper mode must produce a payload path");
    assert!(payload_path.is_file(), "payload file must exist before spawn");

    let mut session_spec = qaqh_pty::SessionSpec::new(launch.argv);
    session_spec.env = helper_env();
    let mut session = spawn_pty(&session_spec).expect("spawn sandboxed pty");
    let watchdog = Watchdog::arm("landlock pty", session.pid);

    let mut output = String::new();
    session.reader.read_to_string(&mut output).expect("read");
    let exit = wait_session(&mut session).expect("wait");

    assert!(
        inside.is_file(),
        "workspace write should succeed: output={output}"
    );
    assert!(
        !outside.exists(),
        "outside workspace write must be denied: output={output}"
    );
    assert!(
        !exit.code.is_some_and(|code| code == 0),
        "shell must report the denied redirection: exit={exit:?} output={output}"
    );
    assert!(
        !payload_path.exists(),
        "payload file must be unlinked by the helper after read"
    );
    drop(watchdog);
}

#[test]
fn stdin_stays_a_tty_under_pty_helper() {
    configure_helper_exe();
    let tmp = tempfile::tempdir().expect("tempdir");
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let argv = vec![
        "sh".into(),
        "-c".into(),
        "test -t 0 && echo TTY-UNDER-SANDBOX".into(),
    ];

    let launch = qaqh_sandbox::prepare_pty_launch(&argv, None, &spec(&workspace))
        .expect("prepare pty launch");
    let mut session_spec = qaqh_pty::SessionSpec::new(launch.argv);
    session_spec.env = helper_env();
    let mut session = spawn_pty(&session_spec).expect("spawn sandboxed pty");
    let watchdog = Watchdog::arm("tty under sandbox", session.pid);

    let mut output = String::new();
    session.reader.read_to_string(&mut output).expect("read");
    let exit = wait_session(&mut session).expect("wait");

    assert_eq!(exit.code, Some(0), "output={output}");
    assert!(
        output.contains("TTY-UNDER-SANDBOX"),
        "stdin must remain a PTY under the helper: output={output}"
    );
    drop(watchdog);
}

#[test]
fn network_denial_survives_pty_helper() {
    configure_helper_exe();
    let tmp = tempfile::tempdir().expect("tempdir");
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let helper = env!("CARGO_BIN_EXE_qaqh-sandbox-exec");
    let argv = vec![helper.into(), "--probe-network".into()];

    let launch = qaqh_sandbox::prepare_pty_launch(&argv, None, &spec(&workspace))
        .expect("prepare pty launch");
    let mut session_spec = qaqh_pty::SessionSpec::new(launch.argv);
    session_spec.env = helper_env();
    let mut session = spawn_pty(&session_spec).expect("spawn sandboxed pty");
    let watchdog = Watchdog::arm("network probe", session.pid);

    let mut output = String::new();
    session.reader.read_to_string(&mut output).expect("read");
    let exit = wait_session(&mut session).expect("wait");

    assert_eq!(exit.code, Some(23), "network probe must exit 23: {output}");
    assert!(
        output.contains("network denied"),
        "probe must fail at socket creation: {output}"
    );
    drop(watchdog);
}

#[test]
fn prepare_pty_launch_passthrough_when_disabled() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut disabled = spec(tmp.path());
    disabled.enabled = false;
    let argv = vec!["true".into()];
    let launch =
        qaqh_sandbox::prepare_pty_launch(&argv, None, &disabled).expect("prepare disabled");
    assert_eq!(launch.backend, qaqh_sandbox::SandboxBackend::None);
    assert_eq!(launch.argv, argv);
    assert!(launch.payload_path.is_none());
}
