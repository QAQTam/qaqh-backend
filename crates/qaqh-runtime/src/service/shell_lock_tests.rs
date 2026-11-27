//! L-11 反证：shell 写入不得等待中央 `AgentRegistry` 锁。

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::*;

const OWNER: &str = "device-shell-lock-regression";
const FAST_PATH_MAX: Duration = Duration::from_millis(10);

fn isolated_service() -> (tempfile::TempDir, QaqhService) {
    let dir = tempfile::tempdir().expect("isolated data root");
    let owners = Arc::new(
        qaqh_session::owner::OwnerDirectory::open(dir.path().join("qaqh.db"))
            .expect("open owners db"),
    );
    let registry = AgentRegistry::new(Arc::clone(&owners));
    let service = QaqhService {
        registry: Arc::new(Mutex::new(registry)),
        shells: Arc::new(crate::shell::ShellRegistry::new()),
        hub: std::sync::OnceLock::new(),
        owners,
        siwc: std::sync::OnceLock::new(),
        dsaccount: std::sync::OnceLock::new(),
    };
    (dir, service)
}

#[test]
fn shell_write_remains_responsive_while_registry_lock_is_held() {
    let (_dir, service) = isolated_service();
    let shell = service
        .shell_open(
            OWNER,
            &crate::ringing::service_methods::ShellOpenParams::default(),
        )
        .expect("open shell before taking the registry lock");

    // 旧实现先取 registry guard 再写 stdin，因此写入线程会一直卡到 guard 释放。
    // 用有界 channel 等回执，让旧实现响亮超时而不是把测试进程永久挂住。
    let registry = service.registry.lock().expect("hold registry lock");
    let writer_service = service.clone();
    let shell_id = shell.id.clone();
    let (tx, rx) = mpsc::channel();
    let writer = std::thread::spawn(move || {
        let started = Instant::now();
        let result = writer_service.shell_stdin(OWNER, &shell_id, b"x");
        tx.send((started.elapsed(), result))
            .expect("shell write result channel");
    });

    let received = rx.recv_timeout(Duration::from_secs(2));
    drop(registry);
    let (elapsed, result) = match received {
        Ok(outcome) => outcome,
        Err(_) => {
            writer
                .join()
                .expect("blocked shell writer must finish after unlock");
            service.shutdown_shells();
            panic!("L-11 回归：registry 锁被持有时 shell write 未能及时返回");
        }
    };
    writer.join().expect("shell writer thread panicked");
    service.shutdown_shells();
    result.expect("shell write succeeds after the registry lock is released");
    assert!(
        elapsed < FAST_PATH_MAX,
        "L-11 回归：shell write 等待 registry 锁，耗时 {elapsed:?}，上限 {FAST_PATH_MAX:?}"
    );
}
