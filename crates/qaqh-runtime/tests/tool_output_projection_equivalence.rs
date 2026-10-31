//! P3 gate: model/display/service projections derive from one typed todo output.

#![allow(clippy::unwrap_used)] // integration test fixture

use serde_json::json;

fn isolate_data_root() -> tempfile::TempDir {
    let temp_home = tempfile::tempdir().expect("temp home");
    let temp_data = temp_home.path().join(".qaqh");
    std::fs::create_dir_all(&temp_data).expect("create data root");
    // SAFETY: 测试二进制独占进程环境；本函数只在启动阶段设置一次隔离数据根。
    unsafe {
        std::env::set_var("USERPROFILE", temp_home.path());
        std::env::set_var("HOME", temp_home.path());
        std::env::set_var("QAQH_DATA_DIR", &temp_data);
    }
    temp_home
}

#[test]
fn todo_typed_output_is_the_single_source_for_model_display_and_service() {
    let _home = isolate_data_root();
    let session_id = "typed-output-equivalence";
    qaqh_workspace::runtime::init_tools(session_id, &[], vec![]);
    qaqh_workspace::runtime::set_context(session_id, 1);

    // `todo_write` 的入参是纯文本块（`list`），不是结构化数组：解析与状态词表
    // 都在工具域，客户端只消费结构化输出。
    let args = json!({
        "list": "- [~] Typed output | one source"
    });
    let ctx = qaqh_workspace::runtime::ToolCtx {
        session_id: session_id.into(),
        permission_level: 1,
        mode: 0,
        workspace_root: None,
    };
    let executed = qaqh_workspace::execution::execute_with_context(
        "todo_write",
        &args.to_string(),
        "typed-output-call",
        None,
        &ctx,
    );
    assert!(
        executed.success,
        "todo_write execution failed: {}",
        executed.content
    );

    assert_eq!(
        executed.result.data["total"].as_u64(),
        Some(1),
        "the canonical output must carry the typed total"
    );
    assert!(
        executed.result.display().is_some(),
        "typed tools must persist the canonical display payload on ToolResult"
    );
    assert!(
        executed
            .result
            .model_text()
            .contains(executed.result.summary()),
        "model projection must carry the typed summary: {}",
        executed.result.model_text()
    );

    let display = qaqh_workspace::runtime::project_tool_display_from_result(
        "todo_write",
        &args,
        &executed.result,
    )
    .expect("typed todo display projection");
    assert_eq!(display.summary.as_deref(), Some(executed.result.summary()));
    assert!(
        !display
            .summary
            .as_deref()
            .is_some_and(|summary| summary.trim_start().starts_with('{')),
        "display summary must be human text, not raw JSON"
    );

    let mut corrupted = executed.result.clone();
    corrupted.set_model_projection("not-json and no longer canonical".into(), false);
    let display =
        qaqh_workspace::runtime::project_tool_display_from_result("todo_write", &args, &corrupted)
            .expect("display must come from ToolResult.display, not model text");
    assert_eq!(display.summary.as_deref(), Some(executed.result.summary()));
}
