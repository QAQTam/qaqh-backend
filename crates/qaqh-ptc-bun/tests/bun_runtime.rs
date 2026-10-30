//! bun 后端的端到端测试。
//!
//! 没有 bun 时整组跳过（`QAQH_PTC_BUN` 可指定可执行文件路径）。这些用例覆盖
//! 协议边界：完成值、日志捕获、绑定往返/拒绝、无损 JSON 校验、超时、取消、
//! 子进程早退与输出上限。

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use qaqh_ptc_bun::{BunConfig, BunPtcRuntime};
use qaqh_ptc_core::{
    PtcBindingFn, PtcBindingNamespace, PtcCancel, PtcFailureKind, PtcRunRequest, PtcRuntime,
};
use serde_json::{Value, json};

fn bun_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("QAQH_PTC_BUN") {
        return Some(PathBuf::from(path));
    }
    let output = Command::new("bun").arg("--version").output().ok()?;
    output.status.success().then(|| PathBuf::from("bun"))
}

macro_rules! require_bun {
    () => {
        match bun_path() {
            Some(path) => path,
            None => {
                eprintln!("skipping: bun is not installed (set QAQH_PTC_BUN to run these tests)");
                return;
            }
        }
    };
}

fn runtime(bun: PathBuf, max_output_bytes: usize) -> BunPtcRuntime {
    BunPtcRuntime::new(BunConfig {
        executable: bun,
        default_timeout: Duration::from_secs(30),
        max_timeout: Duration::from_secs(60),
        max_output_bytes,
        max_message_bytes: 1 << 20,
        startup_timeout: Duration::from_secs(20),
    })
}

fn request(program: &str, bindings: Vec<PtcBindingNamespace>, timeout: Duration) -> PtcRunRequest {
    PtcRunRequest {
        program: program.to_owned(),
        bindings,
        cwd: Some(std::env::temp_dir()),
        timeout: Some(timeout),
        max_output_bytes: None,
        cancel: PtcCancel::new(),
    }
}

#[test]
fn returns_a_lossless_json_value() {
    let bun = require_bun!();
    let runtime = runtime(bun, 1 << 20);
    let spec = runtime
        .resolve(request(
            "return { ok: true, count: 3, list: [1, null, 'x'] }",
            vec![],
            Duration::from_secs(20),
        ))
        .expect("resolve");
    let result = runtime.run(spec).expect("run");
    assert!(result.is_ok(), "unexpected failure: {:?}", result.failure);
    assert_eq!(
        result.value,
        Some(json!({ "ok": true, "count": 3, "list": [1, null, "x"] }))
    );
}

#[test]
fn captures_console_and_stream_writes_in_order() {
    let bun = require_bun!();
    let runtime = runtime(bun, 1 << 20);
    let spec = runtime
        .resolve(request(
            "console.log('one'); process.stdout.write('two\\n'); console.error('three'); return 4",
            vec![],
            Duration::from_secs(20),
        ))
        .expect("resolve");
    let result = runtime.run(spec).expect("run");
    assert!(result.is_ok(), "unexpected failure: {:?}", result.failure);
    assert_eq!(result.logs, vec!["one", "two\n", "three"]);
    assert_eq!(result.value, Some(json!(4)));
}

#[test]
fn binding_round_trip_returns_the_host_value() {
    let bun = require_bun!();
    let runtime = runtime(bun, 1 << 20);
    let read: Arc<PtcBindingFn> = Arc::new(|args: Value| {
        let path = args.get("path").and_then(Value::as_str).unwrap_or_default();
        Ok(json!({ "path": path, "content": "hello" }))
    });
    let tools = PtcBindingNamespace::new("tools")
        .with_member("read", read)
        .with_error_class("ToolCallError", "toolName");
    let spec = runtime
        .resolve(request(
            "const r = await tools.read({ path: 'a.txt' }); return r.content",
            vec![tools],
            Duration::from_secs(20),
        ))
        .expect("resolve");
    let result = runtime.run(spec).expect("run");
    assert!(result.is_ok(), "unexpected failure: {:?}", result.failure);
    assert_eq!(result.value, Some(json!("hello")));
}

#[test]
fn binding_failure_is_a_catchable_typed_error() {
    let bun = require_bun!();
    let runtime = runtime(bun, 1 << 20);
    let fail: Arc<PtcBindingFn> = Arc::new(|_args: Value| Err("permission denied".to_owned()));
    let tools = PtcBindingNamespace::new("tools")
        .with_member("write", fail)
        .with_error_class("ToolCallError", "toolName");
    let spec = runtime
        .resolve(request(
            "try { await tools.write({}) } catch (e) { return { name: e.name, tool: e.toolName, message: e.message } }",
            vec![tools],
            Duration::from_secs(20),
        ))
        .expect("resolve");
    let result = runtime.run(spec).expect("run");
    assert!(result.is_ok(), "unexpected failure: {:?}", result.failure);
    assert_eq!(
        result.value,
        Some(json!({
            "name": "ToolCallError",
            "tool": "write",
            "message": "permission denied",
        }))
    );
}

#[test]
fn undeclared_binding_names_are_rejected_inside_the_program() {
    let bun = require_bun!();
    let runtime = runtime(bun, 1 << 20);
    let spec = runtime
        .resolve(request(
            "try { await tools['__proto__']({}) ; return 'reached' } catch (e) { return 'rejected: ' + e.message }",
            vec![],
            Duration::from_secs(20),
        ))
        .expect("resolve");
    let result = runtime.run(spec).expect("run");
    assert!(result.is_ok(), "unexpected failure: {:?}", result.failure);
    assert_eq!(result.value, Some(json!("rejected: tools is not defined")));
}

#[test]
fn non_lossless_completion_is_invalid_output() {
    let bun = require_bun!();
    let runtime = runtime(bun, 1 << 20);
    let spec = runtime
        .resolve(request(
            "return { fn: () => 1 }",
            vec![],
            Duration::from_secs(20),
        ))
        .expect("resolve");
    let result = runtime.run(spec).expect("run");
    let failure = result.failure.expect("failure");
    assert_eq!(failure.kind, PtcFailureKind::InvalidOutput);
}

#[test]
fn syntax_errors_surface_as_exceptions() {
    let bun = require_bun!();
    let runtime = runtime(bun, 1 << 20);
    let spec = runtime
        .resolve(request("return (", vec![], Duration::from_secs(20)))
        .expect("resolve");
    let result = runtime.run(spec).expect("run");
    let failure = result.failure.expect("failure");
    assert_eq!(failure.kind, PtcFailureKind::Exception);
}

#[test]
fn timeout_kills_a_program_that_never_settles() {
    let bun = require_bun!();
    let runtime = runtime(bun, 1 << 20);
    let spec = runtime
        .resolve(request(
            "await new Promise(() => {})",
            vec![],
            Duration::from_millis(400),
        ))
        .expect("resolve");
    let started = std::time::Instant::now();
    let result = runtime.run(spec).expect("run");
    assert_eq!(
        result.failure.map(|failure| failure.kind),
        Some(PtcFailureKind::Timeout)
    );
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[test]
fn cancel_aborts_a_running_program() {
    let bun = require_bun!();
    let runtime = Arc::new(runtime(bun, 1 << 20));
    let cancel = PtcCancel::new();
    let mut request = request(
        "await new Promise((resolve) => setTimeout(resolve, 10_000))",
        vec![],
        Duration::from_secs(30),
    );
    request.cancel = cancel.clone();
    let spec = runtime.resolve(request).expect("resolve");
    let flag = cancel.clone();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(300));
        flag.cancel();
    });
    let result = runtime.run(spec).expect("run");
    assert_eq!(
        result.failure.map(|failure| failure.kind),
        Some(PtcFailureKind::Abort)
    );
}

#[test]
fn child_exit_before_settlement_is_worker_exit() {
    let bun = require_bun!();
    let runtime = runtime(bun, 1 << 20);
    let spec = runtime
        .resolve(request("process.exit(3)", vec![], Duration::from_secs(20)))
        .expect("resolve");
    let result = runtime.run(spec).expect("run");
    assert_eq!(
        result.failure.map(|failure| failure.kind),
        Some(PtcFailureKind::WorkerExit)
    );
}

#[test]
fn output_over_budget_fails_the_run() {
    let bun = require_bun!();
    let runtime = runtime(bun, 4_096);
    let spec = runtime
        .resolve(request(
            "console.log('x'.repeat(20_000)); return 1",
            vec![],
            Duration::from_secs(20),
        ))
        .expect("resolve");
    let result = runtime.run(spec).expect("run");
    assert_eq!(
        result.failure.map(|failure| failure.kind),
        Some(PtcFailureKind::OutputLimit)
    );
}

#[test]
fn undefined_completion_has_no_value() {
    let bun = require_bun!();
    let runtime = runtime(bun, 1 << 20);
    let spec = runtime
        .resolve(request("const x = 1", vec![], Duration::from_secs(20)))
        .expect("resolve");
    let result = runtime.run(spec).expect("run");
    assert!(result.is_ok(), "unexpected failure: {:?}", result.failure);
    assert_eq!(result.value, None);
}

#[test]
fn resolve_rejects_relative_or_missing_cwd_and_zero_timeout() {
    let bun = require_bun!();
    let runtime = runtime(bun, 1 << 20);
    let mut relative = request("return 1", vec![], Duration::from_secs(5));
    relative.cwd = Some(PathBuf::from("relative/path"));
    assert!(runtime.resolve(relative).is_err());

    let mut missing = request("return 1", vec![], Duration::from_secs(5));
    missing.cwd = Some(std::env::temp_dir().join("qaqh-ptc-does-not-exist"));
    assert!(runtime.resolve(missing).is_err());

    let zero = request("return 1", vec![], Duration::ZERO);
    assert!(runtime.resolve(zero).is_err());
}

#[test]
fn runtime_declares_typescript_function_body() {
    use qaqh_ptc_core::{PtcLanguage, PtcProgramFormat};
    let runtime = BunPtcRuntime::default();
    assert_eq!(runtime.language(), PtcLanguage::TypeScript);
    assert_eq!(runtime.program_format(), PtcProgramFormat::FunctionBody);
    assert!(!runtime.execution_instructions().is_empty());
}
