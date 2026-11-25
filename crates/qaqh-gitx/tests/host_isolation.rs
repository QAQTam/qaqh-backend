//! P2-1b 反证：`GitxHost` 随工具实例构造，同一进程两套宿主互不串台。
//! 旧全局槽实现下第二次 `install_host` 被 `OnceLock` 静默忽略、且工具读取
//! 的是唯一进程级槽——宿主 B 的 worktree 只会建进宿主 A 的仓库、B 也收不到
//! 任何调用记录，本文件必红。新实现：宿主经 `register`/构造器进入工具实例。

#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use qaqh_gitx::tools::{GitxHost, WorktreeCreateArgs, WorktreeCreateTool, WorktreeListTool};
use qaqh_tool_core::tool_api::{
    SandboxMode, SandboxSpec, ToolCallContext, ToolCallSource, TypedTool,
};
use qaqh_workspace::tool_api::CancellationToken;
use qaqh_workspace::{PermissionLevel, ToolManager};

struct RecordingHost {
    root: PathBuf,
    calls: Mutex<Vec<String>>,
}

impl GitxHost for RecordingHost {
    fn source_workspace(&self, session_id: &str) -> Option<PathBuf> {
        self.calls.lock().unwrap().push(session_id.to_string());
        Some(self.root.clone())
    }
}

fn init_repo(path: &Path) {
    std::fs::create_dir_all(path).expect("mkdir repo");
    let ok = |args: &[&str]| {
        let status = Command::new("git")
            .current_dir(path)
            .args([
                "-c",
                "user.email=iso@test",
                "-c",
                "user.name=iso@test",
                "-c",
                "core.autocrlf=false",
            ])
            .args(args)
            .status()
            .expect("git on PATH");
        assert!(status.success(), "git {args:?} failed");
    };
    ok(&["init", "-q"]);
    std::fs::write(path.join("README.md"), "iso\n").expect("seed file");
    ok(&["add", "."]);
    ok(&["commit", "-q", "-m", "init"]);
}

fn model_ctx(session_id: &str) -> ToolCallContext {
    ToolCallContext {
        call_id: "call-iso".to_string(),
        session_id: session_id.to_string(),
        workspace_root: PathBuf::from("/tmp/qaqh-gitx-iso"),
        permission_level: PermissionLevel::WorkspaceWrite,
        sandbox: SandboxMode::Main,
        sandbox_spec: SandboxSpec::disabled(),
        exec_default_shell: None,
        exec_max_output_chars: None,
        timeout: Duration::from_secs(30),
        cancellation: CancellationToken::new(),
        progress: None,
        exec_progress: None,
        exec_host: None,
        official_web: None,
        source: ToolCallSource::Model,
    }
}

#[test]
fn two_gitx_hosts_route_per_instance_under_concurrency() {
    let temp_root = tempfile::tempdir().expect("tempdir");
    let host_a = Arc::new(RecordingHost {
        root: temp_root.path().join("iso-root-A"),
        calls: Mutex::new(Vec::new()),
    });
    let host_b = Arc::new(RecordingHost {
        root: temp_root.path().join("iso-root-B"),
        calls: Mutex::new(Vec::new()),
    });
    init_repo(&host_a.root);
    init_repo(&host_b.root);

    let dyn_a: Arc<dyn GitxHost> = host_a.clone();
    let dyn_b: Arc<dyn GitxHost> = host_b.clone();

    // 两套 ToolManager 各自注册——词表齐全，宿主随注册期注入。
    let mut mgr_a = ToolManager::new();
    qaqh_gitx::register(&mut mgr_a, Arc::clone(&dyn_a));
    let mut mgr_b = ToolManager::new();
    qaqh_gitx::register(&mut mgr_b, Arc::clone(&dyn_b));
    for name in [
        "worktree_list",
        "worktree_create",
        "worktree_bind",
        "worktree_unbind",
        "worktree_remove",
    ] {
        assert!(mgr_a.builtin(name).is_some(), "A manager missing {name}");
        assert!(mgr_b.builtin(name).is_some(), "B manager missing {name}");
    }

    let workers: Vec<_> = [
        ("ses-a", "A", Arc::clone(&dyn_a)),
        ("ses-b", "B", Arc::clone(&dyn_b)),
    ]
    .into_iter()
    .map(|(session, tag, host)| {
        std::thread::spawn(move || {
            // 判据 1：list 走自己宿主（fresh 仓库无 managed 条目 → 空表
            // 且调用记录落在自己的宿主上——旧全局槽下 B 的调用会打到 A）。
            let listed = WorktreeListTool {
                host: Arc::clone(&host),
            }
            .run(&model_ctx(session), qaqh_gitx::tools::WorktreeListArgs {})
            .unwrap_or_else(|error| panic!("host-{tag} list must succeed: {error:?}"));
            assert!(listed.result.worktrees.is_empty(), "fresh repo has no managed entry");
            // 数据面判据 2：create 的 worktree 落在自己宿主的仓库下。
            let args: WorktreeCreateArgs = serde_json::from_value(serde_json::json!({
                "branch_name": format!("b-{tag}")
            }))
            .expect("args");
            let created = WorktreeCreateTool { host }
                .run(&model_ctx(session), args)
                .unwrap_or_else(|error| panic!("host-{tag} create must succeed: {error:?}"));
            assert!(
                created.result.worktree.path.contains(&format!("iso-root-{tag}")),
                "host-{tag} worktree must live under its own source root: {}",
                created.result.worktree.path
            );
        })
    })
    .collect();
    for worker in workers {
        worker.join().expect("worker");
    }

    assert_eq!(
        *host_a.calls.lock().unwrap(),
        vec!["ses-a".to_string(), "ses-a".to_string()],
        "host A must have served only its own session (list+create)"
    );
    assert_eq!(
        *host_b.calls.lock().unwrap(),
        vec!["ses-b".to_string(), "ses-b".to_string()],
        "host B must have served only its own session (list+create)"
    );
}
