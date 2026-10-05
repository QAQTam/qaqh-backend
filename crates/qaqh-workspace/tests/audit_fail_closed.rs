//! P4 A1: audit intent barrier for high-risk tools.
//!
//! `write` / `exec` / `net` must durably record a v2 `tool_intent` before the
//! handler is dispatched. If that intent write fails, the handler must not run
//! and the call must fail with `audit_unavailable`.

use std::path::PathBuf;
use std::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::new(());

struct Env {
    _tmp: tempfile::TempDir,
    data: PathBuf,
    workspace: PathBuf,
}

fn setup(label: &str) -> Env {
    setup_with_v2_block(label, true)
}

fn setup_result_failure(label: &str) -> Env {
    setup_with_v2_block(label, false)
}

fn setup_with_v2_block(label: &str, block_v2: bool) -> Env {
    let tmp = tempfile::tempdir().expect("tempdir");
    let data = tmp.path().join(format!("{label}-data"));
    let workspace = tmp.path().join(format!("{label}-ws"));
    std::fs::create_dir_all(&data).expect("create data dir");
    std::fs::create_dir_all(&workspace).expect("create workspace");
    if block_v2 {
        // Force the v2 append path to fail: v2.jsonl is a directory, not a file.
        std::fs::create_dir_all(data.join("audit").join("v2.jsonl")).expect("block v2 path");
    }
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
        data,
        workspace,
    }
}

fn call(
    name: &str,
    args: serde_json::Value,
    call_id: &str,
    ctx: &qaqh_workspace::runtime::ToolCtx,
) -> qaqh_workspace::execution::ToolExecResult {
    qaqh_workspace::execution::execute_with_context(name, &args.to_string(), call_id, None, ctx)
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
        Some("audit_unavailable")
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
        Some("audit_unavailable")
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
        Some("audit_unavailable")
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

#[test]
fn audit_result_failure_quarantines_and_blocks_subsequent_high_risk_tools() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let env = setup_result_failure("audit-result-quarantine");
    let ctx = qaqh_workspace::runtime::ToolCtx::admitted("audit-result-quarantine");
    let first_target = env.workspace.join("first-side-effect.txt");

    // Let the intent barrier succeed, then fail only the terminal result write.
    qaqh_workspace::audit::fail_next_result_for_test();
    let first = call(
        "write",
        serde_json::json!({"path": first_target, "content": "side effect"}),
        "call-result-quarantine",
        &ctx,
    );

    assert!(
        !first.success,
        "result barrier failure must not report success: {}",
        first.content
    );
    assert_eq!(
        first.result.error.as_ref().map(|error| error.code.as_str()),
        Some("audit_quarantined")
    );
    assert!(
        first_target.exists(),
        "handler side effect should have happened before the result barrier failed"
    );
    assert!(
        qaqh_workspace::audit::is_quarantined(),
        "result barrier failure must enter quarantine"
    );

    let emergency = std::fs::read_to_string(env.data.join("audit").join("emergency.jsonl"))
        .expect("emergency sink record");
    assert!(
        emergency.contains("call-result-quarantine"),
        "emergency sink must identify the quarantined call: {emergency}"
    );

    let second_target = env.workspace.join("must-not-run.txt");
    let second = call(
        "write",
        serde_json::json!({"path": second_target, "content": "must not run"}),
        "call-after-quarantine",
        &ctx,
    );
    assert!(!second.success, "quarantined write must be rejected");
    assert_eq!(
        second
            .result
            .error
            .as_ref()
            .map(|error| error.code.as_str()),
        Some("audit_quarantined")
    );
    assert!(
        !second_target.exists(),
        "quarantine must stop subsequent high-risk handlers"
    );
    let _ = env;
}
