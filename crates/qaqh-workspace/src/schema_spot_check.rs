#[cfg(test)]
#[allow(clippy::module_inception)]
mod schema_spot_check {
    use crate::registration::build_tool_manager;

    /// SDK v2 全量形态验收：input schema 一律由 `Args` 类型生成——
    /// 必须是 object，且不得出现手写聚合 schema 残留的 oneOf/anyOf/allOf
    /// （类型生成路径从机制上不允许组合键）。
    ///
    /// 本 crate 内置 22 工具；subagent 面（+18）无法在此以 dev-dep 注入
    /// （workspace→subagent→workspace 循环 dev-dep 会使 lib 双编译、类型
    /// 不统一），其形态验收在 qaqh-subagent 自身测试与 runtime 侧全量面
    /// （prompt_and_tool_defs_char_budget）覆盖。
    #[test]
    fn all_defs_are_object_shaped_without_composition_keys() {
        let defs = build_tool_manager(&[]).all_defs();
        assert!(
            defs.len() >= 22,
            "builtin surface unexpectedly small: {}",
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
            for key in ["oneOf", "anyOf", "allOf", "$ref", "$defs"] {
                assert!(
                    !serialized.contains(key),
                    "{name}: composition key {key} leaked into generated schema"
                );
            }
            assert!(
                schema.get("$schema").is_none(),
                "{name}: wire-tax $schema must be absent"
            );
        }
    }

    /// deny_unknown_fields Args 的生成形态：additionalProperties=false。
    #[test]
    fn closed_args_generate_additional_properties_false() {
        let defs = build_tool_manager(&[]).all_defs();
        let by_name = |n: &str| defs.iter().find(|d| d.function.name == n).unwrap();
        for name in ["edit", "skill_activate", "todo_write"] {
            assert_eq!(
                by_name(name).function.parameters["additionalProperties"],
                serde_json::json!(false),
                "{name} must reject unknown fields"
            );
        }
    }

    /// 关键工具的生成 schema 抽查：required 集合与字段描述由类型 + doc comment
    /// 决定，防止字段改名/描述丢失悄悄破坏模型面。
    #[test]
    fn schema_descriptions_are_effective() {
        let defs = build_tool_manager(&[]).all_defs();
        let by_name = |n: &str| defs.iter().find(|d| d.function.name == n).unwrap();
        let params = |n: &str| &by_name(n).function.parameters["properties"];
        let required_of = |n: &str| -> Vec<String> {
            by_name(n).function.parameters["required"]
                .as_array()
                .map(|values| {
                    values
                        .iter()
                        .map(|v| v.as_str().unwrap_or_default().to_owned())
                        .collect()
                })
                .unwrap_or_default()
        };

        // read：批量 requests 优先（v2 去 oneOf 后仍需在描述中点名）。
        let read_desc = by_name("read").function.description.as_str();
        assert!(
            read_desc.contains("requests"),
            "read description must point at the requests field: {read_desc}"
        );
        for field in ["path", "start_line", "end_line", "if_hash", "requests"] {
            let f = &params("read")[field];
            assert!(
                f["description"].as_str().is_some_and(|d| !d.is_empty()),
                "read.{field} missing description"
            );
        }
        assert!(
            params("read")["if_hash"]["description"]
                .as_str()
                .unwrap()
                .contains("NOT_MODIFIED"),
            "read.if_hash description must explain NOT_MODIFIED"
        );

        // web_fetch：url 带 serde(default) → 生成 schema 不再 required
        //（v2 已知行为），缺 url 由 run() 校验兜底（missing_url）。
        assert!(
            !required_of("web_fetch").contains(&"url".to_string()),
            "web_fetch.url is serde(default); required set comes from the type"
        );
        assert!(
            params("web_fetch")["url"]["description"]
                .as_str()
                .is_some_and(|d| !d.is_empty()),
            "web_fetch.url missing description"
        );

        // edit：str_replace 三字段契约（path/old_str/new_str 必填），
        // 不得残留 legacy 字段。
        assert_eq!(
            required_of("edit"),
            vec![
                "path".to_string(),
                "old_str".to_string(),
                "new_str".to_string()
            ]
        );
        let edit_params = &by_name("edit").function.parameters;
        for legacy in [
            "hunks",
            "kind",
            "context_before",
            "context_after",
            "hint_line",
            "expected_hash",
            "dry_run",
        ] {
            assert!(
                !edit_params.to_string().contains(legacy),
                "edit schema still carries legacy field {legacy}"
            );
        }

        // write：保留"定点改动用 edit"的选择指引。
        let write_desc = by_name("write").function.description.as_str();
        assert!(
            write_desc.contains("use edit for targeted changes"),
            "write missing edit guidance: {write_desc}"
        );

        // Todo v4（全量覆写形态）：items 上限 20（P1 迁移曾丢失，靠
        // todo_contract 集成测试兜住后补回）；条目内 status 必填。
        let tw = params("todo_write");
        assert!(tw["items"].is_object(), "todo_write.items missing");
        assert!(
            tw["items"]["maxItems"].is_number(),
            "todo_write.items.maxItems missing: {tw}"
        );
        let tw_item = &tw["items"]["items"];
        assert_eq!(
            tw_item["required"].as_array().map(|r| r.len()),
            Some(1),
            "todo_write items[] must require exactly status"
        );

        // skills 三件套已上线，聚合名/validate 退役。
        for name in ["skill_activate", "skill_list", "skill_resource"] {
            let _ = by_name(name);
        }
        assert!(
            defs.iter()
                .all(|d| !matches!(d.function.name.as_str(), "skills" | "skill_validate")),
            "aggregate skills/validate must stay retired"
        );
    }
}

/// 能力迁移表 ↔ 注册表交叉验证（tool_capabilities 随 SDK 拆至
/// qaqh-tool-core；需要 build_tool_manager 的断言留在门面侧）。
mod tool_capabilities_cross_check {
    use crate::registration::build_tool_manager;
    use qaqh_policy::ToolCategory;
    use qaqh_tool_core::tool_api::Concurrency;
    use qaqh_tool_core::tool_capabilities::{builtin_capabilities, table_tool_names};

    #[test]
    fn table_covers_registry_exactly() {
        let registry: Vec<String> = build_tool_manager(&[])
            .all_defs()
            .into_iter()
            .map(|def| def.function.name)
            .collect();
        let table: Vec<String> = table_tool_names()
            .iter()
            .map(|name| (*name).to_owned())
            .collect();
        assert_eq!(
            table, registry,
            "迁移表必须与注册表词表逐项一致（新增/删除工具必须同步本表）"
        );
    }

    #[test]
    fn parallel_tools_are_pure_reads() {
        let manager = build_tool_manager(&[]);
        for name in table_tool_names() {
            let capabilities = builtin_capabilities(name).expect("表内条目");
            if capabilities.concurrency == Concurrency::Parallel {
                let category = manager.category_of(name).expect("注册工具");
                assert!(
                    matches!(category, ToolCategory::Read | ToolCategory::Net),
                    "{name} 声明 Parallel 但 category={category:?}（只读才可并行）"
                );
            }
        }
    }

    #[test]
    fn exclusive_tools_are_process_or_rewrite() {
        let manager = build_tool_manager(&[]);
        let exclusive: Vec<&str> = table_tool_names()
            .into_iter()
            .filter(|name| {
                builtin_capabilities(name).expect("表内条目").concurrency == Concurrency::Exclusive
            })
            .collect();
        assert_eq!(
            exclusive,
            vec!["exec", "journal", "process"],
            "独占档变更必须显式审查（spec §3.4）"
        );
        for name in exclusive {
            let category = manager.category_of(name).expect("注册工具");
            assert!(
                matches!(category, ToolCategory::Exec | ToolCategory::Write),
                "{name} 独占但 category={category:?}"
            );
        }
    }

    #[test]
    fn idempotent_tools_are_pure_reads() {
        let manager = build_tool_manager(&[]);
        for name in table_tool_names() {
            let capabilities = builtin_capabilities(name).expect("表内条目");
            if capabilities.idempotent {
                let category = manager.category_of(name).expect("注册工具");
                assert!(
                    matches!(category, ToolCategory::Read | ToolCategory::Net),
                    "{name} 声明幂等但 category={category:?}（重放安全仅限纯读取）"
                );
            }
        }
    }
}
