//! PTY 会话 × ProcessRegistry 生命周期集成测试（unix）。
//!
//! 验证 `attach_pty_session` 后注册表的既有原语对 PTY 会话成立：
//! `list_sessions` 可见、`kill` 经 os_pid（会话 leader）覆盖整个会话组。

#![cfg(unix)]

use std::time::Duration;

use qaqh_process_tools::process_registry::{KillOutcome, ProcessRegistry};
use qaqh_pty::{SessionSpec, spawn_pty};

struct Watchdog {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Watchdog {
    fn arm(label: &'static str, pid: u32) -> Self {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop2 = stop.clone();
        let handle = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(60);
            while std::time::Instant::now() < deadline {
                if stop2.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            // SAFETY: kill(2) 无指针参与。
            unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGKILL) };
            panic!("registry pty test watchdog fired: {label}");
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
    vec![("PATH".into(), "/usr/bin:/bin".into())]
}

#[test]
fn registry_kill_covers_a_registered_pty_session() {
    let mut spec = SessionSpec::new(vec!["sleep".into(), "30".into()]);
    spec.env = minimal_env();
    let session = spawn_pty(&spec).expect("spawn pty");
    let pid = session.pid;
    let (io, mut handle) = session.into_parts();
    let watchdog = Watchdog::arm("registry kill", pid);

    let proc_id = ProcessRegistry::register("pty-lifecycle-test");
    ProcessRegistry::attach_pty_session(proc_id, pid, io.writer);

    // 前端列表可见（list_sessions 含在册 PTY 会话）。
    let listed = ProcessRegistry::list_sessions();
    assert!(
        listed.iter().any(|info| info.get("id") == Some(&serde_json::json!(proc_id))),
        "PTY 会话必须出现在 list_sessions: {listed:?}"
    );

    // kill：注册表按 os_pid killpg（session leader ⇒ PID == PGID）。
    assert_eq!(ProcessRegistry::kill(proc_id), KillOutcome::Killed);

    // 组内进程确认死亡：句柄收割为信号终止（code None）。
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut observed = None;
    while std::time::Instant::now() < deadline {
        if let Ok(Some(exit)) = handle.try_wait() {
            observed = Some(exit);
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let exit = observed.expect("被 kill 的 PTY 会话必须可收割");
    assert_eq!(exit.code, None, "SIGKILL → code None: {exit:?}");
    drop(watchdog);
}

#[test]
fn registry_kill_unknown_id_is_not_found() {
    // 1_000..100_000 随机 id 空间；用 999_999 保证未注册。
    assert_eq!(ProcessRegistry::kill(999_999), KillOutcome::NotFound);
}
