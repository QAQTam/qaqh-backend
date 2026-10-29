use serde_json::json;

#[test]
fn public_schema_exposes_todo_write_update_with_no_legacy_names() {
    let manager = qaqh_workspace::registration::build_tool_manager(&[]);
    let definitions = manager.all_defs();
    let names: Vec<&str> = definitions
        .iter()
        .map(|definition| definition.function.name.as_str())
        .collect();

    // 文本范式（本版本）：模型工具面收敛为 todo_write / todo_update 双工具。
    // 完整列表由 write 的全量文本覆写 + output.items 快照携带；
    // 只读 todo_list 从模型面退役（`todo_list_for` 保留给 HTTP service）。
    assert!(names.contains(&"todo_write"), "missing todo_write");
    assert!(names.contains(&"todo_update"), "missing todo_update");
    let todo_prefixed: Vec<_> = names
        .iter()
        .filter(|name| name.starts_with("todo_"))
        .copied()
        .collect();
    assert_eq!(
        todo_prefixed.len(),
        2,
        "exactly the two text-format todo tools must be exposed, got {todo_prefixed:?}"
    );
    // 旧聚合工具 todo / task 别名 / 只读 todo_list 都不得再暴露。
    assert!(!names.contains(&"todo"));
    assert!(!names.contains(&"task"));
    assert!(
        !names.contains(&"todo_list"),
        "todo_list must not be exposed to the model (retired from the tool face)"
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
    assert_eq!(write.function.parameters["required"], json!(["list"]));
    assert_eq!(
        write.function.parameters["additionalProperties"],
        json!(false)
    );
    assert_eq!(
        write.function.parameters["properties"]["list"]["type"],
        json!("string"),
        "todo_write takes one plain-text block (apply_patch-style), not an items array"
    );
    // 全量覆写语义守卫：描述必须声明 full-replace（防 append 语义回潮）。
    assert!(
        write.function.description.contains("full-replace"),
        "todo_write description must advertise full-replace semantics"
    );
    assert!(
        write.function.description.contains("Titles are identities"),
        "todo_write description must document title-as-identity"
    );

    let update = find("todo_update");
    assert_eq!(
        update.function.parameters["required"],
        json!(["title", "status"])
    );
    assert_eq!(
        update.function.parameters["properties"]["status"]["enum"],
        json!(["pending", "in_progress", "completed", "cancelled"])
    );
    assert_eq!(
        update.function.parameters["additionalProperties"],
        json!(false)
    );
    assert!(
        update.function.parameters["properties"]["id"].is_null(),
        "title-as-identity: todo_update must not take an id"
    );
}

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
    // 三个变量都必须钉住——否则真实 home 的历史 todo 会造成"跑两遍就假红"。
    unsafe {
        std::env::set_var("USERPROFILE", &temp_home);
        std::env::set_var("HOME", &temp_home);
        std::env::set_var("QAQH_DATA_DIR", &temp_data);
    }
    qaqh_workspace::runtime::init_tools("todo-contract", &[], vec![]);
    qaqh_workspace::runtime::set_context("todo-contract", 1);
    // 显式上下文。刻意用最严档 read-only：todo_* 是会话内状态工具，
    // permission 层对其豁免（永不弹确认），本测试同时验证这一点。
    let ctx = qaqh_workspace::runtime::ToolCtx {
        session_id: "todo-contract".into(),
        permission_level: 1,
        mode: 0,
        workspace_root: None,
    };

    // write：全量文本覆写（apply_patch 范式，一行一条）。
    let create = qaqh_workspace::execution::execute_with_context(
        "todo_write",
        serde_json::json!({"list": "- [ ] Working | item 0\n- [ ] Done | item 1\n- [ ] Cancelled | item 2\n- [ ] Waiting | item 3"})
            .to_string()
            .as_str(),
        "todo-create",
        None,
        &ctx,
    );
    assert!(create.success, "create failed: {}", create.content);
    assert_eq!(create.result.data["total"], 4);
    assert_eq!(create.result.data["in_progress"], 0);
    assert_eq!(create.result.data["items"].as_array().unwrap().len(), 4);
    // id = title（身份即标题，GoalEngine / dashboard 契约）。
    assert_eq!(create.result.data["items"][0]["id"], "Working");
    let create_display = qaqh_workspace::runtime::project_tool_display_from_result(
        "todo_write",
        &serde_json::json!({}),
        &create.result,
    )
    .expect("typed todo_write display");
    assert_eq!(
        create_display.summary.as_deref(),
        Some("Plan updated: 4 item(s), 0 in progress.")
    );

    // update：按 title 精确翻转（无 ID）。
    let working = qaqh_workspace::execution::execute_with_context(
        "todo_update",
        r#"{"title":"Working","status":"in_progress"}"#,
        "todo-working",
        None,
        &ctx,
    );
    assert!(
        working.success,
        "working update failed: {}",
        working.content
    );
    assert_eq!(working.result.data["item"]["status"], "in_progress");
    assert_eq!(
        qaqh_workspace::runtime::project_tool_display_from_result(
            "todo_update",
            &serde_json::json!({}),
            &working.result,
        )
        .and_then(|display| display.summary)
        .as_deref(),
        Some("Todo `Working` is now in_progress.")
    );

    let completed = qaqh_workspace::execution::execute_with_context(
        "todo_update",
        r#"{"title":"Done","status":"completed","evidence":"verified"}"#,
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
        r#"{"title":"Cancelled","status":"cancelled"}"#,
        "todo-cancelled",
        None,
        &ctx,
    );
    assert!(
        cancelled.success,
        "cancel operation failed: {}",
        cancelled.content
    );

    // update 的 output.items 是全量快照：四个终态分布必须与文本覆写一致。
    let items = completed.result.data["items"].as_array().unwrap();
    assert_eq!(items.len(), 4);
    assert_eq!(items[0]["status"], "in_progress");
    assert_eq!(items[1]["status"], "completed");
    assert_eq!(items[1]["evidence"], "verified");

    let status: serde_json::Value = serde_json::from_str(
        &qaqh_workspace::todo::todo_status_json("todo-contract").expect("status JSON"),
    )
    .expect("parse status JSON");
    assert_eq!(status["mode"], "manual");
    // current 收敛到第一个 in_progress；id == title。
    assert_eq!(status["current_id"], "Working");
    assert_eq!(status["current_title"], "Working");
    assert_eq!(status["pending"], 1);
    assert_eq!(status["in_progress"], 1);
    assert_eq!(status["completed"], 1);
    assert_eq!(status["cancelled"], 1);
    assert_eq!(status["total"], 4);
    assert_eq!(status["items"][2]["status"], "cancelled");

    // todo.json 格式干净（无 legacy Goal 强制归一化）。
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
