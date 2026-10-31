//! Linux audit probe using the CURRENT registry source directly, without a Cargo rebuild.
//! The only replaced dependency is the ambient cancellation fixture below.
#![allow(dead_code)]

#[path = "../../crates/qaqh-process-tools/src/process_registry.rs"]
mod process_registry;

#[path = "../../crates/qaqh-session/src/canonical/notify.rs"]
mod commit_notify;

use process_registry::ProcessRegistry;
use std::{
    io::{BufRead, Write},
    os::unix::process::CommandExt,
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

static CANCELLED: AtomicBool = AtomicBool::new(false);
fn is_cancel() -> bool {
    CANCELLED.load(Ordering::Acquire)
}

struct BackpressuredWriter {
    ready: Option<mpsc::Sender<()>>,
    release: Arc<AtomicBool>,
}

impl Write for BackpressuredWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if let Some(ready) = self.ready.take() {
            ready.send(()).expect("announce write");
        }
        if self.release.load(Ordering::Acquire) {
            Ok(bytes.len())
        } else {
            Err(std::io::ErrorKind::WouldBlock.into())
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn write_exits_on_cancel() {
    let id = ProcessRegistry::register("audit-write");
    let release = Arc::new(AtomicBool::new(false));
    let cancel = Arc::new(AtomicBool::new(false));
    let (ready, entered) = mpsc::channel();
    ProcessRegistry::attach_pty_session(
        id,
        i32::MAX as u32,
        Box::new(BackpressuredWriter {
            ready: Some(ready),
            release: Arc::clone(&release),
        }),
    );
    let (result, observed) = mpsc::channel();
    let writer_cancel = Arc::clone(&cancel);
    let writer = std::thread::spawn(move || {
        result
            .send(ProcessRegistry::write_to(
                id,
                "x",
                Some(&writer_cancel),
                Duration::from_secs(2),
            ))
            .expect("write result");
    });
    entered
        .recv_timeout(Duration::from_secs(1))
        .expect("writer started");
    cancel.store(true, Ordering::Release);
    let result = observed
        .recv_timeout(Duration::from_millis(150))
        .expect("cancelled write returns promptly");
    writer.join().expect("join writer");
    assert!(
        result
            .expect_err("backpressured write must cancel")
            .to_string()
            .contains("cancelled")
    );
    release.store(true, Ordering::Release);
    println!("FIXED write_to: cancellation released a backpressured write promptly");
}

fn kill_wait_holds_global_registry() {
    // Start in its own group, exactly as production does. The owned helper then
    // rejoins the parent's group: the original group disappears while the child lives.
    // SAFETY: getpgrp only reads this process's group identity.
    let parent_group = unsafe { libc::getpgrp() };
    let mut child = Command::new(std::env::current_exe().expect("probe executable"))
        .args(["--owned-child", &parent_group.to_string()])
        .process_group(0)
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn owned helper");
    let pid = child.id();
    let mut line = String::new();
    std::io::BufReader::new(child.stdout.take().expect("helper stdout"))
        .read_line(&mut line)
        .expect("helper ready");
    assert_eq!(line.trim(), "READY");
    // SAFETY: signal 0 does not send a signal; pid belongs to our live helper.
    assert_eq!(unsafe { libc::killpg(pid as i32, 0) }, -1);
    let id = ProcessRegistry::register("audit-kill");
    ProcessRegistry::attach_child(id, child);
    let (enter, entered) = mpsc::channel();
    let killer = std::thread::spawn(move || {
        enter.send(()).expect("announce kill");
        ProcessRegistry::kill(id)
    });
    entered.recv().expect("kill entered");
    std::thread::sleep(Duration::from_millis(50));
    let (report, observed) = mpsc::channel();
    let lookup = std::thread::spawn(move || {
        let start = Instant::now();
        let _ = ProcessRegistry::get_info(u32::MAX);
        report.send(start.elapsed()).expect("report lookup");
    });
    let lookup_result = observed.recv_timeout(Duration::from_millis(150));
    lookup.join().expect("lookup");
    let result = killer.join().expect("kill");
    let elapsed = lookup_result.expect("unrelated lookup stays responsive");
    println!(
        "FIXED kill: unrelated registry lookup completed in {elapsed:?}; kill returned {result:?}"
    );
}

fn commit_between_baseline_and_generation_is_missed() {
    let dir = std::env::temp_dir().join(format!("qaqh-wait-audit-{}", std::process::id()));
    let before_baseline = commit_notify::commit_generation(&dir);
    // Exact ordering used in wait_agent: read the baseline, a commit races it,
    // then capture generation and wait. The fixture baseline has no IO.
    commit_notify::notify_commit(&dir);
    let after_baseline = commit_notify::commit_generation(&dir);
    let start = Instant::now();
    let changed = commit_notify::wait_for_commit_change(
        &dir,
        after_baseline,
        start + Duration::from_millis(150),
    );
    assert!(!changed, "generation captured after racing commit hides it");
    // Capturing generation before the baseline does observe the raced commit.
    assert!(commit_notify::wait_for_commit_change(
        &dir,
        before_baseline,
        Instant::now() + Duration::from_millis(150),
    ));
    println!(
        "REPRODUCED wait_agent ordering: raced commit missed until {:?} timeout",
        start.elapsed()
    );
}

fn cancellation_alone_cannot_wake_commit_wait() {
    let dir = std::env::temp_dir().join(format!("qaqh-cancel-wait-audit-{}", std::process::id()));
    let observed = commit_notify::commit_generation(&dir);
    let cancel = Arc::new(AtomicBool::new(false));
    let reader_cancel = Arc::clone(&cancel);
    let wait_dir = dir.clone();
    let (entered, ready) = mpsc::channel();
    let (finished, result) = mpsc::channel();
    let waiter = std::thread::spawn(move || {
        assert!(!reader_cancel.load(Ordering::Acquire));
        entered.send(()).expect("enter commit wait");
        let changed = commit_notify::wait_for_commit_change(
            &wait_dir,
            observed,
            Instant::now() + Duration::from_secs(2),
        );
        finished.send(changed).expect("wait result");
    });
    ready.recv().expect("waiter ready");
    cancel.store(true, Ordering::Release);
    let blocked = result.recv_timeout(Duration::from_millis(150)).is_err();
    // Wake the fixture explicitly so the audit leaves no blocked thread.
    commit_notify::notify_commit(&dir);
    assert!(
        result
            .recv_timeout(Duration::from_secs(1))
            .expect("released waiter")
    );
    waiter.join().expect("waiter");
    assert!(
        blocked,
        "an atomic cancellation flag cannot notify this condvar"
    );
    println!("REPRODUCED wait_agent primitive: cancellation alone does not wake commit wait");
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("--owned-child") {
        let group: i32 = std::env::args()
            .nth(2)
            .expect("parent group")
            .parse()
            .expect("group");
        // SAFETY: only this owned helper changes its group inside the same session.
        assert_eq!(unsafe { libc::setpgid(0, group) }, 0);
        println!("READY");
        std::io::stdout().flush().expect("flush ready");
        std::thread::sleep(Duration::from_secs(3));
        return;
    }
    write_exits_on_cancel();
    kill_wait_holds_global_registry();
    commit_between_baseline_and_generation_is_missed();
    cancellation_alone_cannot_wake_commit_wait();
}
