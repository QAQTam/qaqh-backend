#[test]
fn public_schema_exposes_todo_write_update_list_with_no_legacy_names() {
    let manager = qaqh_workspace::registration::build_tool_manager(&[]);
    let definitions = manager.all_defs();
    let names: Vec<&str> = definitions
        .iter()
        .map(|definition| definition.function.name.as_str())
        .collect();

    // Todo v4（全量覆写形态）：todo_write / todo_update / todo_list 三件套。
    let todo_tools = ["todo_write", "todo_update", "todo_list"];
    for expected in todo_tools {
        assert!(
            names.contains(&expected),
            "missing {expected} in public schema"
        );
    }
    // todo_* 前缀下不允许存在三件套之外的残留（W1 的 todo_create /
    // todo_insert / todo_set 已随聚合与再拆分退役）。
    let todo_prefixed: Vec<_> = names
        .iter()
        .filter(|name| name.starts_with("todo_"))
        .copied()
        .collect();
    assert_eq!(
        todo_prefixed.len(),
        3,
        "exactly the three v4 todo tools must be exposed, got {todo_prefixed:?}"
    );
    // 旧聚合工具 todo 与更早的别名 task 都不得再暴露。
    assert!(
        !names.contains(&"todo"),
        "aggregated todo tool must not be exposed (v3 splits it)"
    );
    assert!(
        !names.contains(&"task"),
        "removed task alias must not be exposed"
    );
    for definition in &definitions {
        if !definition.function.name.starts_with("todo_") {
            continue;
        }
        assert!(
            !definition.function.description.contains("Goal"),
            "the frozen Goal workflow must not be advertised to the model"
        );
    }

    let find = |name: &str| {
        definitions
            .iter()
            .find(|definition| definition.function.name == name)
            .expect("todo definition")
    };
    let write = find("todo_write");
    assert_eq!(write.function.parameters["required"], json!(["items"]));
    assert_eq!(
        write.function.parameters["additionalProperties"],
        json!(false)
    );
    assert_eq!(
        write.function.parameters["properties"]["items"]["maxItems"],
        json!(20)
    );
    // v4：条目内 status 必填（写即状态——in_progress 显式告知载体）；
    // id/evidence/description 可选（QAQ 增强字段保留）。
    let write_item = &write.function.parameters["properties"]["items"]["items"];
    assert_eq!(write_item["required"], json!(["title", "status"]));
    assert_eq!(
        write_item["properties"]["status"]["enum"],
        json!(["idle", "in_progress", "completed", "cancelled"])
    );
    assert!(write_item["properties"]["id"].is_object());
    assert!(write_item["properties"]["evidence"].is_object());
    // v4：explanation 顶层可选（中改计划的理由，对齐 Codex update_plan）。
    assert!(
        write.function.parameters["properties"]["explanation"].is_object(),
        "todo_write.explanation missing"
    );
    // v4 回归守卫：工具描述必须声明全量覆写语义（防 append 语义回潮）。
    assert!(
        write.function.description.contains("full-replace"),
        "todo_write description must advertise full-replace semantics"
    );

    let update = find("todo_update");
    assert_eq!(
        update.function.parameters["required"],
        json!(["id", "status"])
    );
    assert_eq!(
        update.function.parameters["properties"]["status"]["enum"],
        json!(["idle", "in_progress", "completed", "cancelled"])
    );
    assert_eq!(
        update.function.parameters["additionalProperties"],
        json!(false)
    );

    let list = find("todo_list");
    assert_eq!(
        list.function.parameters["additionalProperties"],
        json!(false)
    );
    assert_eq!(
        list.function.parameters["properties"]["status"]["enum"],
        json!(["idle", "in_progress", "completed", "cancelled"])
    );
}

use serde_json::json;

#[test]
fn manual_status_transitions_round_trip_to_the_frontend_contract() {
    let temp_home = std::env::temp_dir().join(format!(
        "qaqh-todo-contract-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&temp_home).expect("create isolated home");
    let temp_data = temp_home.join(".qaqh");
    std::fs::create_dir_all(&temp_data).expect("create isolated data root");

    // This integration test binary owns an isolated process home.
    // `qaqh_types::platform::data_dir()` 的优先级是
    // `QAQH_DATA_DIR` > `%USERPROFILE%`(win) / `$XDG_CONFIG_HOME` / `$HOME`(unix)，
    // 所以三个变量都必须钉住：
    // - 不钉 `HOME`/`USERPROFILE`：会读进真实用户 home 的历史 todo；
    // - 不钉 `QAQH_DATA_DIR`：外层（CI/验收脚本）若复用了同一数据目录，
    //   上一次运行留下的 `sessions/todo-contract/todo.json` 会让本次
    //   `todo_write` 拿不到 `T1`（`NOT_FOUND: no matching todos: T1`）——
    //   即「跑两遍就假红」（N-6）。
    unsafe {
        std::env::set_var("USERPROFILE", &temp_home);
        std::env::set_var("HOME", &temp_home);
        std::env::set_var("QAQH_DATA_DIR", &temp_data);
    }
    qaqh_workspace::runtime::init_tools("todo-contract", &[], vec![]);
    qaqh_workspace::runtime::set_context("todo-contract", 1);
    // PR-3-2：显式上下文。刻意用 Level 1（MaxLockdown）：todo_* 是会话内
    // 状态工具，permission 层对其豁免（永不弹确认），本测试同时验证这一点。
    let ctx = qaqh_workspace::runtime::ToolCtx {
        session_id: "todo-contract".into(),
        permission_level: 1,
        mode: 0,
        workspace_root: None,
    };

    let create = qaqh_workspace::execution::execute_with_context(
        "todo_write",
        "",
        &serde_json::json!({"items": [
            {"title": "Working", "description": "item 0", "status": "idle"},
            {"title": "Done", "description": "item 1", "status": "idle"},
            {"title": "Cancelled", "description": "item 2", "status": "idle"},
            {"title": "Waiting", "description": "item 3", "status": "idle"}
        ]})
        .to_string(),
        "todo-create",
        None,
        &ctx,
    );
    assert!(create.success, "create failed: {}", create.content);

    let working = qaqh_workspace::execution::execute_with_context(
        "todo_update",
        "",
        r#"{"id":"T1","status":"in_progress"}"#,
        "todo-working",
        None,
        &ctx,
    );
    assert!(
        working.success,
        "working update failed: {}",
        working.content
    );

    let completed = qaqh_workspace::execution::execute_with_context(
        "todo_update",
        "",
        r#"{"id":"T2","status":"completed","evidence":"verified"}"#,
        "todo-completed",
        None,
        &ctx,
    );
    assert!(
        completed.success,
        "completed update failed: {}",
        completed.content
    );

    let cancelled = qaqh_workspace::execution::execute_with_context(
        "todo_update",
        "",
        r#"{"id":"T3","status":"cancelled"}"#,
        "todo-cancelled",
        None,
        &ctx,
    );
    assert!(
        cancelled.success,
        "cancel operation failed: {}",
        cancelled.content
    );

    let list = qaqh_workspace::execution::execute_with_context(
        "todo_list",
        "",
        r#"{}"#,
        "todo-list",
        None,
        &ctx,
    );
    assert!(list.success, "list failed: {}", list.content);
    let list_json: serde_json::Value =
        serde_json::from_str(&list.content).expect("structured list response");
    assert_eq!(list_json["counts"]["in_progress"], 1);
    assert_eq!(list_json["counts"]["completed"], 1);
    assert_eq!(list_json["counts"]["cancelled"], 1);
    assert_eq!(list_json["counts"]["idle"], 1);
    assert_eq!(list_json["items"][0]["id"], "T1");
    assert_eq!(list_json["items"][1]["status"], "completed");
    assert_eq!(list_json["items"][1]["evidence"], "verified");
    assert_eq!(list_json["items"][3]["status"], "idle");

    let status: serde_json::Value = serde_json::from_str(
        &qaqh_workspace::todo::todo_status_json("todo-contract").expect("status JSON"),
    )
    .expect("parse status JSON");
    assert_eq!(status["mode"], "manual");
    assert_eq!(status["current_id"], "T1");
    assert_eq!(status["current_title"], "Working");
    assert_eq!(status["idle"], 1);
    assert_eq!(status["pending"], 1);
    assert_eq!(status["in_progress"], 1);
    assert_eq!(status["completed"], 1);
    assert_eq!(status["cancelled"], 1);
    assert_eq!(status["total"], 4);
    assert_eq!(status["items"][2]["status"], "cancelled");

    // Verify that the todo.json format is clean (no legacy Goal-enforced normalization).
    let store = qaqh_workspace::todo::load_todo().expect("load todo");
    assert_eq!(store.mode, qaqh_workspace::todo::TodoMode::Manual);

    std::fs::remove_dir_all(&temp_home).expect("remove isolated home");
}

/// web_fetch 的 `output` 是真实写目标（web.rs 里无条件 `fs::write`），
/// 必须与 journal 的 `out` 同款进授权资源；否则审批清单看不到写目标，
/// `all_within_workspace` / trust folder 边界对 web_fetch 全部失明。
#[test]
fn web_fetch_output_enters_authorization_resources() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().canonicalize().unwrap();
    let out = workspace.join("notes").join("page.md");

    let _guard =
        qaqh_workspace::push_thread_workspace(Some(workspace.to_string_lossy().into_owned()));
    let paths = qaqh_workspace::permission::extract_target_paths(
        "web_fetch",
        &serde_json::json!({"url": "https://example.com", "output": "notes/page.md"}),
    );
    // 无 output 参数的 web_fetch（只读正文）不得凭空产生写目标。
    let read_only = qaqh_workspace::permission::extract_target_paths(
        "web_fetch",
        &serde_json::json!({"url": "https://example.com"}),
    );
    qaqh_workspace::pop_thread_workspace(None);

    assert!(
        read_only.is_empty(),
        "web_fetch without `output` must declare no path: {read_only:?}"
    );
    assert!(
        paths.contains(&out),
        "web_fetch output missing from authorization resources: {paths:?}"
    );
}
