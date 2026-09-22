//! Knife-1 step-1 regression: `AgentRegistry::spawn_subagent` must create a
//! daemon-thread Ringing Loop, not a `qaqh agent --seed` child process.

use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, Instant};

use qaqh_domain::{
    ControlCommand, ControlEvent, ConversationCommand, ConversationEvent, DomainEvent,
    RingingChannel, SessionState,
};
use qaqh_ringing::{RingingCommand, RingingEvent, RingingWorkerCommandEnvelope};
use qaqh_runtime::{AgentRegistry, RingingHub};

static TEST_LOCK: Mutex<()> = Mutex::new(());

/// 进程级装配（QAQH_DATA_DIR / 工作区 / SessionManager 单例 / 工具快照）。
/// 必须在 [`test_guard`] 内调用：这些都是进程级全局状态，且
/// `SessionManager::init` 只能成功一次。
static ENV_INIT: Once = Once::new();

fn test_guard() -> std::sync::MutexGuard<'static, ()> {
    // 中毒只说明别的用例 panic 过，不代表本用例的前置条件不成立。
    TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn init_env(tag: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!(
        "qaqh-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let data = root.join("data");
    std::fs::create_dir_all(&data).expect("test setup must not fail");
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).expect("test setup must not fail");
    unsafe {
        // QAQH_DATA_DIR is process-wide; this integration-test binary holds
        // TEST_LOCK around all global state mutations.
        std::env::set_var("QAQH_DATA_DIR", &data);
    }
    qaqh_workspace::set_workspace(&ws.to_string_lossy());
    ENV_INIT.call_once(|| qaqh_session::SessionManager::init(qaqh_types::platform::data_dir()));
    // Daemon process manager snapshot: must stay stable while the actor
    // installs its private ToolManager.
    qaqh_workspace::runtime::init_tools("daemon-test", &[], vec![]);
    root
}

fn spawn_linked_subagent(registry: &mut AgentRegistry, parent: &str, child: &str) {
    qaqh_workspace::runtime::set_context(parent, 4);
    let result = registry.spawn_subagent(child, &[], None, None, None);
    qaqh_workspace::runtime::clear_context();
    result.unwrap_or_else(|error| panic!("spawn child {child} under {parent}: {error}"));
}

#[test]
fn spawn_subagent_runs_inprocess_loops_and_shutdown_signals_all() {
    let _test_lock = test_guard();
    let _root = init_env("subagent-inprocess-test");
    let process_tools = qaqh_workspace::runtime::process_all_tool_names();

    let seed = format!("sub-inproc-{}", std::process::id());
    let hub = Arc::new(RingingHub::new("subagent-inprocess-test"));
    let mut control_rx = hub.subscribe_channel(qaqh_domain::RingingChannel::Control);
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        while let Ok(envelope) = control_rx.blocking_recv() {
            if event_tx.send(envelope).is_err() {
                break;
            }
        }
    });
    let mut registry = AgentRegistry::new(qaqh_session::SessionManager::global());
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

    // The process-level snapshot must stay stable and must NOT pick up the
    // actor's private manager (which is now thread-local per actor): the daemon
    // `skills.list_tools` view reflects only the daemon's own registrar set, so
    // a running subagent actor cannot leak its tool set into the daemon.
    assert_eq!(
        qaqh_workspace::runtime::process_all_tool_names(),
        process_tools,
        "daemon process tool snapshot must be stable while an actor is running"
    );
    assert!(
        !qaqh_workspace::runtime::process_all_tool_names()
            .iter()
            .any(|name| name == "spawn_subagent"),
        "actor-private tools (spawn_subagent) must not leak into the daemon snapshot"
    );

    // Knife-1 step 2: a second subagent must run concurrently (per-actor
    // thread-local state), not queue behind a process-wide serialization lock.
    // shutdown_all must signal every instance before joining any of them, or
    // this test hangs.
    let queued_seed = format!("sub-concurrent-{}", std::process::id());
    registry
        .spawn_subagent(&queued_seed, &[], None, None, None)
        .expect("spawn concurrent in-process subagent");
    assert!(registry.is_running(&queued_seed));
    // Both actors alive at once proves concurrency (previously the second
    // actor blocked on SUBAGENT_ACTOR_SERIAL until the first exited).
    assert!(
        registry.is_running(&seed) && registry.is_running(&queued_seed),
        "concurrent subagents must both be running"
    );

    // The flush-stop race the old serialized path needed is gone: a subagent
    // spawned while another is mid-turn must still emit its own Created event
    // promptly. Under `ACTOR_SERIAL` this recv would time out because the
    // second actor's thread blocked behind the first actor's loop.
    let second_deadline = Instant::now() + Duration::from_secs(10);
    let mut saw_second_created = false;
    while Instant::now() < second_deadline {
        match event_rx.recv_timeout(second_deadline.saturating_duration_since(Instant::now())) {
            Ok(envelope) if envelope.seed == queued_seed => match envelope.event {
                RingingEvent::Control(ControlEvent::SessionStateChanged {
                    state: SessionState::Created,
                    ..
                }) => {
                    saw_second_created = true;
                    break;
                }
                _ => continue,
            },
            Ok(_) => continue,
            Err(error) => panic!("second actor emitted no Created event: {error}"),
        }
    }
    assert!(
        saw_second_created,
        "second subagent must reach Created while the first is still running (concurrency)"
    );

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

/// T-1-1 回归：`spawn_subagent_inprocess` 必须把子 seed 登记进 hub 的活表。
///
/// 未修复时本测试红：子 seed 不在 `live_workers`，bootstrap 路径
/// （`seal_orphan_channel_state(seed, force=false)`）会把它判为孤儿，封禁其
/// 正在进行的 turn（前端据此显示 cancelled），而子 actor 仍在运行并继续
/// 发布事件——即「已判定 cancel 的子代理复活」。
#[test]
fn spawn_subagent_registers_liveness() {
    let _test_lock = test_guard();
    let _root = init_env("subagent-liveness-test");
    let seed = format!("sub-live-{}", std::process::id());
    let hub = Arc::new(RingingHub::new("subagent-liveness-test"));
    let mut registry = AgentRegistry::new(qaqh_session::SessionManager::global());
    registry.attach_ringing(Arc::clone(&hub));

    registry
        .spawn_subagent(&seed, &[], None, None, None)
        .expect("spawn in-process subagent");

    // 构造可观察的「无终态 running 状态」：一个已开但未收尾的 turn。
    hub.publish(
        &seed,
        DomainEvent::Conversation(ConversationEvent::TurnStarted {
            turn_id: "t1".into(),
            user_text: "hello".into(),
        }),
    );
    let before = hub.snapshot(RingingChannel::Conversation, &seed);
    assert_eq!(
        before.state.get("active_turn").and_then(|v| v.as_str()),
        Some("t1"),
        "前置条件：存在未收尾的 running turn"
    );

    // 活 worker 在册 → bootstrap 收尾（force=false）必须整体跳过。
    assert!(
        !hub.seal_orphan_channel_state(&seed, false),
        "活 worker 的 seed 不得被 bootstrap 收尾（本 seed 必须已在 live_workers）"
    );

    let after = hub.snapshot(RingingChannel::Conversation, &seed);
    assert_eq!(
        after.state.get("active_turn").and_then(|v| v.as_str()),
        Some("t1"),
        "活 worker 的 running turn 不得被 seal——否则前端显示 cancelled 而 actor 仍在跑"
    );

    registry.shutdown_all();
}

/// T-1-4 回归：父会话收到 `ConversationCancel` 时，取消必须传播到它派生的
/// 子 seed（子 actor 收到 `ConversationCancel` 并发布 `ConversationCancelled`）。
///
/// 未修复时本测试红：子 seed 收不到任何取消，继续跑完自己的回合并（在
/// T-1-2 之前）把结果注入已取消的父会话。
#[test]
fn parent_cancel_propagates_to_children() {
    let _test_lock = test_guard();
    let _root = init_env("subagent-parent-cancel-test");
    let parent = format!("sub-parent-{}", std::process::id());
    let child = format!("sub-child-{}", std::process::id());
    let hub = Arc::new(RingingHub::new("subagent-parent-cancel-test"));
    let mut registry = AgentRegistry::new(qaqh_session::SessionManager::global());
    registry.attach_ringing(Arc::clone(&hub));

    // 父：无运行时上下文 → 无父链（顶层会话语义）。
    qaqh_workspace::runtime::clear_context();
    registry
        .spawn_subagent(&parent, &[], None, None, None)
        .expect("spawn parent");
    // 子：spawn_subagent handler 运行在父会话的工具线程上，RUNTIME_CTX 即父
    // 会话 → 登记 parent -> child。
    qaqh_workspace::runtime::set_context(&parent, 4);
    registry
        .spawn_subagent(&child, &[], None, None, None)
        .expect("spawn child");
    qaqh_workspace::runtime::clear_context();

    assert_eq!(
        registry.subagent_children(&parent),
        vec![child.clone()],
        "spawn 时必须登记 parent -> children"
    );

    let mut child_events = hub.subscribe(RingingChannel::Conversation, &child);

    let cancel_env = RingingWorkerCommandEnvelope::new(
        parent.clone(),
        "cancel-parent-1",
        RingingCommand::Conversation(ConversationCommand::ConversationCancel { turn_id: None }),
    );
    registry
        .send_ringing(&parent, &cancel_env)
        .expect("cancel must reach the parent worker");

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut saw_child_cancel = false;
    while Instant::now() < deadline {
        match child_events.try_recv() {
            Ok(envelope)
                if envelope.seed == child
                    && matches!(
                        envelope.event,
                        RingingEvent::Conversation(ConversationEvent::ConversationCancelled { .. })
                    ) =>
            {
                saw_child_cancel = true;
                break;
            }
            Ok(_) => continue,
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    assert!(
        saw_child_cancel,
        "父取消必须传播到子 seed（子 actor 应发布 ConversationCancelled）"
    );

    registry.shutdown_all();
}

/// #114：parent close 必须先递归取消并 join 整棵 child 树；返回后任何后代
/// 都不得仍在 registry 中可运行。
#[test]
fn parent_close_cancels_and_joins_child_tree() {
    let _test_lock = test_guard();
    let _root = init_env("subagent-parent-close-tree-test");
    let parent = format!("sub-close-parent-{}", std::process::id());
    let child = format!("sub-close-child-{}", std::process::id());
    let grandchild = format!("sub-close-grandchild-{}", std::process::id());
    let hub = Arc::new(RingingHub::new("subagent-parent-close-tree-test"));
    let mut registry = AgentRegistry::new(qaqh_session::SessionManager::global());
    registry.attach_ringing(hub);

    qaqh_workspace::runtime::clear_context();
    registry
        .spawn_subagent(&parent, &[], None, None, None)
        .expect("spawn parent");
    spawn_linked_subagent(&mut registry, &parent, &child);
    spawn_linked_subagent(&mut registry, &child, &grandchild);

    assert_eq!(
        registry.subagent_children(&parent),
        vec![child.clone()],
        "parent must own child edge"
    );
    assert_eq!(
        registry.subagent_children(&child),
        vec![grandchild.clone()],
        "child must own grandchild edge"
    );
    assert!(registry.is_running(&parent));
    assert!(registry.is_running(&child));
    assert!(registry.is_running(&grandchild));

    registry.close(&parent);

    assert!(!registry.is_running(&parent), "parent must be joined");
    assert!(!registry.is_running(&child), "child must be joined");
    assert!(
        !registry.is_running(&grandchild),
        "grandchild must be joined"
    );
    assert!(registry.subagent_children(&parent).is_empty());
    assert!(registry.subagent_children(&child).is_empty());

    // Repeated close must stay idempotent after the whole tree is gone.
    registry.close(&parent);
}

/// P2-5 Gate：parent unload 必须等到 child terminal 先闭合 edge，再 join
/// child，最后才对 parent unload ack。
#[test]
fn parent_unload_waits_child_terminal_join() {
    let _test_lock = test_guard();
    let _root = init_env("subagent-parent-unload-order-test");
    let parent = format!("sub-unload-parent-{}", std::process::id());
    let child = format!("sub-unload-child-{}", std::process::id());
    let hub = Arc::new(RingingHub::new("subagent-parent-unload-order-test"));
    let mut registry = AgentRegistry::new(qaqh_session::SessionManager::global());
    registry.attach_ringing(hub);

    qaqh_workspace::runtime::clear_context();
    registry.spawn_new(&parent).expect("spawn parent session");
    spawn_linked_subagent(&mut registry, &parent, &child);

    registry.close(&parent);

    let trace = registry.subagent_lifecycle_trace();
    let ordered: Vec<String> = trace
        .into_iter()
        .filter(|event| {
            matches!(
                event.split(':').next(),
                Some(
                    "parent_unload_requested"
                        | "child_cancel_sent"
                        | "child_terminal"
                        | "parent_subagent_finished"
                        | "child_joined"
                        | "parent_unload_ack"
                )
            )
        })
        .collect();
    assert_eq!(
        ordered,
        vec![
            format!("parent_unload_requested:{parent}"),
            format!("child_cancel_sent:{parent}:{child}"),
            format!("child_terminal:{parent}:{child}"),
            format!("parent_subagent_finished:{parent}:{child}"),
            format!("child_joined:{parent}:{child}"),
            format!("parent_unload_ack:{parent}"),
        ],
        "parent unload must follow child terminal -> parent edge finish -> child join -> parent ack"
    );
    assert!(!registry.is_running(&parent));
    assert!(!registry.is_running(&child));
}

/// P2-5：daemon shutdown 也走同一 child terminal -> edge finish -> join 顺序。
#[test]
fn shutdown_all_waits_child_terminal_join() {
    let _test_lock = test_guard();
    let _root = init_env("subagent-shutdown-order-test");
    let parent = format!("sub-shutdown-parent-{}", std::process::id());
    let child = format!("sub-shutdown-child-{}", std::process::id());
    let hub = Arc::new(RingingHub::new("subagent-shutdown-order-test"));
    let mut registry = AgentRegistry::new(qaqh_session::SessionManager::global());
    registry.attach_ringing(hub);

    qaqh_workspace::runtime::clear_context();
    registry.spawn_new(&parent).expect("spawn parent session");
    spawn_linked_subagent(&mut registry, &parent, &child);

    registry.shutdown_all();

    let ordered: Vec<String> = registry
        .subagent_lifecycle_trace()
        .into_iter()
        .filter(|event| {
            matches!(
                event.split(':').next(),
                Some(
                    "parent_unload_requested"
                        | "child_cancel_sent"
                        | "child_terminal"
                        | "parent_subagent_finished"
                        | "child_joined"
                        | "parent_unload_ack"
                )
            )
        })
        .collect();
    assert_eq!(
        ordered,
        vec![
            format!("parent_unload_requested:{parent}"),
            format!("child_cancel_sent:{parent}:{child}"),
            format!("child_terminal:{parent}:{child}"),
            format!("parent_subagent_finished:{parent}:{child}"),
            format!("child_joined:{parent}:{child}"),
            format!("parent_unload_ack:{parent}"),
        ],
        "shutdown must use the same ordered child-tree teardown"
    );
    assert!(!registry.is_running(&parent));
    assert!(!registry.is_running(&child));
}

/// P2-5：parent worker 已退出（panic/异常终止）时，respawn 前必须先关闭
/// child tree，不能留下孤儿 child。
#[test]
fn dead_parent_closes_child_tree_before_respawn() {
    let _test_lock = test_guard();
    let _root = init_env("subagent-dead-parent-test");
    let parent = format!("sub-dead-parent-{}", std::process::id());
    let child = format!("sub-dead-child-{}", std::process::id());
    let hub = Arc::new(RingingHub::new("subagent-dead-parent-test"));
    let mut registry = AgentRegistry::new(qaqh_session::SessionManager::global());
    registry.attach_ringing(hub);

    qaqh_workspace::runtime::clear_context();
    registry.spawn_new(&parent).expect("spawn parent session");
    spawn_linked_subagent(&mut registry, &parent, &child);

    let shutdown = RingingWorkerCommandEnvelope::new(
        parent.clone(),
        "test-parent-exit",
        RingingCommand::Control(ControlCommand::SessionShutdown),
    );
    registry
        .send_ringing(&parent, &shutdown)
        .expect("send parent shutdown");

    let deadline = Instant::now() + Duration::from_secs(10);
    while !registry.worker_finished(&parent) {
        assert!(
            Instant::now() < deadline,
            "parent worker must exit before respawn test"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    // respawn_dead_agents has a 1s crash-loop backoff; keep this test honest
    // about exercising the dead-parent path rather than the backoff branch.
    std::thread::sleep(Duration::from_millis(1100));
    registry.respawn_dead_agents();

    assert!(registry.is_running(&parent), "parent must be respawned");
    assert!(
        !registry.is_running(&child),
        "child tree must be closed before parent respawn"
    );
    let trace = registry.subagent_lifecycle_trace();
    let child_terminal = trace
        .iter()
        .position(|event| event == &format!("child_terminal:{parent}:{child}"))
        .expect("child terminal must be recorded");
    let parent_finished = trace
        .iter()
        .position(|event| event == &format!("parent_subagent_finished:{parent}:{child}"))
        .expect("parent edge finish must be recorded");
    let child_joined = trace
        .iter()
        .position(|event| event == &format!("child_joined:{parent}:{child}"))
        .expect("child join must be recorded");
    assert!(
        child_terminal < parent_finished && parent_finished < child_joined,
        "dead parent teardown must preserve terminal -> edge finish -> join: {trace:?}"
    );

    registry.shutdown_all();
}

/// #114：idle unload 走同一 close 路径，父 session 卸载后 child 不得继续运行。
#[test]
fn parent_idle_unload_cancels_and_joins_child_tree() {
    let _test_lock = test_guard();
    let _root = init_env("subagent-parent-idle-unload-test");
    let parent = format!("session-idle-parent-{}", std::process::id());
    let child = format!("sub-idle-child-{}", std::process::id());
    let hub = Arc::new(RingingHub::new("subagent-parent-idle-unload-test"));
    let mut registry = AgentRegistry::new(qaqh_session::SessionManager::global());
    registry.attach_ringing(hub);

    qaqh_workspace::runtime::clear_context();
    registry.spawn_new(&parent).expect("spawn parent session");
    spawn_linked_subagent(&mut registry, &parent, &child);
    assert!(registry.is_running(&parent));
    assert!(registry.is_running(&child));

    let liveness = registry
        .worker_liveness(&parent)
        .expect("parent session must expose liveness");
    liveness.rewind_last_activity(7200);
    let unloaded = registry.unload_idle_sessions(3600);

    assert_eq!(unloaded, vec![parent.clone()]);
    assert!(!registry.is_running(&parent), "parent must be unloaded");
    assert!(
        !registry.is_running(&child),
        "child must not survive parent unload"
    );
    assert!(
        registry.unload_idle_sessions(3600).is_empty(),
        "repeated unload must be idempotent"
    );
}

/// #114：无 child 的普通 close 行为保持不变。
#[test]
fn childless_close_remains_working_and_idempotent() {
    let _test_lock = test_guard();
    let _root = init_env("subagent-childless-close-test");
    let seed = format!("sub-childless-{}", std::process::id());
    let hub = Arc::new(RingingHub::new("subagent-childless-close-test"));
    let mut registry = AgentRegistry::new(qaqh_session::SessionManager::global());
    registry.attach_ringing(hub);

    qaqh_workspace::runtime::clear_context();
    registry
        .spawn_subagent(&seed, &[], None, None, None)
        .expect("spawn childless subagent");
    assert!(registry.is_running(&seed));

    registry.close(&seed);
    assert!(!registry.is_running(&seed));

    registry.close(&seed);
}
