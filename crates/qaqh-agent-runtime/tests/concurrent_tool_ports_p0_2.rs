//! P0-2 并发探针：8 线程同时持有不同 `session_id` + `workspace_root` 的
//! `WorkspaceToolPort`，并发执行 `read` / `edit` / `apply_patch` / `todo_write`。
//!
//! 验收判据（MUST.md §铁闸 3 #2 & PLAN_P0-2）：
//! 1. 零 `STALE_FILE` 误判（每个会话各自持有独立 `FileRuntime`）；
//! 2. 零跨会话工作区/todo/账本串台；
//! 3. 不依赖任何 `TEST_RUNTIME_SERIAL` 全局串行锁或 `thread_local!` 视图。

#![allow(clippy::unwrap_used)]

use std::sync::{
    Arc, Barrier, Mutex,
};

use qaqh_agent_runtime::ExecSessionManager;
use qaqh_agent_runtime::tool_job::{ToolPolicySnapshot, ToolRequest, WorkspaceToolPort};
use qaqh_session::owner::OwnerDirectory;

fn call_port(
    port: &WorkspaceToolPort,
    turn_id: &str,
    call_id: &str,
    tool_name: &str,
    read_only: bool,
    args: serde_json::Value,
) -> String {
    let req = ToolRequest {
        execution_id: format!("exec-{call_id}"),
        call_id: call_id.to_string(),
        turn_id: turn_id.to_string(),
        tool_name: tool_name.to_string(),
        read_only,
        args: serde_json::to_vec(&args).expect("serialize args"),
    };
    port.execute(&req, qaqh_workspace::tool_api::CancellationToken::new())
        .unwrap_or_else(|err| panic!("{tool_name} ({call_id}) failed: {err:?}"))
        .text
}

#[test]
fn eight_threads_concurrent_tool_ports_never_cross_contaminate() {
    let temp_root = tempfile::tempdir().expect("tempdir");
    let data_dir = temp_root.path().join("data");
    std::fs::create_dir_all(&data_dir).expect("mkdir data");

    // SAFETY: 本文件是独立集成测试二进制，仅此单测在启动各工作线程前设置一次
    // 隔离数据根，确保 audit/todo 不写入用户真实目录。
    unsafe {
        std::env::set_var("QAQH_DATA_DIR", &data_dir);
    }

    let owners = Arc::new(
        OwnerDirectory::open(data_dir.join("qaqh.db")).expect("open OwnerDirectory"),
    );
    let exec_sessions = Arc::new(ExecSessionManager::new());
    let barrier = Arc::new(Barrier::new(8));

    std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(8);
        for idx in 0..8usize {
            let owners = Arc::clone(&owners);
            let exec_sessions = Arc::clone(&exec_sessions);
            let barrier = Arc::clone(&barrier);
            let root = temp_root.path().to_path_buf();
            handles.push(scope.spawn(move || {
                let session_id = format!("p02-concurrent-{idx}");
                let ws = root.join(format!("ws-{idx}"));
                std::fs::create_dir_all(&ws).expect("mkdir ws");
                std::fs::write(
                    ws.join("target.txt"),
                    format!("session-{idx}-v1\ncommon-tail\n"),
                )
                .expect("write target");
                std::fs::write(
                    ws.join("patch.txt"),
                    format!("patch-{idx}-alpha\nkeep-line\n"),
                )
                .expect("write patch");

                let owner = owners.get_or_create(&session_id);
                let port = WorkspaceToolPort::new(
                    &session_id,
                    Arc::new(Mutex::new(qaqh_workspace::registration::build_tool_manager(
                        &[],
                    ))),
                    ToolPolicySnapshot::workspace_write(&ws),
                    owner.lifecycle(),
                    exec_sessions,
                );

                // 1. 8 线程齐步走：相对路径 read（验证 workspace_root 零串台）
                barrier.wait();
                let read_out = call_port(
                    &port,
                    "turn-1",
                    &format!("read-{idx}"),
                    "read",
                    true,
                    serde_json::json!({ "path": "target.txt" }),
                );
                assert!(
                    read_out.contains(&format!("session-{idx}-v1")),
                    "thread {idx} read wrong workspace: {read_out}"
                );
                for other in 0..8usize {
                    if other != idx {
                        assert!(
                            !read_out.contains(&format!("session-{other}-v1")),
                            "thread {idx} leaked thread {other} workspace: {read_out}"
                        );
                    }
                }

                // 2. 8 线程齐步走：edit（依赖各自 FileRuntime 的 read 基线，零 STALE_FILE）
                barrier.wait();
                let edit_out = call_port(
                    &port,
                    "turn-1",
                    &format!("edit-{idx}"),
                    "edit",
                    false,
                    serde_json::json!({
                        "path": "target.txt",
                        "old_str": format!("session-{idx}-v1"),
                        "new_str": format!("session-{idx}-v2"),
                    }),
                );
                assert!(
                    !edit_out.contains("stale_file"),
                    "thread {idx} hit false STALE_FILE: {edit_out}"
                );

                // 3. 8 线程齐步走：apply_patch + 紧随其后的 edit（账本同步，零 STALE_FILE）
                barrier.wait();
                let patch = format!(
                    "*** Begin Patch\n*** Update File: patch.txt\n@@\n-patch-{idx}-alpha\n+patch-{idx}-beta\n*** End Patch\n"
                );
                call_port(
                    &port,
                    "turn-1",
                    &format!("patch-{idx}"),
                    "apply_patch",
                    false,
                    serde_json::json!({ "input": patch }),
                );
                call_port(
                    &port,
                    "turn-1",
                    &format!("edit-after-patch-{idx}"),
                    "edit",
                    false,
                    serde_json::json!({
                        "path": "patch.txt",
                        "old_str": format!("patch-{idx}-beta"),
                        "new_str": format!("patch-{idx}-gamma"),
                    }),
                );

                // 4. 8 线程齐步走：todo_write（会话显式隔离）
                barrier.wait();
                call_port(
                    &port,
                    "turn-1",
                    &format!("todo-{idx}"),
                    "todo_write",
                    false,
                    serde_json::json!({
                        "list": format!("- [~] task-{idx} | owned by {idx}"),
                    }),
                );

                // 5. 最终落盘校验：每条会话工作区与 todo 账本完全独立
                let target_disk = std::fs::read_to_string(ws.join("target.txt")).unwrap();
                assert_eq!(
                    target_disk.replace("\r\n", "\n"),
                    format!("session-{idx}-v2\ncommon-tail\n")
                );
                let patch_disk = std::fs::read_to_string(ws.join("patch.txt")).unwrap();
                assert_eq!(
                    patch_disk.replace("\r\n", "\n"),
                    format!("patch-{idx}-gamma\nkeep-line\n")
                );
                let todo_store = qaqh_workspace::todo::load_todo_for(&session_id).unwrap();
                assert_eq!(todo_store.items.len(), 1);
                assert_eq!(todo_store.items[0].title, format!("task-{idx}"));
            }));
        }
        for handle in handles {
            handle.join().expect("worker thread panicked");
        }
    });
}
