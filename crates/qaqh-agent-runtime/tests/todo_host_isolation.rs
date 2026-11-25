//! P2-1b 反证：todo 工具面的 `TodoHost` 随注册期注入 `ToolManager`，
//! 同一测试进程内两套宿主并发服务互不串台。
//!
//! 旧实现（`qaqh-workspace::todo::install_host` 全局 `OnceLock` 槽 +
//! `install_memory_host` 夹具）在本文件必红：第二次安装被静默忽略，两个
//! port 的 `todo_write` 全部落到同一份内存存储——B 侧断言（只见到自己的
//! 清单、A 侧 items 计数）逐一失败。新实现：`build_tool_manager` 在构造期
//! 显式接收 `Arc<dyn TodoHost>`，无进程级安装步骤，也无需任何测试串行锁。

#![allow(clippy::unwrap_used)]

use std::sync::{Arc, Barrier};

use qaqh_agent_runtime::ExecSessionManager;
use qaqh_agent_runtime::tool_job::{ToolPolicySnapshot, ToolRequest, WorkspaceToolPort};
use qaqh_session::owner::OwnerDirectory;
use qaqh_workspace::todo::{MemoryTodoHost, TodoHost};

fn call_todo_write(port: &WorkspaceToolPort, session_id: &str, text: &str) -> String {
    let req = ToolRequest {
        execution_id: format!("exec-{session_id}"),
        call_id: format!("call-{session_id}"),
        turn_id: format!("turn-{session_id}"),
        tool_name: "todo_write".to_string(),
        read_only: false,
        args: serde_json::to_vec(&serde_json::json!({ "list": text })).expect("args"),
    };
    port.execute(&req, qaqh_workspace::tool_api::CancellationToken::new())
        .unwrap_or_else(|err| panic!("todo_write({session_id}) failed: {err:?}"))
        .text
}

#[test]
fn two_todo_hosts_never_cross_contaminate() {
    let temp_root = tempfile::tempdir().expect("tempdir");
    let owners = Arc::new(
        OwnerDirectory::open(temp_root.path().join("qaqh.db")).expect("open OwnerDirectory"),
    );
    let exec_sessions = Arc::new(ExecSessionManager::new());

    let host_a: Arc<dyn TodoHost> = Arc::new(MemoryTodoHost::default());
    let host_b: Arc<dyn TodoHost> = Arc::new(MemoryTodoHost::default());
    let barrier = Arc::new(Barrier::new(2));
    let sessions: Vec<(String, Arc<dyn TodoHost>)> = vec![
        ("iso-A".to_string(), Arc::clone(&host_a)),
        ("iso-B".to_string(), Arc::clone(&host_b)),
    ];
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for (session_id, host) in sessions {
            let owners = Arc::clone(&owners);
            let exec_sessions = Arc::clone(&exec_sessions);
            let barrier = Arc::clone(&barrier);
            let root = temp_root.path().to_path_buf();
            handles.push(scope.spawn(move || {
                let ws = root.join(format!("ws-{session_id}"));
                std::fs::create_dir_all(&ws).expect("mkdir ws");
                let owner = owners.get_or_create(&session_id);
                let port = WorkspaceToolPort::new(
                    &session_id,
                    Arc::new(std::sync::Mutex::new(
                        qaqh_workspace::registration::build_tool_manager(&[], Arc::clone(&host)),
                    )),
                    ToolPolicySnapshot::workspace_write(&ws),
                    owner.lifecycle(),
                    exec_sessions,
                );
                barrier.wait();
                for round in 0..6 {
                    let out = call_todo_write(
                        &port,
                        &session_id,
                        &format!("- [ ] {session_id} task {round}"),
                    );
                    assert!(
                        out.contains(&format!("{session_id} task {round}")),
                        "输出必须携带本会话宿主的清单: {out}"
                    );
                }
            }));
        }
        for handle in handles {
            handle.join().expect("worker thread panicked");
        }
    });

    let store_a = qaqh_workspace::todo::load_todo_for(&*host_a, "iso-A").expect("load A");
    let store_b = qaqh_workspace::todo::load_todo_for(&*host_b, "iso-B").expect("load B");
    assert_eq!(store_a.items.len(), 1, "todo_write 全量覆写：A 只剩最后一轮");
    assert_eq!(store_a.items[0].title, "iso-A task 5");
    assert_eq!(store_b.items.len(), 1);
    assert_eq!(store_b.items[0].title, "iso-B task 5");
    // 旧全局槽串台的直接判据：任一宿主里都不得出现对方的会话键。
    assert!(
        host_b.load_todo("iso-A").expect("probe B/A").is_none(),
        "host B must never hold session iso-A's list"
    );
    assert!(
        host_a.load_todo("iso-B").expect("probe A/B").is_none(),
        "host A must never hold session iso-B's list"
    );
}
