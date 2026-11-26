use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use qaqh_workspace::authorization::{Admission, ToolInvocation, admit_with_context};
use qaqh_workspace::execution::execute_authorized_with_context;
use qaqh_workspace::permission::{PermissionLevel, ToolCategory};
use qaqh_workspace::registration::build_tool_manager;
use qaqh_workspace::todo::MemoryTodoHost;
use qaqh_workspace::tool_api::{CancellationToken, SandboxMode, SandboxSpec, ToolCallContext, ToolCallSource};
use qaqh_workspace::tool_side_fold::StandardPolicy;

fn ctx_for(ws: PathBuf) -> ToolCallContext {
    let level = PermissionLevel::SkipPermissions;
    ToolCallContext {
        call_id: "call_safety".into(),
        session_id: "sess_safety".into(),
        workspace_root: ws.clone(),
        permission_level: level,
        sandbox: SandboxMode::Main,
        sandbox_spec: SandboxSpec::for_permission(ws, level, false),
        exec_default_shell: None,
        exec_max_output_chars: None,
        timeout: Duration::from_secs(10),
        cancellation: CancellationToken::new(),
        progress: None,
        exec_progress: None,
        exec_host: None,
        official_web: None,
        source: ToolCallSource::Model,
    }
}

fn run_apply_patch(
    mgr: &Mutex<qaqh_workspace::ToolManager>,
    ws: &std::path::Path,
    patch: &str,
    dry_run: bool,
) -> qaqh_workspace::execution::ToolExecResult {
    let ctx = ctx_for(ws.to_path_buf());
    let inv = ToolInvocation {
        session_id: "sess_safety".into(),
        call_id: "call_safety".into(),
        tool_name: "apply_patch".into(),
        args: serde_json::json!({ "input": patch, "dry_run": dry_run }),
        category: ToolCategory::Write,
    };
    let Admission::Authorized(call) = admit_with_context(inv, &ctx, &HashSet::new()) else {
        panic!("SkipPermissions must authorize at admission so SafetyPolicy acts as last gate");
    };
    execute_authorized_with_context(call, ctx, None, mgr, &StandardPolicy)
}

#[test]
fn apply_patch_delete_outside_workspace_is_blocked_by_safety_policy() {
    let dir = tempfile::tempdir().expect("tempdir");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).expect("create ws");
    let outside_rel = dir.path().join("outside_rel.txt");
    let outside_abs = dir.path().join("outside_abs.txt");
    let inside = ws.join("inside.txt");
    std::fs::write(&outside_rel, "keep rel\n").expect("write outside_rel");
    std::fs::write(&outside_abs, "keep abs\n").expect("write outside_abs");
    std::fs::write(&inside, "delete me\n").expect("write inside");

    let mgr = Mutex::new(build_tool_manager(&[], Arc::new(MemoryTodoHost::default())));

    for dry_run in [false, true] {
        let rel_patch = "*** Begin Patch\n*** Delete File: ../outside_rel.txt\n*** End Patch";
        let res = run_apply_patch(&mgr, &ws, rel_patch, dry_run);
        assert!(!res.success, "relative delete escape must be blocked (dry_run={dry_run}): {}", res.content);
        assert!(
            res.content.contains("Destructive operation outside workspace is blocked"),
            "expected SafetyPolicy block message, got: {}",
            res.content
        );
        assert!(outside_rel.exists(), "outside_rel.txt must not be deleted");

        let abs_patch = format!(
            "*** Begin Patch\n*** Delete File: {}\n*** End Patch",
            outside_abs.display()
        );
        let res_abs = run_apply_patch(&mgr, &ws, &abs_patch, dry_run);
        assert!(!res_abs.success, "absolute delete escape must be blocked (dry_run={dry_run}): {}", res_abs.content);
        assert!(
            res_abs.content.contains("Destructive operation outside workspace is blocked"),
            "expected SafetyPolicy block message, got: {}",
            res_abs.content
        );
        assert!(outside_abs.exists(), "outside_abs.txt must not be deleted");
    }

    // Inside-workspace delete still works normally.
    let inside_patch = "*** Begin Patch\n*** Delete File: inside.txt\n*** End Patch";
    let ok_inside = run_apply_patch(&mgr, &ws, inside_patch, false);
    assert!(ok_inside.success, "inside delete must succeed: {}", ok_inside.content);
    assert!(!inside.exists(), "inside.txt must be deleted");

    // Non-destructive add outside workspace stays allowed under SkipPermissions
    // (ownership remains with admission layer, not SafetyPolicy).
    let add_outside = "*** Begin Patch\n*** Add File: ../added_outside.txt\n+ok\n*** End Patch";
    let ok_add = run_apply_patch(&mgr, &ws, add_outside, false);
    assert!(ok_add.success, "outside add under SkipPermissions stays allowed: {}", ok_add.content);
    assert!(dir.path().join("added_outside.txt").exists());
}
