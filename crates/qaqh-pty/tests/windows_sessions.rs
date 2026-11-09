//! PTY 会话冒烟测试（Windows ConPTY）。
//!
//! 回归锚（两代缺陷各有一条断言）：
//! 1. spawn 后 master 即 drop → ClosePseudoConsole → conhost 关闭 → 子进程
//!    被终止、输出丢失（"tty=true 无输出 + exit -1" 的根因）；
//! 2. 反向踩坑：master 保留但收割后不关终端 → ConPTY 输出管道写端由 master
//!    侧持有，子进程退出**不会**产生 EOF，读线程被永久钉死。
//! 契约：master 存活期间输出完整捕获；子进程退出后 EOF 不会自行到来；
//! close_terminal 后 EOF 确定性到达并携带全部输出。

#![cfg(windows)]

use std::io::Read;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use qaqh_pty::{SessionSpec, spawn_pty};

fn minimal_env() -> Vec<(String, String)> {
    vec![
        (
            "SystemRoot".into(),
            std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into()),
        ),
        ("PATH".into(), std::env::var("PATH").unwrap_or_default()),
    ]
}

#[test]
fn conpty_output_is_captured_and_eof_arrives_on_terminal_close() {
    let mut spec = SessionSpec::new(vec![
        "cmd.exe".into(),
        "/c".into(),
        "echo".into(),
        "pty-windows-ok".into(),
    ]);
    spec.env = minimal_env();
    let session = spawn_pty(&spec).expect("spawn ConPTY session");
    let (mut io, mut handle) = session.into_parts();

    let captured: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_reader = Arc::clone(&captured);
    let (eof_tx, eof_rx) = mpsc::channel::<()>();
    let reader = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match io.reader.read(&mut buf) {
                Ok(0) => break,
                Ok(count) => captured_reader
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .extend_from_slice(&buf[..count]),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => panic!("ConPTY reader failed: {error}"),
            }
        }
        let _ = eof_tx.send(());
    });

    // 等待子进程退出（收割）。
    let deadline = Instant::now() + Duration::from_secs(10);
    let exit = loop {
        match handle.try_wait().expect("try_wait") {
            Some(exit) => break exit,
            None => {
                if Instant::now() >= deadline {
                    let seen = captured.lock().unwrap_or_else(|error| error.into_inner());
                    panic!(
                        "child did not exit in time; ConPTY output so far: {:?}",
                        String::from_utf8_lossy(&seen)
                    );
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    };

    // ConPTY 契约：写端由 master 侧持有，退出后 EOF 不会自行到来。
    // （master 被提前 drop 的旧缺陷在这里表现为"读空即 EOF"。）
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        eof_rx.try_recv().is_err(),
        "ConPTY output pipe must stay open while the master is alive"
    );

    // 收割后关终端 → EOF 确定性到达，且携带完整输出。
    handle.close_terminal();
    eof_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("EOF after close_terminal");
    reader.join().expect("reader thread");
    let bytes = captured.lock().unwrap_or_else(|error| error.into_inner());
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        text.contains("pty-windows-ok"),
        "ConPTY output lost: {text:?}"
    );
    assert_eq!(exit.code, Some(0), "cmd /c echo must exit cleanly");
}
