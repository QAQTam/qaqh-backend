//! P3 typed-output contract for the `skills` tool.

#![allow(clippy::unwrap_used)] // integration test fixture

use serde_json::json;

fn isolate_workspace() -> tempfile::TempDir {
    let temp = tempfile::tempdir().expect("temp workspace");
    let skill_dir = temp.path().join(".agents/skills/typed-skill");
    std::fs::create_dir_all(&skill_dir).expect("skill dir");
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: typed-skill\ndescription: Typed skill fixture.\n---\nTYPED_SKILL_BODY\n",
    )
    .expect("skill body");
    temp
}

#[test]
fn skills_typed_activation_carries_effect_and_same_source_output() {
    let workspace = isolate_workspace();
    qaqh_workspace::runtime::init_tools("skills-typed", &[], vec![]);
    qaqh_workspace::set_workspace(&workspace.path().to_string_lossy());
    let ctx = qaqh_workspace::runtime::ToolCtx {
        session_id: "skills-typed".into(),
        permission_level: 4,
        mode: 0,
        workspace_root: Some(workspace.path().to_string_lossy().into_owned()),
    };

    let args = json!({"action": "activate", "name": "typed-skill"});
    let executed = qaqh_workspace::execution::execute_with_context(
        "skills",
        "",
        &args.to_string(),
        "skills-typed-call",
        None,
        &ctx,
    );
    assert!(
        executed.success,
        "typed skills activation failed: {}",
        executed.content
    );
    assert_eq!(executed.result.data["skill"], "typed-skill");
    assert_eq!(executed.skill_effects.len(), 1);
    assert!(
        executed.result.model_text().contains("typed-skill")
            && executed.result.model_text().contains("activated"),
        "model projection must carry activation text: {}",
        executed.result.model_text()
    );

    let display = qaqh_workspace::runtime::project_tool_display_from_result(
        "skills",
        &args,
        &executed.result,
    )
    .expect("skills display projection");
    assert!(
        display.summary.is_some(),
        "skills display must carry a human summary"
    );
}
