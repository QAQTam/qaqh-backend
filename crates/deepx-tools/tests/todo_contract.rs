#[test]
fn public_schema_exposes_the_activation_path_handled_by_the_engine() {
    let mut manager = deepx_tools::ToolManager::new();
    deepx_tools::todo::register(&mut manager);
    let definition = manager
        .all_defs()
        .into_iter()
        .find(|definition| definition.function.name == "todo")
        .expect("todo definition");
    let actions = definition.function.parameters["properties"]["action"]["enum"]
        .as_array()
        .expect("todo action enum");

    assert!(
        actions.iter().any(|action| action == "activate"),
        "todo activate must be visible to the model so the review flow is reachable"
    );
}

#[test]
fn crud_accepts_numeric_ids_and_status_round_trips_to_the_frontend_contract() {
    let temp_home = std::env::temp_dir().join(format!(
        "deepx-todo-contract-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&temp_home).expect("create isolated home");

    // This test binary owns a unique process and executes this test serially.
    unsafe { std::env::set_var("USERPROFILE", &temp_home) };
    deepx_tools::runtime::init_tools("todo-contract", &[], vec![]);
    deepx_tools::runtime::set_context("todo-contract", 4);

    let create = deepx_tools::execution::execute_with_context(
        "todo",
        "",
        r#"{"action":"create","title":"Trace todo chain","description":"backend to UI"}"#,
        "todo-create",
        None,
    );
    assert!(create.success, "create failed: {}", create.content);

    let update = deepx_tools::execution::execute_with_context(
        "todo",
        "",
        r#"{"action":"update","id":1,"status":"completed","evidence":"verified"}"#,
        "todo-update",
        None,
    );
    assert!(
        update.success,
        "numeric ID update failed: {}",
        update.content
    );

    let list = deepx_tools::execution::execute_with_context(
        "todo",
        "",
        r#"{"action":"list"}"#,
        "todo-list",
        None,
    );
    assert!(list.success, "list failed: {}", list.content);
    assert!(
        list.content.contains("T1:"),
        "list omitted T1: {}",
        list.content
    );
    assert!(
        !list.content.contains("TT1"),
        "list doubled ID prefix: {}",
        list.content
    );

    let status: serde_json::Value = serde_json::from_str(
        &deepx_tools::todo::todo_status_json("todo-contract").expect("status JSON"),
    )
    .expect("parse status JSON");
    assert_eq!(status["mode"], "manual");
    assert_eq!(status["completed"], 1);
    assert_eq!(status["total"], 1);
    assert_eq!(status["items"][0]["id"], "T1");
    assert_eq!(status["items"][0]["status"], "completed");

    let create_second = deepx_tools::execution::execute_with_context(
        "todo",
        "",
        r#"{"action":"create","title":"Activate review","description":"commit after approval"}"#,
        "todo-create-second",
        None,
    );
    assert!(
        create_second.success,
        "second create failed: {}",
        create_second.content
    );

    let activate = deepx_tools::todo::exec_todo_activate(&serde_json::json!({}))
        .expect("approved activation commits");
    assert!(
        activate.contains("Starting: T2"),
        "activation returned the wrong ID: {activate}"
    );
    assert!(
        !activate.contains("TT2"),
        "activation doubled ID prefix: {activate}"
    );

    let active_status: serde_json::Value = serde_json::from_str(
        &deepx_tools::todo::todo_status_json("todo-contract").expect("active status JSON"),
    )
    .expect("parse active status JSON");
    assert_eq!(active_status["mode"], "goal");
    assert_eq!(active_status["current_id"], "T2");
    assert_eq!(active_status["items"][0]["status"], "in_progress");

    let complete = deepx_tools::execution::execute_with_context(
        "todo",
        "",
        r#"{"action":"step_complete","id":2,"summary":"approved flow verified"}"#,
        "todo-step-complete",
        None,
    );
    assert!(
        complete.success,
        "numeric step completion failed: {}",
        complete.content
    );

    let completed_status: serde_json::Value = serde_json::from_str(
        &deepx_tools::todo::todo_status_json("todo-contract").expect("completed status JSON"),
    )
    .expect("parse completed status JSON");
    assert_eq!(completed_status["mode"], "manual");
    assert_eq!(completed_status["completed"], 1);
    assert_eq!(completed_status["items"][0]["status"], "completed");

    std::fs::remove_dir_all(&temp_home).expect("remove isolated home");
}
