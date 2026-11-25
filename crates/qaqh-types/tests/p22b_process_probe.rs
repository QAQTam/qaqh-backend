//! v4.5 P2-2b 反证（plan §三.5）：`process_is_running` 必须是**零子进程派生**
//! 的原生探测。
//!
//! 红→绿语义：
//! - 基线（Windows 派生 `tasklist`、Unix 派生 `kill -0` 命令）下 100 次均值是
//!   毫秒级（`kill` fork+exec ≈0.5–2ms，`tasklist` ≈150ms），性能断言必红；
//! - 换 `OpenProcess`+`GetExitCodeProcess` / `libc::kill(pid,0)` 后单次 ≈1µs，绿。
//!
//! Windows 分支在 Linux 宿主只能实测 Unix 路径；`cfg(windows)` 面以
//! `x86_64-pc-windows-msvc` 交叉 `cargo check` 自证（见 PR 四联单未验面）。
use std::process::{Child, Command, Stdio};
use std::time::Instant;

use qaqh_types::platform::process_is_running;

/// 一个存活约一秒的子进程：判活必须 `true`，`wait` 回收后必须 `false`。
fn spawn_sleeper() -> Child {
    #[cfg(windows)]
    let mut command = {
        let mut c = Command::new("cmd");
        c.args(["/C", "ping", "-n", "2", "127.0.0.1", ">nul"]);
        c
    };
    #[cfg(not(windows))]
    let mut command = {
        let mut c = Command::new("/bin/sh");
        c.args(["-c", "sleep 1"]);
        c
    };
    command
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn sleeper child")
}

#[test]
fn self_process_is_running() {
    assert!(
        process_is_running(std::process::id()),
        "当前进程必须判活；判死说明探测实现失效"
    );
}

#[test]
fn pid_zero_and_out_of_range_are_false() {
    assert!(!process_is_running(0), "pid 0 恒非普通进程");
    // 超出 pid_t 值域（i32::MAX）：fail-closed 判死，绝不回退子进程探测。
    assert!(!process_is_running(u32::MAX));
    assert!(!process_is_running(i32::MAX as u32 + 7));
}

#[test]
fn live_child_true_then_reaped_child_false() {
    let mut child = spawn_sleeper();
    assert!(process_is_running(child.id()), "存活子进程必须判活");
    let _ = child.wait();
    assert!(
        !process_is_running(child.id()),
        "已 wait 回收的子进程必须判死"
    );
}

#[test]
fn probe_is_subprocess_free_fast() {
    let pid = std::process::id();
    // 预热一次（首调用可能有页错误/符号解析噪声）。
    assert!(process_is_running(pid));
    let start = Instant::now();
    let rounds = 100u32;
    for _ in 0..rounds {
        assert!(process_is_running(pid));
    }
    let mean_micros = start.elapsed().as_nanos() as f64 / 1000.0 / rounds as f64;
    assert!(
        mean_micros < 50.0,
        "process_is_running 100 次均值 {mean_micros:.1}µs ≥ 50µs：疑似仍派生子进程（kill/tasklist fork+exec 毫秒级）"
    );
}
