#[cfg(test)]
#[allow(clippy::module_inception)]
mod schema_spot_check {
    use crate::registration::build_tool_manager;

    #[test]
    fn schema_descriptions_are_effective() {
        let defs = build_tool_manager(&[]).all_defs();
        let by_name = |n: &str| defs.iter().find(|d| d.function.name == n).unwrap();
        let params = |n: &str| &by_name(n).function.parameters["properties"];

        // process: action enum 带描述
        let pa = &params("process")["action"];
        assert!(
            pa["description"].as_str().unwrap().contains("check"),
            "process.action missing per-action description"
        );

        // read_image: anyOf 互斥（image_index / path）
        let img = &by_name("read_image").function.parameters;
        assert!(img["anyOf"].is_array(), "read_image missing anyOf");
        assert!(
            img["anyOf"][0]["required"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("image_index"))
        );

        // web_fetch: url required
        let web = &by_name("web_fetch").function.parameters;
        assert!(
            web["required"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("url")),
            "web_fetch.url not required"
        );

        // Todo v4（全量覆写形态）：
        // write = items-only + 条目内 status 必填（写即状态）。
        let tw = params("todo_write");
        assert!(tw["items"].is_object(), "todo_write.items missing");
        assert!(
            tw["items"]["maxItems"].is_number(),
            "todo_write.items.maxItems missing"
        );
        let tw_item = &tw["items"]["items"];
        assert!(
            tw_item["required"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("status")),
            "todo_write items[].status must be required (write-as-state)"
        );
        // update = 单一形态（无 ids/updates，required [id, status]）。
        let tu = params("todo_update");
        assert!(
            tu.get("ids").is_none() && tu.get("updates").is_none(),
            "todo_update 应为单一形态（批量/updates 已移除）"
        );
        assert!(
            by_name("todo_update").function.parameters["required"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("status")),
            "todo_update required 应含 status"
        );
        assert!(
            !by_name("todo_update")
                .function
                .description
                .contains("updates"),
            "todo_update description 不应再宣传 updates 形态"
        );

        // 文件修改工具选择指引
        let write_desc = by_name("write").function.description.as_str();
        assert!(
            write_desc.contains("use edit for targeted changes"),
            "write missing edit guidance: {write_desc}"
        );

        // read：单文件模式字段必须有描述（曾缺失导致模型不知 if_hash 语义）
        for field in ["path", "start_line", "end_line", "if_hash"] {
            let f = &params("read")[field];
            assert!(
                f["description"].as_str().is_some_and(|d| !d.is_empty()),
                "read.{field} missing description"
            );
        }
        let if_hash = &params("read")["if_hash"];
        assert!(
            if_hash["description"]
                .as_str()
                .unwrap()
                .contains("NOT_MODIFIED"),
            "read.if_hash description must explain NOT_MODIFIED"
        );

        // edit：描述不得残留历史版本号/旧名（schema 更新不及时的回归保护）
        let edit_desc = by_name("edit").function.description.as_str();
        assert!(
            !edit_desc.contains("v2"),
            "edit description must not mention v2"
        );
        assert!(
            !edit_desc.contains("edit_file_v2"),
            "edit description must not mention legacy name"
        );

        // edit：str_replace 三字段契约（path/old_str/new_str），不得残留 v2 字段。
        let edit_params = &by_name("edit").function.parameters;
        let required: Vec<&str> = edit_params["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(required, vec!["path", "old_str", "new_str"]);
        let props = edit_params["properties"].as_object().unwrap();
        assert_eq!(props.len(), 4, "edit schema: {edit_params}");
        assert_eq!(props["replace_all"]["type"], "boolean");
        assert!(!required.contains(&"replace_all"));
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
    }
}
