//! 最小端到端演示：一段模型风格的 TypeScript 程序调用一个宿主绑定。
//!
//! 运行：
//!
//! ```text
//! QAQH_PTC_BUN=/path/to/bun cargo run -p qaqh-ptc-bun --example demo
//! ```

use std::sync::Arc;
use std::time::Duration;

use qaqh_ptc_bun::{BunConfig, BunPtcRuntime};
use qaqh_ptc_core::{PtcBindingFn, PtcBindingNamespace, PtcCancel, PtcRunRequest, PtcRuntime};
use serde_json::{Value, json};

fn main() {
    let executable = std::env::var("QAQH_PTC_BUN").unwrap_or_else(|_| "bun".to_owned());
    let runtime = BunPtcRuntime::new(BunConfig {
        executable: executable.into(),
        default_timeout: Duration::from_secs(30),
        ..BunConfig::default()
    });

    // 宿主绑定的形状与 run_code 一致：一个 `tools` 命名空间，成员是异步可调用函数。
    let read: Arc<PtcBindingFn> = Arc::new(|args: Value| {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("<missing>");
        Ok(json!({ "path": path, "content": "hello from the host" }))
    });
    let tools = PtcBindingNamespace::new("tools")
        .with_member("read", read)
        .with_member(
            "explode",
            Arc::new(|_args: Value| Err("permission denied by policy".to_owned())),
        )
        .with_error_class("ToolCallError", "toolName");

    let program = r#"
const first = await tools.read({ path: "README.md" })
let guarded = "not called"
try {
  await tools.explode({})
} catch (error) {
  guarded = `${error.name}:${error.toolName}`
}
console.log("read", first.path)
return { content: first.content, guarded }
"#;

    let spec = runtime
        .resolve(PtcRunRequest {
            program: program.to_owned(),
            bindings: vec![tools],
            cwd: Some(std::env::current_dir().expect("cwd")),
            timeout: Some(Duration::from_secs(30)),
            max_output_bytes: None,
            cancel: PtcCancel::new(),
        })
        .expect("resolve");

    let result = runtime.run(spec).expect("run");
    println!("logs: {:?}", result.logs);
    println!("value: {:?}", result.value);
    if let Some(failure) = result.failure {
        eprintln!("failure: {failure}");
        std::process::exit(1);
    }
}
