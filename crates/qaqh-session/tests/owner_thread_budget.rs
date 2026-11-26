//! 每会话 OS 线程预算闸门（`V4_4_EXECUTION_PLAN.md` §三.6 验收：创建 100 个会话
//! 的线程数增量必须为 0）。
//!
//! 病灶（L-31 + P1-2 本体）：旧实现每建一个 `SessionOwner` 就
//! `std::thread::Builder::spawn` 一条名为 `qaqh-owner-<sid>` 的常驻泵线程，
//! 会话数即线程数；Actor 化之后泵是宿主 runtime 上的**任务**，不是线程。
//!
//! 本用例只做「建 owner」这一件事：不碰 DB、不发事件、不跑回合，因此增量的唯一
//! 来源就是泵自身的载体形态。旧实现 delta == 会话数（红），新实现 delta == 0（绿）。

use std::sync::Arc;

use qaqh_session::owner::OwnerDirectory;

/// 计数进程内 OS 线程。unix 读 `/proc/self/task`；windows 走
/// `CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD)` 按当前 pid 过滤——与
/// `crates/sbx-win/tests/probe-src/probe.rs` 同款裸 FFI，不为一个测试引新依赖。
fn os_thread_count() -> usize {
    #[cfg(unix)]
    {
        std::fs::read_dir("/proc/self/task")
            .expect("/proc/self/task readable")
            .filter_map(Result::ok)
            .count()
    }
    #[cfg(windows)]
    {
        use std::ffi::c_void;
        type Dword = u32;
        type Bool = i32;
        type Handle = *mut c_void;
        const TH32CS_SNAPTHREAD: Dword = 0x00000004;
        #[repr(C)]
        struct ThreadEntry32 {
            dw_size: Dword,
            cnt_usage: Dword,
            th32_thread_id: Dword,
            th32_owner_process_id: Dword,
            pri_class_base: i32,
            delta_pri: i32,
            dw_flags: Dword,
        }
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn CreateToolhelp32Snapshot(flags: Dword, pid: Dword) -> Handle;
            fn Thread32First(snap: Handle, entry: *mut ThreadEntry32) -> Bool;
            fn Thread32Next(snap: Handle, entry: *mut ThreadEntry32) -> Bool;
            fn CloseHandle(handle: Handle) -> Bool;
            fn GetCurrentProcessId() -> Dword;
        }
        let mut count = 0usize;
        // SAFETY: 快照句柄由本函数持有并在每个返回路径 CloseHandle；
        // THREADENTRY32 由 size_of 填 dw_size，遍历只用 Thread32* 契约内的字段。
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
            assert!(
                !snap.is_null() && (snap as isize != -1),
                "CreateToolhelp32Snapshot 失败，线程计数不可信"
            );
            let pid = GetCurrentProcessId();
            let mut entry = ThreadEntry32 {
                dw_size: std::mem::size_of::<ThreadEntry32>() as Dword,
                cnt_usage: 0,
                th32_thread_id: 0,
                th32_owner_process_id: 0,
                pri_class_base: 0,
                delta_pri: 0,
                dw_flags: 0,
            };
            if Thread32First(snap, &mut entry) != 0 {
                loop {
                    if entry.th32_owner_process_id == pid {
                        count += 1;
                    }
                    if Thread32Next(snap, &mut entry) == 0 {
                        break;
                    }
                }
            }
            CloseHandle(snap);
        }
        count
    }
    #[cfg(not(any(unix, windows)))]
    {
        compile_error!("owner_thread_budget 需要能枚举本进程 OS 线程的平台");
    }
}

/// 建 owner 前后线程数增量必须为 0：泵不得是每会话一条 OS 线程。
#[test]
fn creating_owners_does_not_add_os_threads() {
    const SESSIONS: usize = 100;

    // 计数器自检：先证明它真的能看见一条已知线程，否则 delta==0 只是瞎报零。
    let probe_before = os_thread_count();
    let hold = std::thread::spawn(|| std::thread::sleep(std::time::Duration::from_secs(30)));
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert_eq!(
        os_thread_count(),
        probe_before + 1,
        "线程计数器必须能观察到一条已知新建线程"
    );

    let dir = tempfile::tempdir().expect("tempdir");
    let owners = Arc::new(
        OwnerDirectory::open(dir.path().join("qaqh.db")).expect("open owner directory db"),
    );

    // current_thread runtime：泵若是宿主 runtime 上的任务，它跑在测试线程自己的
    // runtime 上，不额外产生 OS 线程；Db 写执行器线程在 `open` 里已建好，计入基线。
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current_thread runtime");

    // 计数必须在 owner **还活着**的时候做：泵线程随 owner drop 而退出，
    // 先释放再数只会数到一串正在退场的线程（实测把 100 稀释成个位数）。
    let (baseline, delta) = runtime.block_on(async move {
        let baseline = os_thread_count();
        let mut kept = Vec::with_capacity(SESSIONS);
        for index in 0..SESSIONS {
            kept.push(owners.get_or_create(&format!("ses_BENCH{index:03}")));
        }
        assert_eq!(kept.len(), SESSIONS, "全部会话 owner 都应建立");
        let delta = os_thread_count().saturating_sub(baseline);
        drop(kept);
        (baseline, delta)
    });

    assert_eq!(
        delta, 0,
        "创建 {SESSIONS} 个会话 owner（基线 {baseline} 线程）后进程内 OS 线程增量 {delta}\
         （旧实现每会话一条 `qaqh-owner-*` 泵线程；Actor 化后泵必须是宿主 runtime 上的任务）"
    );
    drop(hold);
}

/// 泵任务必须随 owner 掉光引用而回收（`SessionOwner::drop` → `abort`）。
///
/// 旧实现靠「sender 全部 drop ⇒ `rx.recv()` 返回 Err」退线程。泵成任务后这条
/// 链不再可靠：`ToolLifecycle` 是发给执行路径的**克隆**句柄，只要还有一个活在
/// 工具线程上，通道就没断，泵任务会永远挂在 `recv().await` 上——删一个会话
/// leak 一个任务（连同它持有的 `Arc<OwnerPump>`/`Arc<Db>`）。
///
/// owner 的真实生命周期由 `OwnerDirectory` 的 map 决定（`get_or_create` 自己
/// 也持一个 `Arc`，调用方 drop 本地句柄不触发 `Drop`），所以本用例走生产回收
/// 路径 `delete_session_tree`，并**全程留住 lifecycle 克隆句柄**：去掉 `Drop`
/// 里的 `pump_task.abort()`，这 20 个泵任务就留在计数里，本用例即红。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropped_owners_reclaim_their_pump_tasks() {
    const SESSIONS: usize = 20;
    let dir = tempfile::tempdir().expect("tempdir");
    let owners = Arc::new(
        OwnerDirectory::open(dir.path().join("qaqh.db")).expect("open owner directory db"),
    );

    // 预热一次并等回收完成,让任务计数基线稳定（runtime 惰性结构不算泄漏）。
    owners
        .ensure_session("ses_WARMUP", None, false)
        .expect("warm row");
    let warm = owners.get_or_create("ses_WARMUP");
    let warm_lifecycle = warm.lifecycle();
    drop(warm);
    owners
        .delete_session_tree("ses_WARMUP")
        .expect("warm delete");
    drop(warm_lifecycle);
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    let baseline = tokio::runtime::Handle::current()
        .metrics()
        .num_alive_tasks();

    // 建会话 + 泵任务,把 owner 从目录删掉,lifecycle 克隆句柄故意不 drop。
    let mut lingering_lifecycles = Vec::with_capacity(SESSIONS);
    let mut ids = Vec::with_capacity(SESSIONS);
    for index in 0..SESSIONS {
        let session_id = format!("ses_RECYCLE{index:02}");
        owners
            .ensure_session(&session_id, None, false)
            .expect("business row");
        let owner = owners.get_or_create(&session_id);
        lingering_lifecycles.push(owner.lifecycle());
        drop(owner);
        ids.push(session_id);
    }
    assert_eq!(lingering_lifecycles.len(), SESSIONS);
    for session_id in &ids {
        owners
            .delete_session_tree(session_id)
            .expect("delete session tree");
    }

    // 与 evicted_* 同律:abort 的 future drop 由其他 worker 调度完成,需要真实
    // CPU 时间片——纯 `yield_now` 在宿主负载抖动下只是观测竞态(本文件 evicted
    // 用例的注释即首轮红测成因)。给 250ms 有界轮询窗口:任务真死会收敛到基线;
    // 真泄漏在任何预算下都不会收敛,反证强度不变。
    let mut after;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(250);
    loop {
        after = tokio::runtime::Handle::current()
            .metrics()
            .num_alive_tasks();
        if after == baseline || tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(
        after, baseline,
        "{SESSIONS} 个 owner 回收后仍有 {}/基线 {baseline} 之外的泵任务存活：\
         lifecycle 克隆句柄还持有 sender 时,泵必须靠 Drop::abort 回收,不能只靠通道断开",
        after
    );
}

/// 闲置卸载回收（MUST 铁闸 3 / P1-2 验收「`unload_idle_sessions` 后任务句柄立即
/// drop 回收」的本体）。生产接线是 `AgentRegistry::unload_idle_sessions` 的
/// `close` 之后 `owners.take_session`——map 是 owner 的唯一长持有者（泵任务持
/// `Arc<OwnerPump>`，`ToolLifecycle` 只克隆 sender），摘出即最后一个引用。
///
/// 判据分两层，不再依赖调度运气：
/// 1. **确定性主证**：take 出的 `Arc::strong_count == 1` —— 目录之外零持有者，
///    drop 它必触发 `SessionOwner::drop` → `pump_task.abort()`。lifecycle 克隆
///    句柄全程留住（通道不断，泵不可能靠 recv-Err 自行退出——旧实现的假设）。
/// 2. **观察面**：drop 前后宿主 runtime 的 alive 任务数对比。abort 的 future
///    drop 由其他 worker 调度完成，回收需要真实 CPU 时间片（纯 `yield_now` 在
///    2 worker 机器上实测只能清空 2/20，这正是首轮红测的成因——观测竞态，
///    非回收缺失）；这里给 250ms 窗口，实测清空 20 个只需亚毫秒级调度。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn evicted_owners_reclaim_pump_tasks() {
    const SESSIONS: usize = 20;
    let dir = tempfile::tempdir().expect("tempdir");
    let owners = Arc::new(
        OwnerDirectory::open(dir.path().join("qaqh.db")).expect("open owner directory db"),
    );

    // 预热走同一入口并等回收窗口结束再取基线（与 dropped_* 同律，只是慢启动）。
    owners
        .ensure_session("ses_WARMUP", None, false)
        .expect("warm row");
    let warm = owners.get_or_create("ses_WARMUP");
    let warm_lifecycle = warm.lifecycle();
    drop(warm);
    drop(owners.take_session("ses_WARMUP").expect("warm owner"));
    drop(warm_lifecycle);
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    let baseline = tokio::runtime::Handle::current()
        .metrics()
        .num_alive_tasks();

    let mut lingering_lifecycles = Vec::with_capacity(SESSIONS);
    let mut ids = Vec::with_capacity(SESSIONS);
    for index in 0..SESSIONS {
        let session_id = format!("ses_EVICT{index:02}");
        owners
            .ensure_session(&session_id, None, false)
            .expect("business row");
        let owner = owners.get_or_create(&session_id);
        lingering_lifecycles.push(owner.lifecycle());
        drop(owner);
        ids.push(session_id);
    }

    // 主证：目录是最后持有者，strong_count==1 意味着随后的 drop 必回收泵。
    let taken: Vec<_> = ids
        .iter()
        .map(|session_id| owners.take_session(session_id))
        .collect();
    for (session_id, owner) in ids.iter().zip(taken.iter()) {
        let owner = owner.as_ref().unwrap_or_else(|| panic!("{session_id} 首次 take 必须摘出条目"));
        assert_eq!(
            Arc::strong_count(owner),
            1,
            "{session_id} 的 owner 摘出目录后必须只剩这一个引用（close 路径之外不得有长持有者）"
        );
    }
    for session_id in ids.iter().chain(std::iter::once(&"ses_WARMUP".to_string())) {
        assert!(
            owners.take_session(session_id).is_none(),
            "重复 take 是幂等 None，不是错误"
        );
    }

    // 观察面：drop 前 20 个 abort 尚未发生，任务仍在册。
    let before = tokio::runtime::Handle::current()
        .metrics()
        .num_alive_tasks();
    assert!(
        before >= baseline + SESSIONS,
        "take 出但未 drop 时泵任务必须仍存活（观察面自检：基线 {baseline}，应 >= {}，实得 {before}）",
        baseline + SESSIONS
    );

    drop(taken);
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    let after = tokio::runtime::Handle::current()
        .metrics()
        .num_alive_tasks();
    assert_eq!(
        after, baseline,
        "{SESSIONS} 个 owner drop 后仍有基线 {baseline} 之外的泵任务存活：         take 出的 Arc drop 即应触发 SessionOwner::drop → pump_task.abort()         （MUST 铁闸 3：unload 后任务句柄立即 drop 回收）"
    );

    // 与 delete_session_tree 的正交性：evict 不碰业务行，会话可原地重建。
    let revived = owners.get_or_create(&ids[0]);
    assert_eq!(revived.session_id(), ids[0]);
    drop(owners.take_session(&ids[0]));
    drop(lingering_lifecycles);
}

/// 泵在等 DB 时必须**让出宿主 worker**,而不是把它占住(Actor 化的本体主张)。
///
/// 载体是 runtime 上的任务之后,泵里再出现同步阻塞 IO(比如把 `db.read().await`
/// “优化”回 `db.read_blocking()`)就会把它所在的那条 worker 钉住;会话一多就是
/// 整台 daemon 的调度饿死——正是 L-03 的形状,只是换了个位置。
///
/// 反证(本机实测):把 `OwnerPump::active_turn_id` 的 `read().await` 换成
/// `read_blocking()` 并保持 worker_threads = 1,本用例报「计时任务迟到」而红。
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn pump_db_wait_yields_the_host_worker() {
    use std::time::Instant;

    use qaqh_ringing_contract::fact::ToolCallOutcome;
    use qaqh_session::owner::ToolSettlement;

    let dir = tempfile::tempdir().expect("tempdir");
    let owners = Arc::new(
        OwnerDirectory::open(dir.path().join("qaqh.db")).expect("open owner directory db"),
    );
    owners
        .ensure_session("SES-YIELD", None, false)
        .expect("business row");
    let owner = owners.get_or_create("SES-YIELD");
    let lifecycle = owner.lifecycle();
    // 先开回合:Settle 分支的 `active_turn_id()` 要有可归属的回合才会正常落库。
    owner.start_turn("T-YIELD", None).await.expect("start turn");

    // 确定性慢 DB 窗口：占住共享只读连接 300ms,并**等它真的占到**再继续——
    // 否则泵可能压根没撞上慢窗口,计时任务当然准点,用例退化成永远绿。
    let db = Arc::clone(owners.db());
    let (gate_tx, gate_rx) = tokio::sync::oneshot::channel::<()>();
    let blocker = tokio::task::spawn_blocking(move || {
        db.read_blocking(move |_conn| {
            // 只读连接真的在手里了,才放行后面的计时测量。
            let _ = gate_tx.send(());
            std::thread::sleep(std::time::Duration::from_millis(300));
            Ok::<_, qaqh_session::db::DbError>(())
        })
    });
    gate_rx.await.expect("blocker holds the read connection");

    // 泵任务:Settle 分支第一步就是 `active_turn_id()`(走只读连接),它会 park。
    lifecycle.settled(
        "call-yield",
        "probe",
        None,
        ToolSettlement {
            call_id: "call-yield".into(),
            tool_name: "probe".into(),
            outcome: ToolCallOutcome::Completed,
            output_text: None,
            output_truncated: false,
            diff_text: None,
            metrics: None,
            code_delta: None,
            effects: vec![],
        },
    );

    // 同一个 runtime 上的计时任务:泵若占住唯一 worker,它必然迟到。
    //
    // 这里**不能**用 `time::timeout(sleep(60ms))`:worker 被占住时 tokio 的时钟
    // 整体停摆,60ms 的 sleep 与 150ms 的 timeout 会在解冻瞬间同时就绪,
    // `select!` 随机挑一个 → 用例退化成看运气。只能拿真实墙钟量 elapsed。
    let started = Instant::now();
    tokio::time::sleep(std::time::Duration::from_millis(60)).await;
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_millis(150),
        "泵在等 DB 时占住了宿主 worker:60ms 计时任务实际 {elapsed:?} 才醒(容忍 150ms, \
         慢 DB 窗口 300ms)。泵内部对共享只读/写连接的等待必须是 `db.read/db.write` 的 \
         await 形态(等锁发生在阻塞池),不能是 `read_blocking/write_blocking`(在 worker 上等锁)",
    );

    blocker
        .await
        .expect("blocker join")
        .expect("blocker held the read connection cleanly");
    // 泵最终照常落库(不因为让出而丢写)。
    owner.flush().await.expect("flush barrier");
    let state = owner.snapshot().await.expect("snapshot");
    assert!(
        state
            .tools
            .iter()
            .any(|call| call.call_id.as_ref() == "call-yield"),
        "让出 worker 后泵仍须完成 Settle 落库"
    );
}
