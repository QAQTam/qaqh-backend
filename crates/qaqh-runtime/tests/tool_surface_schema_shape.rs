//! SDK v2 生产工具面形态验收（仅内置工具，子代理及 task/board 暂时不注册）。
//!
//! input schema 一律由 `Args` 类型生成：必须是 object，不得出现手写聚合
//! schema 残留的 oneOf/anyOf/allOf 与 `$defs`/`$schema` wire 税。
//! 使用与生产 agent 相同的注册入口，避免测试继续暴露已停用的子代理工具。

use qaqh_runtime::agent::state::agent::agent_tool_registrars;
use qaqh_workspace::registration::build_tool_manager;

#[test]
fn full_surface_schemas_are_object_shaped_without_composition_keys() {
    let defs = build_tool_manager(&agent_tool_registrars()).all_defs();
    let builtin_names: Vec<_> = build_tool_manager(&[])
        .all_defs()
        .into_iter()
        .map(|def| def.function.name)
        .collect();
    let names: Vec<_> = defs.iter().map(|def| def.function.name.clone()).collect();
    assert_eq!(names, builtin_names, "production must expose only builtins");
    for def in &defs {
        let name = def.function.name.as_str();
        let schema = &def.function.parameters;
        assert!(
            schema.is_object(),
            "{name}: input_schema must be a JSON object"
        );
        assert_eq!(schema["type"], "object", "{name}: input_schema type");
        let serialized = schema.to_string();
        for key in ["oneOf", "anyOf", "allOf", "$ref", "$defs", "$schema"] {
            assert!(
                !serialized.contains(key),
                "{name}: composition/wire-tax key {key} leaked into generated schema"
            );
        }
    }
}

#[test]
fn production_registry_omits_subagent_and_collaboration_tools() {
    let manager = build_tool_manager(&agent_tool_registrars());
    let defs = manager.all_defs();
    for name in [
        "spawn_subagent",
        "list_agents",
        "send_message",
        "followup_task",
        "steer_agent",
        "interject_agent",
        "wait_agent",
        "interrupt_agent",
        "task_create",
        "task_claim",
        "task_update",
        "task_close",
        "task_list",
        "board_channel_create",
        "board_thread_create",
        "board_post",
        "board_subscribe",
        "board_list",
    ] {
        assert!(manager.lookup(name).is_none(), "{name} must not be callable");
        assert!(
            defs.iter().all(|def| def.function.name != name),
            "{name} must not be advertised"
        );
        assert!(
            manager
                .search_tools(name, 100)
                .iter()
                .all(|hit| hit.name != name),
            "{name} must not be discoverable through tool_search"
        );
    }
    for name in ["read", "exec", "tool_search", "todo_write", "todo_update"] {
        assert!(manager.lookup(name).is_some(), "{name} must remain available");
    }
}
