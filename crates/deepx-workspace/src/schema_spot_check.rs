#[cfg(test)]
mod schema_spot_check {
    use crate::registration::build_tool_manager;

    #[test]
    fn schema_descriptions_are_effective() {
        let defs = build_tool_manager(&[]).all_defs();
        let by_name = |n: &str| defs.iter().find(|d| d.function.name == n).unwrap();
        let params = |n: &str| &by_name(n).function.parameters["properties"];

        // process: action enum 带描述
        let pa = &params("process")["action"];
        assert!(pa["description"].as_str().unwrap().contains("check"), "process.action missing per-action description");

        // image: anyOf 互斥
        let img = &by_name("image").function.parameters;
        assert!(img["anyOf"].is_array(), "image missing anyOf");
        assert!(img["anyOf"][0]["required"].as_array().unwrap().contains(&serde_json::json!("image_index")));

        // web: url required
        let web = &by_name("web").function.parameters;
        assert!(web["required"].as_array().unwrap().contains(&serde_json::json!("url")), "web.url not required");

        // task: id 描述
        let tid = &params("task")["id"];
        assert!(tid["description"].as_str().unwrap().contains("Omit for action=create"), "task.id description missing");

        // 文件修改工具选择指引
        for (tool, needle) in [
            ("apply_patch", "use edit for single-file"),
            ("edit", "prefer apply_patch"),
            ("edit_block", "For single-string replacement use edit"),
            ("write", "use edit/edit_block for targeted changes"),
            ("search", "it does not return file bodies"),
        ] {
            let desc = by_name(tool).function.description.as_str();
            assert!(desc.contains(needle), "{tool} missing guidance: {needle}");
        }
    }
}
