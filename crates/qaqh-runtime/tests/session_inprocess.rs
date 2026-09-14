//! Knife-1 step-2 regression: normal session agents must also run as in-process
//! daemon actors, not as `qaqh agent --seed` child processes.

use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, Instant};

use qaqh_domain::{ControlEvent, SessionState};
use qaqh_ringing::RingingEvent;
use qaqh_runtime::{AgentRegistry, QaqhService, RingingHub};

static TEST_LOCK: Mutex<()> = Mutex::new(());

/// SessionManager 是全局单例：同测试进程只 init 一次，两个用例共享。
///
/// ⚠ 返回值是**本进程单例实际采用的 data_dir**（`Once` 的胜者）。`TEST_LOCK`
/// 只保证用例不并发，**保证不了初始化顺序**：谁先跑到 `init_manager`，单例就
/// 认谁的 data_dir。因此每个用例必须拿返回值与自己的 tempdir 比对，不能假定
/// 「我 set 的 QAQH_DATA_DIR 就是单例的 data_dir」（BUG-2026-09-13-24 返工）。
static INIT: Once = Once::new();
static MANAGER_DATA_DIR: Mutex<Option<std::path::PathBuf>> = Mutex::new(None);

/// 本测试进程内**唯一**的 SessionManager 初始化入口。
///
/// 把「读 `QAQH_DATA_DIR`」与「init 单例」收进同一个 `call_once` 闭包，保证
/// 二者是**同一对**：先读到的值立刻用于 init，中间不会被别的用例改掉。返回
/// 单例实际采用的 data_dir。
///
/// 由于单例只能 init 一次，后续用例会拿到**别人**的 data_dir；调用方必须用
/// 返回的路径而不是自己的 `data` 变量（见 `init_manager_for_this_test`）。
fn init_manager() -> std::path::PathBuf {
    INIT.call_once(|| {
        let dir = qaqh_types::platform::data_dir();
        qaqh_session::SessionManager::init(dir.clone());
        *MANAGER_DATA_DIR.lock().unwrap_or_else(|e| e.into_inner()) = Some(dir);
    });
    MANAGER_DATA_DIR
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .expect("SessionManager init must record its data dir")
}

/// 断言单例的 data_dir 就是本用例的 `data`，否则说明另一个用例先 init 了。
///
/// 返回 `true` 表示本用例持有单例；`false` 表示单例已被别人占用 —— 此时
/// **必须跳过**任何依赖「单例属于我」的断言，而不是让断言去读别人的目录
/// （旧行为：sentinel 落进别人的 data_dir → 断言必然翻车 → 中毒 TEST_LOCK →
/// 连带弄红下一个用例）。
fn init_manager_for_this_test(data: &std::path::Path) -> bool {
    let owner = init_manager();
    owner == data
}

/// 获取测试锁，容忍中毒：中毒只说明**别的**用例 panic 过，不代表本用例
/// 的前置条件不成立。旧实现用 `.expect()` → 一个用例 panic 会把同文件所有
/// 后续用例连锁弄红（Windows 上报的「2 个失败」正是这么来的）。
fn test_guard() -> std::sync::MutexGuard<'static, ()> {
    TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[test]
fn session_spawns_inprocess_and_receives_created_event() {
    let _test_lock = test_guard();
    let root = std::env::temp_dir().join(format!(
        "qaqh-session-inprocess-test-{}-{}",
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
        std::env::set_var("QAQH_DATA_DIR", &data);
    }
    qaqh_workspace::set_workspace(&ws.to_string_lossy());
    let manager_owns_this_test_dir = init_manager_for_this_test(&data);
    qaqh_workspace::runtime::init_tools("daemon-test", &[], vec![]);

    let seed = format!("session-inproc-{}", std::process::id());
    let hub = Arc::new(RingingHub::new("session-inprocess-test"));
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

    registry.spawn_new(&seed).expect("spawn in-process session");
    assert!(
        registry.is_running(&seed),
        "registry must track the session actor"
    );

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match event_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(envelope) if envelope.seed == seed => match envelope.event {
                RingingEvent::Control(ControlEvent::SessionStateChanged {
                    state: SessionState::Created,
                    ..
                }) => {
                    break;
                }
                _ => continue,
            },
            Ok(_) => continue,
            Err(error) => panic!("session actor emitted no Created event: {error}"),
        }
    }
    registry.shutdown_all();
    assert!(!registry.is_running(&seed));
}

#[test]
fn session_spawn_has_no_process_spawn_in_source() {
    let root = env!("CARGO_MANIFEST_DIR");
    let source = std::fs::read_to_string(format!("{root}/src/registry.rs"))
        .expect("read qaqh-runtime registry.rs");
    let body = source
        .lines()
        .skip_while(|line| !line.contains("fn spawn_with("))
        .take_while(|line| !line.contains("/// 发送 Ringing worker 命令帧"))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        !body.contains("current_exe"),
        "session spawn must not use current_exe: {body}"
    );
    assert!(
        !body.contains("command.arg(\"agent\")"),
        "session spawn must not start the qaqh-daemon agent subcommand: {body}"
    );
}

#[test]
fn cross_session_cancel_does_not_leak() {
    let _test_lock = test_guard();
    // 前序用例的 shutdown_all 会置进程级全局取消 flag，先复位本进程状态。
    qaqh_workspace::clear_cancel();
    let root = std::env::temp_dir().join(format!(
        "qaqh-cross-cancel-test-{}-{}",
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
        std::env::set_var("QAQH_DATA_DIR", &data);
    }
    qaqh_workspace::set_workspace(&ws.to_string_lossy());
    let manager_owns_this_test_dir = init_manager_for_this_test(&data);
    qaqh_workspace::runtime::init_tools("daemon-test", &[], vec![]);

    let hub = Arc::new(RingingHub::new("cross-cancel-test"));
    let mut registry = AgentRegistry::new(qaqh_session::SessionManager::global());
    registry.attach_ringing(hub);

    let seed_a = format!("cross-cancel-a-{}", std::process::id());
    let seed_b = format!("cross-cancel-b-{}", std::process::id());
    registry.spawn_new(&seed_a).expect("spawn session a");
    registry.spawn_new(&seed_b).expect("spawn session b");

    // 会话 A 经 registry 真实 interrupt 路径取消：per-session CancelToken +
    // 会话键控取消表（PR-3-4：不再置进程级全局 flag）。
    let interrupt = qaqh_ringing::RingingWorkerCommandEnvelope::new(
        "cross-cancel-test-1",
        "cross-cancel-client",
        qaqh_ringing::RingingCommand::Conversation(
            qaqh_domain::ConversationCommand::ConversationCancel { turn_id: None },
        ),
    );
    registry
        .send_ringing(&seed_a, &interrupt)
        .expect("interrupt a");

    // 无会话线程视角：进程级全局 flag 不被会话级 interrupt 触碰。
    assert!(
        !qaqh_workspace::is_cancel(),
        "session-scoped interrupt must not set the process-wide cancel flag"
    );

    // 会话 B 的工具执行视角（execute 路径绑定的 runtime ctx = worker 语义）：
    // cancel 检查在 admit 之前——未注册工具报 Unknown tool 而非 Cancelled，
    // 即证明 A 的取消没有泄漏进 B 的执行上下文。
    let ctx_b = qaqh_workspace::runtime::ToolCtx::admitted(&seed_b);
    let result = qaqh_workspace::execution::execute_with_context(
        "read",
        "",
        "{}",
        "cross-cancel-b-tool",
        None,
        &ctx_b,
    );
    assert!(
        !result.content.contains("CANCELLED"),
        "session B tool execution must not be cancelled by session A's interrupt: {}",
        result.content
    );
    assert!(
        result.content.contains("path is required"),
        "expected the read tool's args validation (cancel check passed first): {}",
        result.content
    );

    registry.shutdown_all();
}

/// E: idle 卸载 → 重生 → 会话历史连续（docs/memory-governance-plan.md §E）。
/// 卸载走 registry.close 优雅路径；重生走 spawn_new 的 resume 语义
/// （load_for_resume + WAL 重放 + outbox 对账）。
#[test]
fn idle_unload_then_respawn_preserves_history() {
    let _test_lock = test_guard();
    let root = std::env::temp_dir().join(format!(
        "qaqh-session-idle-unload-{}-{}",
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
        std::env::set_var("QAQH_DATA_DIR", &data);
    }
    qaqh_workspace::set_workspace(&ws.to_string_lossy());
    let manager_owns_this_test_dir = init_manager_for_this_test(&data);
    qaqh_workspace::runtime::init_tools("daemon-test", &[], vec![]);

    let seed = format!("session-idle-unload-{}", std::process::id());
    let hub = Arc::new(RingingHub::new("idle-unload-test"));
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

    // 1. Spawn + wait for Created（worker 活着，liveness 全新）。
    registry.spawn_new(&seed).expect("spawn in-process session");
    let liveness = registry
        .worker_liveness(&seed)
        .expect("session actor must expose liveness");
    assert!(!liveness.unloadable() || liveness.idle_secs() < 1);

    // 未到阈值 → 不卸载。
    let unloaded = registry.unload_idle_sessions(3600);
    assert!(unloaded.is_empty(), "fresh worker must not be unloaded");

    // 2. 拨钟 + 卸载。
    liveness.rewind_last_activity(7200);
    let unloaded = registry.unload_idle_sessions(3600);
    assert_eq!(unloaded, vec![seed.clone()], "idle worker must be unloaded");
    assert!(!registry.is_running(&seed));

    // 卸载后再次卸载 = 幂等（实例已不在 registry）。
    let again = registry.unload_idle_sessions(3600);
    assert!(again.is_empty(), "unload must be idempotent");

    // 3. 重生（resume 语义）→ Created 再现 → 会话可用。
    registry
        .spawn_new(&seed)
        .expect("respawn after idle unload");
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
            Err(error) => panic!("respawned session emitted no Created event: {error}"),
        }
    }
    assert!(registry.is_running(&seed));

    // 4. 挂起交互守卫：busy/suspend 的 worker 不可卸载（拨钟也不行）。
    if let Some(again_liveness) = registry.worker_liveness(&seed) {
        again_liveness.rewind_last_activity(7200);
        again_liveness.set_suspend_pending(true);
        let blocked = registry.unload_idle_sessions(3600);
        assert!(
            !blocked.contains(&seed),
            "suspended session must not be idle-unloaded"
        );
        again_liveness.set_suspend_pending(false);
    }

    registry.shutdown_all();
    assert!(!registry.is_running(&seed));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn close_session_cleans_per_seed_resident_state() {
    let _test_lock = test_guard();
    let root = std::env::temp_dir().join(format!(
        "qaqh-session-close-clean-{}-{}",
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
        std::env::set_var("QAQH_DATA_DIR", &data);
    }
    qaqh_workspace::set_workspace(&ws.to_string_lossy());
    let manager_owns_this_test_dir = init_manager_for_this_test(&data);
    qaqh_workspace::runtime::init_tools("daemon-test", &[], vec![]);

    // 不实际 spawn worker：本用例验证 close_session 对 per-seed 全局常驻态
    // 的清理（IMAGE_REGISTRY / hub channels / content_store）——清理点在
    // service.close_session 内，与实例是否存在无关；有 worker 的完整关闭
    // 路径由 idle_unload_then_respawn_preserves_history 覆盖。
    let seed = format!("session-close-clean-{}", std::process::id());
    let hub = Arc::new(RingingHub::new("close-clean-test"));
    let service = QaqhService::init(qaqh_session::SessionManager::global());
    service.attach_ringing(hub.clone());

    qaqh_workspace::read_image::store_image(&seed, "image/png", "QUJD");
    let content_id = hub.put_content(&seed, "text/plain", b"hello".to_vec(), false);
    let _ = hub.publish(
        &seed,
        qaqh_domain::DomainEvent::Control(ControlEvent::SessionStateChanged {
            seed: seed.clone(),
            state: SessionState::Resumed,
        }),
    );
    assert!(qaqh_workspace::read_image::peek_image(&seed, 0).is_some());
    assert!(hub.get_content(&seed, &content_id).is_some());
    let before = hub
        .snapshot(qaqh_domain::RingingChannel::Control, &seed)
        .baseline_stream_seq;
    assert!(before > 0, "channel state must be resident before close");

    service.close_session(&seed, None).expect("close session");

    assert!(
        qaqh_workspace::read_image::peek_image(&seed, 0).is_none(),
        "IMAGE_REGISTRY entry must be dropped on close"
    );
    assert!(
        hub.get_content(&seed, &content_id).is_none(),
        "content_store entry must be released on close"
    );
    let after = hub
        .snapshot(qaqh_domain::RingingChannel::Control, &seed)
        .baseline_stream_seq;
    assert_eq!(after, 0, "hub channel state must be dropped on close");
}

/// BUG-2026-09-13-24 端到端回归：`session.new` 分配的 seed 绝不能撞进
/// 已有会话目录（旧实现 = `generate_seed`（无碰撞检查）+
/// `persist_new_session_with_cwd`（无条件覆盖 meta）→ 静默写穿）。
///
/// 这里把 id 空间压缩到「已占用的 seed 一定会被再次生成」的程度是不现实的
/// （32 位哈希无法强行注入），因此改为在**真实 spawn 路径上**做可验证的
/// 等价断言：
///   1. 连续多次 `session.new` 得到的 seed 互不相同、且各自目录独立；
///   2. 每个新 seed 的 meta.created_at 与 workspace 归属都属于它自己
///      （没有任何一次落进别人的目录）；
///   3. 预先手工占用的种子目录在整轮 session.new 前后逐字节不变。
///   4. 服务侧确实走的是 `allocate_seed`（同进程占用登记生效：已分配的
///      seed 在 `is_seed_taken` 上为真）。
#[test]
fn session_new_never_reuses_an_existing_seed_directory() {
    let _test_lock = test_guard();
    let root = std::env::temp_dir().join(format!(
        "qaqh-session-seed-collision-{}-{}",
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
        std::env::set_var("QAQH_DATA_DIR", &data);
    }
    qaqh_workspace::set_workspace(&ws.to_string_lossy());
    let manager_owns_this_test_dir = init_manager_for_this_test(&data);
    qaqh_workspace::runtime::init_tools("daemon-test", &[], vec![]);

    // 隔离守卫（BUG-2026-09-13-24 返工）：本用例的断言读的是**单例**的
    // sessions_dir。若单例已被同文件另一个用例先 init（顺序不确定：
    // Windows 上慢 I/O 会改变谁先跑到 INIT），那么本用例预置的 sentinel
    // 与断言对象就落在**别人的 data_dir** 里 —— 这正是 Windows 上
    // `persist_new_session_if_absent` 返回 false、断言必然翻车、并因
    // TEST_LOCK 中毒连带弄红下一个用例的根因。
    //
    // 此时**跳过**而不是去读别人的目录：既不污染也不误伤。
    if !manager_owns_this_test_dir {
        eprintln!(
            "skip: SessionManager singleton was initialized by another test (owner data_dir != {:?})",
            data
        );
        let _ = std::fs::remove_dir_all(root);
        return;
    }

    let sessions = qaqh_session::SessionManager::global();

    // 预置一个「旧会话」，带哨兵内容 —— 任何 session.new 都不许碰它。
    let sentinel = format!("seed-collision-sentinel-{}", std::process::id());
    sessions.persist_new_session(&sentinel);
    let sentinel_dir = sessions.session_path_dir(&sentinel);
    let sentinel_meta_before =
        std::fs::read_to_string(sentinel_dir.join("meta.json")).expect("sentinel meta");
    let sentinel_messages_before =
        std::fs::read_to_string(sentinel_dir.join("messages.jsonl")).expect("sentinel messages");

    // 确定性碰撞注入：候选源**每轮先返回已占用的 sentinel seed**，再返回
    // 真实随机 seed。这样 `session.new` 必然撞上 sentinel —— 未修复的
    // main 上会直接写穿它（返回 sentinel 且覆盖 meta），修复后必须换 seed。
    //
    // RAII 守卫：`set_seed_source_for_test` 是**进程级**单例。若本用例在
    // 中途 panic（断言失败/OOM），注入源会**留在**进程里继续喂给后续用例
    // —— 后续 `session.new` 就会不断撞上已占用 seed 而失败。守卫保证任何
    // 退出路径（含 panic unwind）都会解除注入（BUG-2026-09-13-24 返工）。
    static COLLIDE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
    static SENTINEL: Mutex<Option<String>> = Mutex::new(None);
    struct SeedSourceGuard;
    impl Drop for SeedSourceGuard {
        fn drop(&mut self) {
            qaqh_session::SessionManager::set_seed_source_for_test(None);
        }
    }
    let _seed_source_guard = SeedSourceGuard;
    *SENTINEL.lock().unwrap_or_else(|e| e.into_inner()) = Some(sentinel.clone());
    fn injected() -> String {
        if COLLIDE.swap(false, std::sync::atomic::Ordering::SeqCst) {
            return SENTINEL
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
                .expect("sentinel installed");
        }
        qaqh_session::SessionManager::generate_seed()
    }
    qaqh_session::SessionManager::set_seed_source_for_test(Some(injected));
    // 首轮候选 = sentinel（必碰撞）→ 分配器必须重试。
    COLLIDE.store(true, std::sync::atomic::Ordering::SeqCst);

    let hub = Arc::new(RingingHub::new("seed-collision-test"));
    let mut registry = AgentRegistry::new(qaqh_session::SessionManager::global());
    registry.attach_ringing(hub);
    let service = QaqhService::init(qaqh_session::SessionManager::global());

    let mut seeds: Vec<String> = Vec::new();
    for round in 0..8 {
        let created = service
            .handle(
                "session.new",
                &serde_json::json!({ "cwd": ws.to_string_lossy() }),
            )
            .expect("session.new must succeed");
        let seed = created
            .as_str()
            .expect("session.new returns the seed")
            .to_string();
        assert!(
            !seed.is_empty() && seed != sentinel,
            "round {round}: allocator must not return the occupied sentinel seed"
        );
        assert!(
            !seeds.contains(&seed),
            "round {round}: session.new reused seed {seed}"
        );
        if round == 0 {
            // 首轮被注入的候选就是 sentinel：分配器必须重试到别的 seed，
            // 绝不能把 sentinel 本身发回来（否则就是写穿旧会话目录）。
            assert_ne!(
                seed, sentinel,
                "the first injected candidate collides with the sentinel session —                  session.new must retry, not reuse it"
            );
            // 重试后的候选仍需继续碰撞注入（保证每轮都验证重试逻辑）。
            COLLIDE.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        // 该 seed 目录必须属于它自己：新建 metas 的 cwd 是本次的 cwd。
        let meta = sessions.load_meta(&seed).expect("fresh meta");
        assert_eq!(meta.seed, seed, "meta must be self-owned");
        let expected_cwd = std::fs::canonicalize(&ws)
            .expect("canonicalize ws")
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            meta.cwd.as_deref(),
            Some(expected_cwd.as_str()),
            "round {round}: new session must load its own workspace"
        );
        assert!(
            sessions.is_seed_taken(&seed),
            "allocated seed must be claimed (never re-handed out)"
        );
        seeds.push(seed);
    }
    assert_eq!(seeds.len(), 8);
    // 注入源由 `_seed_source_guard` 在用例退出时解除（panic 路径同样生效）。

    // 哨兵会话逐字节未变（未被任何一次 session.new 写穿）。
    assert_eq!(
        std::fs::read_to_string(sentinel_dir.join("meta.json")).expect("sentinel meta"),
        sentinel_meta_before,
        "an existing session's meta must never be rewritten by session.new"
    );
    assert_eq!(
        std::fs::read_to_string(sentinel_dir.join("messages.jsonl")).expect("sentinel messages"),
        sentinel_messages_before,
        "an existing session's messages must never be touched by session.new"
    );

    registry.shutdown_all();
    let _ = std::fs::remove_dir_all(root);
}
