//! P3 typed-output contract for `skill_activate` (v2 skills 三件套之一)。

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
fn skill_activate_typed_activation_carries_effect_and_same_source_output() {
    let workspace = isolate_workspace();
    qaqh_workspace::runtime::init_tools("skills-typed", &[], vec![]);
    qaqh_workspace::set_workspace(&workspace.path().to_string_lossy());
    let ctx = qaqh_workspace::runtime::ToolCtx {
        session_id: "skills-typed".into(),
        permission_level: 3,
        mode: 0,
        workspace_root: Some(workspace.path().to_string_lossy().into_owned()),
    };

    let args = json!({"name": "typed-skill"});
    let executed = qaqh_workspace::execution::execute_with_context(
        "skill_activate",
        &args.to_string(),
        "skills-typed-call",
        None,
        &ctx,
    );
    assert!(
        executed.success,
        "typed skill activation failed: {}",
        executed.content
    );
    assert_eq!(executed.result.data["skill"], "typed-skill");
    assert!(
        executed.result.display().is_some(),
        "typed activation output must carry canonical display"
    );
    assert_eq!(executed.skill_effects.len(), 1);
    assert!(
        executed.result.model_text().contains("typed-skill")
            && executed.result.model_text().contains("activated"),
        "model projection must carry activation text: {}",
        executed.result.model_text()
    );

    let display = qaqh_workspace::runtime::project_tool_display_from_result(
        "skill_activate",
        &args,
        &executed.result,
    )
    .expect("skill_activate display projection");
    assert!(
        display.summary.is_some(),
        "activation display must carry a human summary"
    );
}
