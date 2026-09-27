//! BUG-2026-09-12-05 回归：actor 线程设置的会话工作区必须对派生工具线程可见。
//!
//! 事故模型（docs/archive/2026-09/report/2026-09-12-会话cwd未传导工具线程grep越界-report.md）：
//! `dae42c7` 让 actor 上下文跳过物理 cd 后，会话 cwd 只存在于 actor 线程 TLS；
//! 工具线程的 `current_workspace()` 回退到恒空的进程全局，相对路径全部锚到
//! daemon 进程 cwd。修复 = `ActorToolScope` 携带 workspace 快照跨线程搬运。
//!
//! 测试只触碰 thread-local 槽位（每个测试独立线程），无进程全局共享状态。

use qaqh_workspace::{
    clear_actor_context, current_workspace, pop_thread_workspace, push_thread_workspace,
    runtime::ActorToolScope, set_actor_context,
};

/// 在"actor 线程"上设置工作区，随后在派生 OS 线程（工具线程同构）上读取。
/// 修复前：派生线程读到空/进程全局，断言失败；修复后：ActorToolScope 搬运。
#[test]
fn spawned_tool_thread_sees_actor_workspace_via_scope() {
    let ws_a = tempfile::tempdir().unwrap();
    let session_id = "bug05-scope-thread";
    let before = std::env::current_dir().unwrap();

    // 模拟 actor 线程：安装 actor context + 会话工作区
    let actor_ws = ws_a.path().to_string_lossy().into_owned();
    let actor_handle = {
        let actor_ws = actor_ws.clone();
        std::thread::Builder::new()
            .name("bug05-actor".into())
            .spawn(move || {
                set_actor_context(&actor_ws, session_id);
                let scope = ActorToolScope::capture();

                // actor 线程派生"工具线程"，capture 后 install 跨线程搬运
                let handle = std::thread::Builder::new()
                    .name("bug05-tool".into())
                    .spawn(move || {
                        let _guard = scope.install();
                        current_workspace()
                    })
                    .unwrap();
                let tool_thread_view = handle.join().unwrap();
                clear_actor_context();
                tool_thread_view
            })
            .unwrap()
    };
    let tool_thread_view = actor_handle.join().unwrap();

    assert_eq!(
        tool_thread_view, actor_ws,
        "工具线程必须看到 actor 的会话工作区（BUG-05 断言）"
    );

    // restore：非 actor 物理状态不受影响（与 dae42c7 防漂移语义共存）。
    let _ = std::env::set_current_dir(before);
}

/// install 的 restore 语义：guard drop 后工具线程应回到自身原值（对称性）。
#[test]
fn scope_guard_restores_previous_thread_workspace_on_drop() {
    let ws_a = tempfile::tempdir().unwrap();
    let ws_b = tempfile::tempdir().unwrap();
    let a = ws_a.path().to_string_lossy().into_owned();
    let b = ws_b.path().to_string_lossy().into_owned();

    let handle = {
        let a = a.clone();
        let b = b.clone();
        std::thread::Builder::new()
            .name("bug05-restore".into())
            .spawn(move || {
                // 工具线程自身原有工作区 B。
                let previous = push_thread_workspace(Some(b.clone()));
                // 在 A 上 capture（scope 工作区 = A），随后把线程值还原为 B，
                // 使 guard drop 的恢复目标可观测。
                let scope = {
                    push_thread_workspace(Some(a.clone()));
                    let scope = ActorToolScope::capture();
                    pop_thread_workspace(Some(b.clone()));
                    scope
                };
                let during_install;
                {
                    let _guard = scope.install();
                    during_install = current_workspace();
                }
                let after_drop = current_workspace();
                pop_thread_workspace(previous);
                (during_install, after_drop)
            })
            .unwrap()
    };
    let (during_install, after_drop) = handle.join().unwrap();

    assert_eq!(during_install, a, "install 期间应为 scope 工作区 A");
    assert_eq!(
        after_drop, b,
        "guard drop 后必须恢复线程原 workspace B（restore 对称性）"
    );
}
