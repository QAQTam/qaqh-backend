#[test]
fn public_schema_exposes_small_direct_todo_tools_and_no_goal_entrypoint() {
    let manager = deepx_tools::registration::build_tool_manager(&[]);
    let definitions = manager.all_defs();
    let names: Vec<&str> = definitions
        .iter()
        .map(|definition| definition.function.name.as_str())
        .collect();

    for expected in ["todo_create", "todo_update", "todo_cancel", "todo_list"] {
        assert!(names.contains(&expected), "missing {expected}");
    }
    assert!(
        !names.contains(&"todo"),
        "legacy multiplexed tool must stay hidden"
    );
    assert!(
        !names.contains(&"task"),
        "deprecated duplicate task tool must stay hidden"
    );
    assert!(
        definitions
            .iter()
            .filter(|definition| definition.function.name.starts_with("todo_"))
            .all(|definition| !definition.function.description.contains("Goal")),
        "the frozen Goal workflow must not be advertised to the model"
    );
}

#[test]
fn manual_status_transitions_round_trip_to_the_frontend_contract() {
    let temp_home = std::env::temp_dir().join(format!(
        "deepx-todo-contract-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&temp_home).expect("create isolated home");

    // This integration test binary owns an isolated process home.
    unsafe { std::env::set_var("USERPROFILE", &temp_home) };
    deepx_tools::runtime::init_tools("todo-contract", &[], vec![]);
    deepx_tools::runtime::set_context("todo-contract", 1);

    for (index, title) in ["Working", "Done", "Cancelled", "Waiting"]
        .into_iter()
        .enumerate()
    {
        let create = deepx_tools::execution::execute_with_context(
            "todo_create",
            "",
            &serde_json::json!({"title": title, "description": format!("item {index}")})
                .to_string(),
            &format!("todo-create-{index}"),
            None,
        );
        assert!(create.success, "create failed: {}", create.content);
    }

    let working = deepx_tools::execution::execute_with_context(
        "todo_update",
        "",
        r#"{"id":1,"status":"in_progress"}"#,
        "todo-working",
        None,
    );
    assert!(
        working.success,
        "working update failed: {}",
        working.content
    );

    let completed = deepx_tools::execution::execute_with_context(
        "todo_update",
        "",
        r#"{"id":"T2","status":"completed","evidence":"verified"}"#,
        "todo-completed",
        None,
    );
    assert!(
        completed.success,
        "completed update failed: {}",
        completed.content
    );

    let cancelled = deepx_tools::execution::execute_with_context(
        "todo_cancel",
        "",
        r#"{"id":"3"}"#,
        "todo-cancelled",
        None,
    );
    assert!(
        cancelled.success,
        "cancel operation failed: {}",
        cancelled.content
    );

    let list =
        deepx_tools::execution::execute_with_context("todo_list", "", r#"{}"#, "todo-list", None);
    assert!(list.success, "list failed: {}", list.content);
    let list_json: serde_json::Value =
        serde_json::from_str(&list.content).expect("structured list response");
    assert_eq!(list_json["counts"]["in_progress"], 1);
    assert_eq!(list_json["counts"]["completed"], 1);
    assert_eq!(list_json["counts"]["cancelled"], 1);
    assert_eq!(list_json["counts"]["pending"], 1);
    assert_eq!(list_json["items"][0]["id"], "T1");

    let status: serde_json::Value = serde_json::from_str(
        &deepx_tools::todo::todo_status_json("todo-contract").expect("status JSON"),
    )
    .expect("parse status JSON");
    assert_eq!(status["mode"], "manual");
    assert_eq!(status["goal_enabled"], false);
    assert_eq!(status["current_id"], "T1");
    assert_eq!(status["current_title"], "Working");
    assert_eq!(status["pending"], 1);
    assert_eq!(status["in_progress"], 1);
    assert_eq!(status["completed"], 1);
    assert_eq!(status["cancelled"], 1);
    assert_eq!(status["total"], 4);
    assert_eq!(status["items"][2]["status"], "cancelled");

    let activation =
        deepx_tools::todo::exec_todo_activate(&serde_json::json!({})).expect_err("Goal frozen");
    assert!(activation.contains("GOAL_FEATURE_FROZEN"));

    let mut store = deepx_tools::todo::load_todo().expect("load todo");
    store.mode = deepx_tools::todo::TodoMode::Goal;
    deepx_tools::todo::save_todo(&store).expect("save normalizes frozen Goal state");
    assert_eq!(
        deepx_tools::todo::load_todo().expect("reload todo").mode,
        deepx_tools::todo::TodoMode::Manual
    );

    std::fs::remove_dir_all(&temp_home).expect("remove isolated home");
}
