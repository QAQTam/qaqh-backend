//! qaqh-pty Unix 会话集成测试：PTY/管道双后端、会话 leader、stdin 续写、
//! 组级终止。全部 `#[cfg(unix)]`；Windows 由 P3 的 ConPTY 路径另行覆盖。

#![cfg(unix)]

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use qaqh_pty::{SessionKind, SessionSpec, kill_session_group, spawn_pty, spawn_pipe, wait_session};

/// 看门狗：任何测试卡死时按组杀掉 `pid` 并让测试失败，避免单线程测试池
/// （workspace `RUST_TEST_THREADS=1`）整体挂起。
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
            // SAFETY: kill(2) 无指针参与；负数 pgid 按进程组投递。
            unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGKILL) };
            panic!("qaqh-pty test watchdog fired: {label}");
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

fn minimal_env() -> Vec<(String, String)> {
    vec![
        ("PATH".into(), "/usr/bin:/bin".into()),
        ("HOME".into(), std::env::temp_dir().to_string_lossy().into_owned()),
    ]
}

#[test]
fn pipe_backend_captures_output_and_exit_code() {
    let mut spec = SessionSpec::new(vec![
        "sh".into(),
        "-c".into(),
        "echo pipe-out; echo pipe-err >&2".into(),
    ]);
    spec.env = minimal_env();
    let mut session = spawn_pipe(&spec).expect("spawn pipe");

    assert_eq!(session.kind, SessionKind::Pipe);
    let mut output = String::new();
    session
        .reader
        .read_to_string(&mut output)
        .expect("read merged output");
    let exit = wait_session(&mut session).expect("wait");
    assert_eq!(exit.code, Some(0));
    assert!(output.contains("pipe-out"), "merged output: {output}");
    assert!(output.contains("pipe-err"), "stderr must merge in: {output}");
}

#[test]
fn pty_backend_allocates_a_tty() {
    let mut spec = SessionSpec::new(vec![
        "sh".into(),
        "-c".into(),
        "test -t 0 && test -t 1 && echo TTY-OK".into(),
    ]);
    spec.env = minimal_env();
    let mut session = spawn_pty(&spec).expect("spawn pty");
    let watchdog = Watchdog::arm("pty tty detection", session.pid);

    assert_eq!(session.kind, SessionKind::Pty);
    let mut output = String::new();
    session.reader.read_to_string(&mut output).expect("read pty");
    let exit = wait_session(&mut session).expect("wait");
    assert_eq!(exit.code, Some(0));
    assert!(output.contains("TTY-OK"), "pty output: {output}");
    drop(watchdog);
}

#[test]
fn pty_child_is_session_leader() {
    let mut spec = SessionSpec::new(vec!["sleep".into(), "5".into()]);
    spec.env = minimal_env();
    let mut session = spawn_pty(&spec).expect("spawn pty");
    let watchdog = Watchdog::arm("session leader", session.pid);

    // portable-pty 在 Unix 上以 setsid 建立 PTY 子进程 → PID == PGID。
    // SAFETY: getpgid(2) 只读取进程表，无指针参与。
    let pgid = unsafe { libc::getpgid(session.pid as libc::pid_t) };
    assert_eq!(pgid, session.pid as libc::pid_t, "PTY child must be a session leader");

    kill_session_group(&mut session).expect("kill group");
    let exit = wait_session(&mut session).expect("wait after kill");
    assert_eq!(exit.code, None, "SIGKILL 的目标 code 应为 None: {exit:?}");
    drop(watchdog);
}

#[test]
fn pty_stdin_roundtrip_with_eot() {
    let mut spec = SessionSpec::new(vec!["cat".into()]);
    spec.env = minimal_env();
    let mut session = spawn_pty(&spec).expect("spawn pty");
    let watchdog = Watchdog::arm("stdin roundtrip", session.pid);

    session
        .writer
        .write_all(b"hello-pty\n")
        .expect("write stdin");
    // Ctrl-D（EOT）：规范模式下终结当前行并触发 EOF。
    session.writer.write_all(b"\x04").expect("write EOT");
    session.writer.flush().expect("flush");

    let mut output = String::new();
    session.reader.read_to_string(&mut output).expect("read until EOF");
    let exit = wait_session(&mut session).expect("wait");
    assert_eq!(exit.code, Some(0));
    assert!(output.contains("hello-pty"), "echoed output: {output}");
    drop(watchdog);
}

#[test]
fn pipe_kill_group_terminates_descendants() {
    let mut spec = SessionSpec::new(vec![
        "sh".into(),
        "-c".into(),
        "sleep 30 & wait".into(),
    ]);
    spec.env = minimal_env();
    let mut session = spawn_pipe(&spec).expect("spawn pipe");
    let watchdog = Watchdog::arm("kill group", session.pid);

    std::thread::sleep(Duration::from_millis(200));
    kill_session_group(&mut session).expect("kill group");
    let exit = wait_session(&mut session).expect("wait after kill");
    assert_eq!(exit.code, None, "组杀后应为信号终止: {exit:?}");
    drop(watchdog);
}

#[test]
fn pty_env_and_cwd_are_applied() {
    let mut spec = SessionSpec::new(vec![
        "sh".into(),
        "-c".into(),
        "pwd; echo \"ENV=$QAQH_PTY_PROBE\"; echo \"PATH_SET=${PATH:+yes}\"".into(),
    ]);
    spec.env = vec![
        ("PATH".into(), "/usr/bin:/bin".into()),
        ("QAQH_PTY_PROBE".into(), "applied".into()),
    ];
    spec.cwd = Some(std::env::temp_dir());
    let mut session = spawn_pty(&spec).expect("spawn pty");
    let watchdog = Watchdog::arm("env/cwd", session.pid);

    let mut output = String::new();
    session.reader.read_to_string(&mut output).expect("read");
    let exit = wait_session(&mut session).expect("wait");
    assert_eq!(exit.code, Some(0));
    assert!(output.contains("ENV=applied"), "env: {output}");
    assert!(output.contains("PATH_SET=yes"), "path: {output}");
    drop(watchdog);
}
