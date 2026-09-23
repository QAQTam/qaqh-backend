//! P4 A1: audit intent barrier for high-risk tools.
//!
//! `write` / `exec` / `net` must durably record a v2 `tool_intent` before the
//! handler is dispatched. If that intent write fails, the handler must not run
//! and the call must fail with `AUDIT_UNAVAILABLE`.

use std::path::PathBuf;
use std::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::new(());

struct Env {
    _tmp: tempfile::TempDir,
    workspace: PathBuf,
}

fn setup(label: &str) -> Env {
    let tmp = tempfile::tempdir().expect("tempdir");
    let data = tmp.path().join(format!("{label}-data"));
    let workspace = tmp.path().join(format!("{label}-ws"));
    std::fs::create_dir_all(&data).expect("create data dir");
    std::fs::create_dir_all(&workspace).expect("create workspace");
    // Force the v2 append path to fail: v2.jsonl is a directory, not a file.
    std::fs::create_dir_all(data.join("audit").join("v2.jsonl")).expect("block v2 path");
    // SAFETY: this test file serializes all env/global runtime setup.
    unsafe {
        std::env::set_var("QAQH_DATA_DIR", &data);
        std::env::set_var("HOME", tmp.path());
        std::env::set_var("USERPROFILE", tmp.path());
    }
    qaqh_workspace::set_workspace(&workspace.to_string_lossy());
    qaqh_workspace::runtime::init_tools(label, &[], vec![]);
    qaqh_workspace::runtime::set_context(label, 4);
    Env {
        _tmp: tmp,
        workspace,
    }
}

fn call(
    name: &str,
    args: serde_json::Value,
    call_id: &str,
    ctx: &qaqh_workspace::runtime::ToolCtx,
) -> qaqh_workspace::execution::ToolExecResult {
    qaqh_workspace::execution::execute_with_context(name, "", &args.to_string(), call_id, None, ctx)
}

#[test]
fn audit_intent_failure_blocks_write_before_side_effect() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let env = setup("audit-intent-block");
    let ctx = qaqh_workspace::runtime::ToolCtx::admitted("audit-intent-block");
    let target = env.workspace.join("must-not-exist.txt");

    let result = call(
        "write",
        serde_json::json!({"path": target, "content": "side effect"}),
        "call-intent-block",
        &ctx,
    );

    assert!(
        !result.success,
        "write must fail closed: {}",
        result.content
    );
    assert_eq!(
        result
            .result
            .error
            .as_ref()
            .map(|error| error.code.as_str()),
        Some("AUDIT_UNAVAILABLE")
    );
    assert!(
        !target.exists(),
        "handler must not create the target after intent failure"
    );
    let _ = env;
}

#[test]
fn audit_intent_failure_blocks_exec_before_side_effect() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let env = setup("audit-intent-exec");
    let ctx = qaqh_workspace::runtime::ToolCtx::admitted("audit-intent-exec");
    let target = env.workspace.join("exec-must-not-exist.txt");

    let result = call(
        "exec",
        serde_json::json!({"command": format!("touch {}", target.display())}),
        "call-exec-intent-block",
        &ctx,
    );

    assert!(!result.success, "exec must fail closed: {}", result.content);
    assert_eq!(
        result
            .result
            .error
            .as_ref()
            .map(|error| error.code.as_str()),
        Some("AUDIT_UNAVAILABLE")
    );
    assert!(
        !target.exists(),
        "exec handler must not run after intent failure"
    );
    let _ = env;
}

#[test]
fn audit_intent_failure_blocks_net_before_request() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let env = setup("audit-intent-net");
    let ctx = qaqh_workspace::runtime::ToolCtx::admitted("audit-intent-net");

    let result = call(
        "web_fetch",
        serde_json::json!({"url": "http://127.0.0.1:1/never"}),
        "call-net-intent-block",
        &ctx,
    );

    assert!(!result.success, "net must fail closed: {}", result.content);
    assert_eq!(
        result
            .result
            .error
            .as_ref()
            .map(|error| error.code.as_str()),
        Some("AUDIT_UNAVAILABLE")
    );
    let _ = env;
}

#[test]
fn audit_intent_failure_does_not_block_read_only() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let env = setup("audit-read-open");
    let ctx = qaqh_workspace::runtime::ToolCtx::admitted("audit-read-open");
    let target = env.workspace.join("readable.txt");
    std::fs::write(&target, "readable body").expect("write readable file");

    let result = call(
        "read",
        serde_json::json!({"path": target}),
        "call-read-open",
        &ctx,
    );

    assert!(
        result.success,
        "read must remain fail-open: {}",
        result.content
    );
    assert!(
        result.content.contains("readable body"),
        "read body missing: {}",
        result.content
    );
    let _ = env;
}
