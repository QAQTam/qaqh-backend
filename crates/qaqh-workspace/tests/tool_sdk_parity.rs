//! P3 gate: builtin registry and capability table stay in lockstep.

#![allow(clippy::unwrap_used)] // integration test fixture

use qaqh_workspace::registration::build_tool_manager;
use qaqh_workspace::tool_capabilities::{builtin_capabilities, table_tool_names};

#[test]
fn builtin_sdk_parity_matches_registry_capabilities_and_dynamic_fallback() {
    let manager = build_tool_manager(&[]);
    let defs = manager.all_defs();
    let registry: Vec<&str> = defs
        .iter()
        .map(|definition| definition.function.name.as_str())
        .collect();
    let capabilities = table_tool_names();

    assert_eq!(
        registry, capabilities,
        "builtin registry order and capability table must match exactly"
    );
    assert_eq!(registry.len(), 19, "P3 freezes the 19 builtin tools");
    for definition in &defs {
        assert!(
            !definition.function.description.trim().is_empty(),
            "{} descriptor description must not be empty",
            definition.function.name
        );
        assert!(
            definition.function.parameters.is_object(),
            "{} descriptor schema must be a JSON object",
            definition.function.name
        );
        assert!(
            builtin_capabilities(&definition.function.name).is_some(),
            "{} must have a frozen capability row",
            definition.function.name
        );
    }

    assert!(
        builtin_capabilities("mcp__demo__echo").is_none(),
        "dynamic tools must fall back to conservative defaults, not a builtin row"
    );
}
