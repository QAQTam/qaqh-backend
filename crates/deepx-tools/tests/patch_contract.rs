use serde_json::json;

#[test]
fn patch_is_advertised_and_applies_a_hash_bound_unified_diff() {
    let workspace = tempfile::tempdir().expect("workspace");
    let target = workspace.path().join("example.txt");
    std::fs::write(&target, "one\ntwo\n").expect("fixture");

    deepx_tools::runtime::init_tools("patch-contract", &[], vec![]);
    deepx_tools::runtime::set_context("patch-contract", 4);
    deepx_tools::set_workspace(&workspace.path().to_string_lossy());

    let schema = deepx_tools::runtime::all_tools()
        .into_iter()
        .find(|definition| definition.function.name == "patch")
        .expect("patch schema must be visible to models");
    assert_eq!(
        schema.function.parameters["required"],
        json!(["path", "patch", "expected_hash"])
    );

    let read = deepx_tools::execution::execute_with_context(
        "read",
        "",
        r#"{"path":"example.txt"}"#,
        "patch-read",
        None,
    );
    assert!(read.success, "read failed: {}", read.content);
    let hash = serde_json::from_str::<serde_json::Value>(&read.content).expect("read JSON")["hash"]
        .as_str()
        .expect("read hash")
        .to_string();

    let patch = json!({
        "path": "example.txt",
        "expected_hash": hash,
        "patch": "--- a/example.txt\n+++ b/example.txt\n@@ -1,2 +1,2 @@\n one\n-two\n+three\n"
    });
    let mut stale_patch = patch.clone();
    stale_patch["expected_hash"] = json!("0".repeat(64));
    let stale = deepx_tools::execution::execute_with_context(
        "patch",
        "",
        &stale_patch.to_string(),
        "patch-stale",
        None,
    );
    assert!(!stale.success, "stale patch unexpectedly applied");
    assert_eq!(
        std::fs::read_to_string(&target).expect("unchanged"),
        "one\ntwo\n"
    );

    let mut preview_patch = patch.clone();
    preview_patch["dry_run"] = json!(true);
    let preview = deepx_tools::execution::execute_with_context(
        "patch",
        "",
        &preview_patch.to_string(),
        "patch-preview",
        None,
    );
    assert!(preview.success, "preview failed: {}", preview.content);
    assert_eq!(
        std::fs::read_to_string(&target).expect("preview unchanged"),
        "one\ntwo\n"
    );

    let applied = deepx_tools::execution::execute_with_context(
        "patch",
        "",
        &patch.to_string(),
        "patch-apply",
        None,
    );
    assert!(applied.success, "patch failed: {}", applied.content);
    assert_eq!(
        std::fs::read_to_string(target).expect("result"),
        "one\nthree\n"
    );
}
