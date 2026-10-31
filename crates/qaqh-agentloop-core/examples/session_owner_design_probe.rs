//! Isolated design probe; this does not implement or replace the production loop.
//! Run: cargo run -p qaqh-agentloop-core --example session_owner_design_probe

use std::{
    collections::HashMap,
    future::Future,
    io::Write,
    pin::pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll, Wake, Waker},
    time::{SystemTime, UNIX_EPOCH},
};

use qaqh_agentloop_core::{ActorIdentity, EffectIdentity};
use tokio::{
    sync::{Semaphore, mpsc, oneshot, watch},
    task::JoinSet,
};

fn identity(effect: &str) -> EffectIdentity {
    EffectIdentity {
        actor: ActorIdentity {
            session_id: "probe-session".into(),
            incarnation: 1,
        },
        input_id: effect.into(),
        effect_id: effect.into(),
        generation: 1,
        intent_digest: [0; 32],
    }
}

struct WakeCount(AtomicUsize);

impl Wake for WakeCount {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

async fn idle_and_select_cancellation() {
    let mut tasks = JoinSet::<u8>::new();
    let (control, mut commands) = mpsc::channel::<()>(1);
    let counter = Arc::new(WakeCount(AtomicUsize::new(0)));
    let waker = Waker::from(Arc::clone(&counter));
    let mut cx = Context::from_waker(&waker);
    {
        let mut wait = pin!(async {
            tokio::select! {
                result = tasks.join_next_with_id(), if !tasks.is_empty() => result.map(|_| ()),
                command = commands.recv() => command,
            }
        });
        assert!(matches!(wait.as_mut().poll(&mut cx), Poll::Pending));
        assert_eq!(counter.0.load(Ordering::SeqCst), 0);
    }

    let (finish, finished) = oneshot::channel();
    let task = tasks.spawn(async move {
        finished.await.expect("release work");
        7
    });
    let task_id = task.id();
    control.send(()).await.expect("submit control");
    tokio::select! {
        result = tasks.join_next_with_id() => panic!("work completed before release: {result:?}"),
        command = commands.recv() => assert_eq!(command, Some(())),
    }
    assert_eq!(tasks.len(), 1, "losing select branch must retain the task");
    finish.send(()).expect("finish work");
    assert_eq!(
        tasks
            .join_next_with_id()
            .await
            .expect("task")
            .expect("join"),
        (task_id, 7)
    );
    println!("PASS idle waits without self-wake; control wins without losing completion");
}

async fn identity_and_settlement_capacity() {
    let capacity = Arc::new(Semaphore::new(1));
    let permit = capacity
        .clone()
        .try_acquire_owned()
        .expect("admit old effect");
    let mut tasks = JoinSet::<u8>::new();
    let mut records = HashMap::new();
    let old = identity("old-turn-effect");
    let current = identity("new-turn-effect");
    let executions = Arc::new(AtomicUsize::new(0));
    let executed = Arc::clone(&executions);
    let task = tasks.spawn(async move {
        executed.fetch_add(1, Ordering::SeqCst);
        panic!("injected old task panic")
    });
    records.insert(task.id(), (old.clone(), permit));

    let error = tasks
        .join_next_with_id()
        .await
        .expect("task")
        .expect_err("panic");
    let (origin, permit) = records.remove(&error.id()).expect("panic retains origin");
    assert!(error.is_panic());
    assert_eq!(origin, old);
    assert_ne!(
        origin, current,
        "old failure cannot interrupt the current effect"
    );

    // Use a scratch file to exercise a real IO failure and retry. This is not
    // the canonical journal and does not validate its crash-durability contract.
    let retained = (origin, error, permit);
    let scratch = std::env::temp_dir().join(format!(
        "qaqh-owner-design-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let marker = scratch.join("settlement");
    let commit_marker = || -> std::io::Result<()> {
        let mut file = std::fs::File::create(&marker)?;
        file.write_all(retained.0.effect_id.as_bytes())?;
        file.sync_all()
    };
    assert_eq!(
        commit_marker().expect_err("parent absent").kind(),
        std::io::ErrorKind::NotFound
    );
    assert!(capacity.clone().try_acquire_owned().is_err());
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    assert_eq!(
        capacity.available_permits(),
        0,
        "retain budget with evidence"
    );
    // Commit succeeds on retry: release the old lease before dispatching a successor.
    std::fs::create_dir(&scratch).expect("create scratch parent");
    commit_marker().expect("retry settlement IO");
    assert_eq!(
        std::fs::read_to_string(&marker).expect("read marker"),
        old.effect_id
    );
    assert_eq!(
        executions.load(Ordering::SeqCst),
        1,
        "retry did not rerun work"
    );
    drop(retained);
    let _next = capacity
        .clone()
        .try_acquire_owned()
        .expect("successor at capacity one");
    std::fs::remove_file(&marker).expect("remove marker");
    std::fs::remove_dir(&scratch).expect("remove scratch parent");
    println!(
        "PASS panic retains identity; failed settlement retains lease; capacity-one successor admits"
    );
}

async fn shutdown_drains_blocking_work() {
    let capacity = Arc::new(Semaphore::new(1));
    let permit = capacity
        .clone()
        .try_acquire_owned()
        .expect("admit blocking work");
    let (release, blocked) = std::sync::mpsc::channel();
    let (entered, started) = oneshot::channel();
    let (cancel, mut cancelled) = watch::channel(false);
    let (observed, cancel_observed) = oneshot::channel();
    let mut tasks = JoinSet::new();
    tasks.spawn(async move {
        let worker = tokio::task::spawn_blocking(move || {
            entered.send(()).expect("announce start");
            blocked.recv().expect("release blocking worker");
            9
        });
        cancelled.changed().await.expect("shutdown request");
        observed.send(()).expect("announce cancellation");
        // Keep the join handle even after cancellation, as the real runners do.
        worker.await.expect("blocking worker")
    });
    started.await.expect("worker entered");
    cancel.send(true).expect("request shutdown");
    cancel_observed.await.expect("cancellation observed");
    assert!(
        tasks.try_join_next().is_none(),
        "cancel is not stop evidence"
    );
    assert_eq!(capacity.available_permits(), 0);
    release.send(()).expect("allow worker to stop");
    assert_eq!(tasks.join_next().await.expect("task").expect("join"), 9);
    assert!(tasks.is_empty());
    drop(permit); // Model the confirmed terminal settlement after draining.
    assert_eq!(capacity.available_permits(), 1);
    println!("PASS shutdown retains owner and capacity until blocking work actually exits");
}

fn main() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(async {
            idle_and_select_cancellation().await;
            identity_and_settlement_capacity().await;
            shutdown_drains_blocking_work().await;
        });
}
