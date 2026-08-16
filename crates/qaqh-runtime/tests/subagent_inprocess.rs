//! Knife-1 step-1 regression: `AgentRegistry::spawn_subagent` must create a
//! daemon-thread Ringing Loop, not a `qaqh agent --seed` child process.

use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, Instant};

use qaqh_domain::{ControlEvent, SessionState};
use qaqh_ringing::RingingEvent;
use qaqh_runtime::{AgentRegistry, RingingHub};

static TEST_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn spawn_subagent_runs_inprocess_loops_and_shutdown_signals_all() {
    let _test_lock = TEST_LOCK.lock().expect("test setup must not fail");
    let root = std::env::temp_dir().join(format!(
        "qaqh-subagent-inprocess-test-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
    ));
    let data = root.join("data");
    std::fs::create_dir_all(&data).expect("test setup must not fail");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).expect("test setup must not fail");
    unsafe {
        // QAQH_DATA_DIR is process-wide; this integration-test binary contains
        // a single test and holds TEST_LOCK around all global state mutations.
        std::env::set_var("QAQH_DATA_DIR", &data);
    }
    qaqh_workspace::set_workspace(&ws.to_string_lossy());
    static INIT: Once = Once::new();
    INIT.call_once(|| qaqh_session::SessionManager::init(qaqh_types::platform::data_dir()));
    // Daemon process manager snapshot: must stay stable while the actor
    // installs its private ToolManager.
    qaqh_workspace::runtime::init_tools("daemon-test", &[], vec![]);
    let process_tools = qaqh_workspace::runtime::process_all_tool_names();

    let seed = format!("sub-inproc-{}", std::process::id());
    let hub = Arc::new(RingingHub::new("subagent-inprocess-test"));
    let mut control_rx = hub.subscribe(qaqh_domain::RingingChannel::Control);
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        while let Ok(envelope) = control_rx.blocking_recv() {
            if event_tx.send(envelope).is_err() {
                break;
            }
        }
    });
    let mut registry = AgentRegistry::new();
    registry.attach_ringing(hub);

    registry
        .spawn_subagent(&seed, &[], None, None, None)
        .expect("spawn in-process subagent");
    assert!(registry.is_running(&seed), "registry must track the actor");

    // The actor emits SessionStateChanged(Created) through the same hub path as
    // a process worker's stdout reader. Its private ToolManager is visible to
    // actor tool calls but must not replace the daemon process snapshot.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match event_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(envelope) if envelope.seed == seed => match envelope.event {
                RingingEvent::Control(ControlEvent::SessionStateChanged {
                    state: SessionState::Created,
                    ..
                }) => break,
                _ => continue,
            },
            Ok(_) => continue,
            Err(error) => panic!("subagent actor emitted no Created event: {error}"),
        }
    }

    assert_eq!(
        qaqh_workspace::runtime::process_all_tool_names(),
        process_tools,
        "daemon process tool snapshot must be stable while an actor is running"
    );
    let actor_tool_names = qaqh_workspace::runtime::all_tool_names();
    assert!(
        actor_tool_names.iter().any(|name| name == "spawn_subagent"),
        "actor manager must include qaqh-subagent registrar: {actor_tool_names:?}"
    );

    // A second actor queues behind SUBAGENT_ACTOR_SERIAL. shutdown_all must
    // signal every instance before joining any of them, or this test hangs.
    let queued_seed = format!("sub-queued-{}", std::process::id());
    registry
        .spawn_subagent(&queued_seed, &[], None, None, None)
        .expect("spawn queued in-process subagent");
    assert!(registry.is_running(&queued_seed));

    registry.shutdown_all();
    assert!(!registry.is_running(&seed));
    assert!(!registry.is_running(&queued_seed));
}

#[test]
fn spawn_subagent_does_not_go_through_process_spawn() {
    let root = env!("CARGO_MANIFEST_DIR");
    let source = std::fs::read_to_string(format!("{root}/src/registry.rs"))
        .expect("read qaqh-runtime registry.rs");
    let body = source
        .lines()
        .skip_while(|line| !line.contains("pub fn spawn_subagent("))
        .skip(1)
        .take_while(|line| !line.contains("fn spawn_subagent_inprocess("))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        !body.contains("Command::new"),
        "spawn_subagent must not construct a child process: {body}"
    );
    assert!(
        !body.contains("spawn_with("),
        "spawn_subagent must not route through the process worker path: {body}"
    );
    assert!(
        body.contains("spawn_subagent_inprocess("),
        "spawn_subagent must delegate to the in-process actor path: {body}"
    );
}
