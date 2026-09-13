//! BUG-2026-09-13-23：ProcessRegistry 终态条目驱逐丢 os_pid 的回归测试。
//!
//! 缺陷：`register` 的惰性驱逐按 `started`（注册时刻）计时 >600s 判定终态
//! 条目过期（`process_registry.rs:120-140`）。于是：
//! 1. 长时间运行后刚结束的进程（started 已 >600s、终态仅数秒）被当成
//!    "陈旧"条目立即驱逐 → `kill` 的按 os_pid 清理残留后代路径拿不到 os_pid，
//!    后台任务的孤儿孙进程再也无法经 harness 清理；
//! 2. 驱逐是无痕的：`get_info`/`check` 对"刚被驱逐"与"从未存在"返回同样的
//!    NOT_FOUND，模型无法区分「已结束」与「id 无效」。
//!
//! 修复预期：
//! - 驱逐门槛改为**终态时间**（终态后 600s），长跑后刚结束的条目不受影响；
//! - 驱逐前把 os_pid 与终态降级为墓碑；`kill` 命中墓碑仍可按 os_pid 清理孤儿，
//!   `get_info` 对墓碑返回 `"evicted": true`。
//!
//! PR #57 返工（reviewer 阻断 1–4）：
//! - 墓碑表**有容量上限**并在超限时按驱逐时间 FIFO 淘汰；淘汰即销毁 os_pid，
//!   此后 `kill` **不得**再按该 pid `killpg`（pid 可能已被复用，会误杀活进程组）；
//! - `os_pid == None` 的墓碑 `kill` **不得**谎报成功，须返回「无 os_pid 可清理」；
//! - 墓碑 kill 须把墓碑状态收敛为 `Killed`（subagent `RegistryRef::killed()` 依赖）；
//! - 环境相关用例须 `#[cfg(unix)]` + 运行时探测 `setsid`，不得把 `orphan_pid`
//!   当进程组 id（新会话的 pgid == 子进程 pid，但须显式断言）。

use qaqh_workspace::process_registry::{KillOutcome, ProcessRegistry};

/// `kill` 的 os_pid 路径在 unix 上走 `killpg`，误用真实进程组会连带杀掉
/// 测试进程自身，故通用用例只用一个必然不存在的 os_pid（`ESRCH` 被忽略）
/// 锁定"路径可达、返回值正确"；真实清理语义由下方 unix 专用用例承载。
fn cleanup_safe_pid() -> u32 {
    u32::MAX - 1
}

/// `ProcessRegistry` 是进程内**全局单例**，而容量上限用例会灌满墓碑表并把
/// 更早的墓碑挤出去。若与其它墓碑用例并行，会把它们的墓碑一起淘汰 → 后者
/// 拿到 `NotFound` 而 flaky。
///
/// 因此所有「依赖自己刚造的墓碑仍存活」或「主动淘汰墓碑」的用例都先取此锁
/// 串行执行（同一把锁内仍可并发，但都是同一组的用例）。
static TOMBSTONE_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 取得墓碑用例的串行锁（poison 亦继续：用例失败不应连坐其它用例）。
fn serialize_tombstone_tests() -> std::sync::MutexGuard<'static, ()> {
    TOMBSTONE_SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 验收标准：驱逐后 `process kill` 仍可按 os_pid 清理孤儿。
#[test]
fn tombstone_keeps_os_pid_for_orphan_cleanup_after_eviction() {
    let _serial = serialize_tombstone_tests();
    let id = ProcessRegistry::register("tombstone-orphan-cleanup");
    ProcessRegistry::attach_os_pid_for_test(id, cleanup_safe_pid());
    ProcessRegistry::mark_exited(id, 0);
    ProcessRegistry::age_registration_for_test(id, 3600);

    // 下一次 register 触发惰性驱逐（老实现：started >600s → 无痕移除）。
    let _trigger = ProcessRegistry::register("trigger-eviction");

    // 核心验收：kill 命中墓碑不再因条目缺失直接返回 NotFound。
    assert_eq!(
        ProcessRegistry::kill(id),
        KillOutcome::TombstoneCleaned,
        "驱逐后 process kill 仍须按 os_pid 清理孤儿（BUG-2026-09-13-23 验收标准）"
    );
}

/// `get_info` 必须区分「已驱逐（已结束）」与「id 无效」。
#[test]
fn get_info_reports_evicted_instead_of_not_found() {
    let _serial = serialize_tombstone_tests();
    let id = ProcessRegistry::register("evicted-report");
    ProcessRegistry::attach_os_pid_for_test(id, cleanup_safe_pid());
    ProcessRegistry::mark_exited(id, 3);
    ProcessRegistry::age_registration_for_test(id, 3600);
    let _trigger = ProcessRegistry::register("trigger-eviction-report");

    let info = ProcessRegistry::get_info(id).expect("墓碑必须可查询（区分已结束与 id 无效）");
    assert_eq!(
        info["evicted"], true,
        "驱逐后 get_info 必须标记 evicted: {info}"
    );
    assert_eq!(info["status"], "exited", "墓碑保留终态: {info}");
    assert_eq!(info["exit_code"], 3, "墓碑保留原 exit code: {info}");
    assert_eq!(info["id"], id, "墓碑保留 id: {info}");

    // 对照：真正不存在的 id 仍是 None。
    assert!(
        ProcessRegistry::get_info(u32::MAX).is_none(),
        "未登记过的 id 必须返回 None"
    );
    assert_eq!(
        ProcessRegistry::kill(u32::MAX),
        KillOutcome::NotFound,
        "未登记过的 id kill 必须 NotFound"
    );
}

/// 根因回归：长跑后**刚结束**的条目不得因 `started` 计时被驱逐。
#[test]
fn long_running_entry_just_finished_is_not_evicted() {
    let id = ProcessRegistry::register("long-run-then-exit");
    ProcessRegistry::attach_os_pid_for_test(id, cleanup_safe_pid());
    // 注册于 1 小时前（started 计时口径下已"过期"），但终态就在刚才。
    ProcessRegistry::age_started_only_for_test(id, 3600);
    ProcessRegistry::mark_exited(id, 0);

    let _trigger = ProcessRegistry::register("trigger-eviction-live");

    let info = ProcessRegistry::get_info(id).expect("刚结束的长跑进程必须仍在册");
    assert_eq!(info["status"], "exited", "{info}");
    assert!(
        info.get("evicted").is_none(),
        "在册条目不得带 evicted: {info}"
    );
    assert!(
        info.get("output").is_some(),
        "在册条目仍保留输出快照: {info}"
    );
}

// ── PR #57 返工：阻断 ① 墓碑容量上限 + 淘汰后不得 killpg ──────────────

/// 阻断①：墓碑表必须有容量上限（单调增长的"只进不出"只是从 entries 搬到
/// tombstones）。超过 `TOMBSTONE_CAPACITY` 后，最旧的墓碑必须被淘汰。
#[test]
fn tombstones_are_capped() {
    let _serial = serialize_tombstone_tests();
    let cap = ProcessRegistry::tombstone_capacity_for_test();
    assert!(cap > 0, "墓碑容量上限必须 > 0");

    // 造 cap + 8 个"已终态且久前注册"的条目，逐一触发驱逐。
    let mut ids = Vec::new();
    for _ in 0..(cap + 8) {
        let id = ProcessRegistry::register("cap-fill");
        ProcessRegistry::attach_os_pid_for_test(id, cleanup_safe_pid());
        ProcessRegistry::mark_exited(id, 0);
        ProcessRegistry::age_registration_for_test(id, 3600);
        ids.push(id);
    }
    let _trigger = ProcessRegistry::register("trigger-cap-eviction");

    let live = ProcessRegistry::tombstone_count_for_test();
    assert_eq!(
        live, cap,
        "墓碑数必须被封顶在 TOMBSTONE_CAPACITY={cap}，实际 {live}"
    );

    // 最旧的墓碑已被淘汰：get_info 视同 id 无效（而非报告 evicted）。
    assert!(
        ProcessRegistry::get_info(ids[0]).is_none(),
        "被容量淘汰的最旧墓碑不得再被报告为 evicted"
    );
    // 最新的墓碑仍在表内。
    assert!(
        ProcessRegistry::get_info(*ids.last().unwrap()).is_some(),
        "最新墓碑必须仍在表内"
    );
}

/// 阻断①：墓碑被容量淘汰后 **不得** 再按 os_pid `killpg`——该 pid 可能已被
/// OS 复用为别的进程组 id，误杀活进程组是数据破坏级 bug。
///
/// 用真实进程组验证：淘汰后 kill 必须返回 NotFound，且该组仍存活。
#[cfg(unix)]
#[test]
fn dropped_tombstone_is_not_killed_by_pid() {
    let _serial = serialize_tombstone_tests();
    let Some(group) = spawn_detached_sleep_group() else {
        eprintln!("skip: setsid unavailable in this environment");
        return;
    };

    // 用一个墓碑锚定该真实进程组的 pgid，随后把它挤出容量窗口。
    let anchor = ProcessRegistry::register("anchor-victim-group");
    ProcessRegistry::attach_os_pid_for_test(anchor, group.pgid);
    ProcessRegistry::mark_exited(anchor, 0);
    ProcessRegistry::age_registration_for_test(anchor, 3600);
    let _ = ProcessRegistry::register("trigger-anchor-eviction");
    assert!(
        ProcessRegistry::get_info(anchor).is_some_and(|i| i["evicted"] == true),
        "锚点必须先成为墓碑"
    );

    // 灌满墓碑表，把锚点挤出容量窗口。
    let cap = ProcessRegistry::tombstone_capacity_for_test();
    for _ in 0..(cap + 8) {
        let id = ProcessRegistry::register("cap-flood");
        ProcessRegistry::attach_os_pid_for_test(id, cleanup_safe_pid());
        ProcessRegistry::mark_exited(id, 0);
        ProcessRegistry::age_registration_for_test(id, 3600);
    }
    let _ = ProcessRegistry::register("trigger-flood-eviction");
    assert!(
        ProcessRegistry::get_info(anchor).is_none(),
        "锚点墓碑必须已被容量淘汰"
    );

    // 被淘汰后 kill：不得触达 OS（pid 复用风险），返回 NotFound。
    assert_eq!(
        ProcessRegistry::kill(anchor),
        KillOutcome::NotFound,
        "淘汰后的墓碑不得再按 os_pid 清理（禁止 killpg 复用 pid）"
    );

    // 该进程组必须仍然存活——证明没有发生 killpg 误杀。
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(
        group.is_alive(),
        "淘汰后的墓碑不得 killpg（pid 可能已被复用，误杀活进程组）"
    );

    group.cleanup();
}

// ── PR #57 返工：阻断 ② os_pid == None 不得谎报成功 ──────────────────

/// 阻断②：subagent 登记录从无 `attach_child` → 墓碑 `os_pid == None`。
/// 此时 `kill` 没有可清理对象，必须返回 `NoOsPid`，不得返回成功。
#[test]
fn tombstone_without_os_pid_reports_no_cleanup() {
    let _serial = serialize_tombstone_tests();
    let id = ProcessRegistry::register("subagent-style-no-os-pid");
    // 不 attach_child、不 attach_os_pid_for_test —— subagent 的登记形态。
    ProcessRegistry::mark_exited(id, 0);
    ProcessRegistry::age_registration_for_test(id, 3600);
    let _trigger = ProcessRegistry::register("trigger-eviction-no-os-pid");

    let info = ProcessRegistry::get_info(id).expect("墓碑必须在册");
    assert_eq!(info["evicted"], true, "{info}");

    assert_eq!(
        ProcessRegistry::kill(id),
        KillOutcome::NoOsPid,
        "无 os_pid 的墓碑 kill 必须如实返回「无 os_pid 可清理」，不得谎报成功"
    );
}

/// 对照：在册且无 os_pid 的条目（同样未 attach_child）kill 仍应成功——
/// 它没有孤儿要清理，状态收敛本身即完成（语义与墓碑不同）。
#[test]
fn in_place_kill_without_os_pid_still_succeeds() {
    let id = ProcessRegistry::register("in-place-no-os-pid");
    assert_eq!(
        ProcessRegistry::kill(id),
        KillOutcome::Killed,
        "在册条目 kill 应成功"
    );
    assert_eq!(
        ProcessRegistry::get_info(id).expect("仍在册")["status"],
        "killed"
    );
}

// ── PR #57 返工：阻断 ③ 真实 kill 路径状态一致性 ─────────────────────

/// 阻断③：`kill_tombstoned` 必须把墓碑状态收敛为 `Killed`。
/// subagent 的 `RegistryRef::killed()` 只看 `status == "killed"`，
/// 若墓碑 kill 后仍报 `exited`，子代理永远检测不到 kill 请求。
#[test]
fn kill_tombstoned_converges_status_to_killed() {
    let _serial = serialize_tombstone_tests();
    let id = ProcessRegistry::register("tombstone-kill-status");
    ProcessRegistry::attach_os_pid_for_test(id, cleanup_safe_pid());
    ProcessRegistry::mark_exited(id, 7);
    ProcessRegistry::age_registration_for_test(id, 3600);
    let _trigger = ProcessRegistry::register("trigger-status-eviction");
    assert_eq!(
        ProcessRegistry::get_info(id).expect("墓碑")["status"],
        "exited",
        "驱逐时保留终态"
    );

    assert_eq!(
        ProcessRegistry::kill(id),
        KillOutcome::TombstoneCleaned,
        "墓碑 kill 应走 os_pid 清理路径"
    );

    let after = ProcessRegistry::get_info(id).expect("墓碑 kill 后仍可查询");
    assert_eq!(
        after["status"], "killed",
        "墓碑 kill 后状态必须收敛为 killed，否则 subagent RegistryRef::killed() 永远读不到: {after}"
    );
    assert_eq!(
        after["exit_code"], 7,
        "墓碑 kill 不得丢失原 exit code: {after}"
    );
    assert_eq!(after["evicted"], true, "仍应标记 evicted: {after}");
}

/// 阻断③比照：在册 kill 与墓碑 kill 对 `status` 的表述必须一致（同为 killed），
/// 否则调用方（subagent / process check）需按"条目是否已驱逐"分支判断。
#[test]
fn in_place_kill_and_tombstone_kill_agree_on_status() {
    let _serial = serialize_tombstone_tests();
    let live = ProcessRegistry::register("live-kill-status");
    ProcessRegistry::attach_os_pid_for_test(live, cleanup_safe_pid());
    ProcessRegistry::mark_exited(live, 5);
    assert_eq!(ProcessRegistry::kill(live), KillOutcome::Killed);
    let live_status = ProcessRegistry::get_info(live).expect("在册")["status"].clone();

    let evicted = ProcessRegistry::register("evicted-kill-status");
    ProcessRegistry::attach_os_pid_for_test(evicted, cleanup_safe_pid());
    ProcessRegistry::mark_exited(evicted, 5);
    ProcessRegistry::age_registration_for_test(evicted, 3600);
    let _trigger = ProcessRegistry::register("trigger-status-parity");
    assert_eq!(
        ProcessRegistry::kill(evicted),
        KillOutcome::TombstoneCleaned
    );
    let evicted_status = ProcessRegistry::get_info(evicted).expect("墓碑")["status"].clone();

    assert_eq!(
        live_status, evicted_status,
        "在册 kill 与墓碑 kill 的状态表述必须一致（均为 killed）"
    );
    assert_eq!(live_status, "killed");
}

// ── 端到端（unix）：真实孤儿进程组清理 ──────────────────────────────

/// 为何需要运行时探测：本测试依赖 `setsid(1)` 建新会话/新进程组。容器或精简
/// 镜像可能没有该二进制。若无 `setsid` 必须 **skip**，绝不能 panic（阻断④）。
fn spawn_detached_sleep_group() -> Option<SleepGroup> {
    let probe = std::process::Command::new("setsid")
        .arg("--help")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    // `setsid --help` 在 util-linux 上退出码 0；某些实现 --help 非 0 但命令存在。
    match probe {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            eprintln!("skip: setsid probe failed: {e}");
            return None;
        }
    }

    let mut child = std::process::Command::new("setsid")
        .args(["sleep", "120"])
        .spawn()
        .ok()?;
    let pid = child.id();
    // setsid(1) 建新会话：新会话首进程即组长，pgid == 自身 pid。
    // 注意 spawn 返回时子进程尚未 exec/lsetsid，须轮询等待新组生效（否则会
    // 读到父进程组的 pgid 而误判 "环境不支持"）。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let mut pgid = -1;
    while std::time::Instant::now() < deadline {
        pgid = unsafe { libc::getpgid(pid as i32) };
        if pgid > 0 && pgid as u32 == pid {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    if pgid < 0 || pgid as u32 != pid {
        let _ = child.kill();
        let _ = child.wait();
        eprintln!("skip: setsid did not create a new process group (pgid={pgid}, pid={pid})");
        return None;
    }
    Some(SleepGroup {
        child,
        pid,
        pgid: pid,
    })
}

/// 真实孤儿进程组句柄（setsid sleep），`Drop` 兜底回收避免测试泄漏。
struct SleepGroup {
    child: std::process::Child,
    pid: u32,
    /// 进程组 id（== 会话首进程 pid）；登记 os_pid 必须用它，而非裸 pid。
    pgid: u32,
}

impl SleepGroup {
    fn is_alive(&self) -> bool {
        // kill(pid, 0) == 0 → 进程仍存在；EPERM 亦说明存在（非本测试场景）。
        unsafe { libc::kill(self.pid as i32, 0) == 0 }
    }

    fn cleanup(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for SleepGroup {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// 端到端（unix）：驱逐后按 os_pid 真能杀掉残留的孤儿进程组。
///
/// 用 `setsid`（新会话 = 新进程组、组长 = 该子进程）造一个真实可被
/// `killpg` 命中的组，再把它登记为某条目的 os_pid，走完"终态 → 驱逐 →
/// 墓碑 kill"全链，断言该组确实被终止。
#[cfg(unix)]
#[test]
fn evicted_tombstone_kill_really_terminates_orphan_group() {
    let _serial = serialize_tombstone_tests();
    let Some(mut group) = spawn_detached_sleep_group() else {
        eprintln!("skip: setsid unavailable in this environment");
        return;
    };

    let id = ProcessRegistry::register("real-orphan-group");
    // 登记**真实 pid**（== 新会话 pgid），而非拿 orphan_pid 当组 id 猜。
    ProcessRegistry::attach_os_pid_for_test(id, group.pgid);
    ProcessRegistry::mark_exited(id, 0);
    ProcessRegistry::age_registration_for_test(id, 3600);
    let _trigger = ProcessRegistry::register("trigger-eviction-real");

    let info = ProcessRegistry::get_info(id).expect("墓碑必须在册");
    assert_eq!(info["evicted"], true, "{info}");

    assert_eq!(
        ProcessRegistry::kill(id),
        KillOutcome::TombstoneCleaned,
        "驱逐后杀孤儿进程组必须成功（入口不再是 NOT_FOUND）"
    );

    // 等待被 SIGKILL 的组消亡；轮询避免 flaky。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut reaped = false;
    while std::time::Instant::now() < deadline {
        if group.child.try_wait().ok().flatten().is_some() {
            reaped = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    group.cleanup();
    assert!(reaped, "墓碑 kill 必须真正终止孤儿进程组");
}
