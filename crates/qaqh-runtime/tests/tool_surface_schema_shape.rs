//! SDK v2 全量工具面形态验收（内置 22 + subagent 18 = 40 工具）。
//!
//! input schema 一律由 `Args` 类型生成：必须是 object，不得出现手写聚合
//! schema 残留的 oneOf/anyOf/allOf 与 `$defs`/`$schema` wire 税。
//! qaqh-workspace 内部的 schema_spot_check 只见本 crate 22 工具（循环
//! dev-dep 不可行）；这里经 registrar 注入 subagent 后覆盖全部 40。

use qaqh_workspace::registration::build_tool_manager;

#[test]
fn full_surface_schemas_are_object_shaped_without_composition_keys() {
    let defs = build_tool_manager(&[qaqh_subagent::register]).all_defs();
    assert!(
        defs.len() >= 40,
        "full surface unexpectedly small: {}",
        defs.len()
    );
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
