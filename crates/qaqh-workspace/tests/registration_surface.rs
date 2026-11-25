//! P2-1a 反向环拆除后的耦合保留：子 crate 的 `register()` 粘合层与**具体**
//! `ToolManager` 的集成面必须继续在门面侧被钉住（子 crate 单测只经
//! `DescriptorRegistry` 断言 descriptor，不再回指本门面）。
//!
//! 覆盖 v4.5 P2-1a 从 `qaqh-file-tools` / `qaqh-process-tools` 内联测试迁出的
//! 「注册进具体 manager」断言：名字、descriptor 同源、schema 类型生成。

use qaqh_file_tools::FileRuntime;
use qaqh_workspace::ToolManager;

#[test]
fn file_tool_registrars_land_on_the_concrete_manager() {
    let files = std::sync::Arc::new(FileRuntime::default());
    let mut manager = ToolManager::new();
    qaqh_file_tools::edit::register(&mut manager, &files);
    qaqh_file_tools::file_mutate::register(&mut manager, &files);
    qaqh_file_tools::file_query::register(&mut manager, &files);
    qaqh_file_tools::file_glob::register(&mut manager);
    qaqh_file_tools::apply_patch::register(&mut manager, &files);
    qaqh_file_tools::copy_range::register(&mut manager, &files);
    qaqh_file_tools::grep_tool::register(&mut manager, &files);
    qaqh_file_tools::confirm_apply::register(&mut manager, &files);
    qaqh_file_tools::read_image::register(&mut manager);

    for name in [
        "edit",
        "write",
        "delete",
        "read",
        "glob",
        "grep",
        "apply_patch",
        "copy_range",
        "confirm_apply",
        "read_image",
    ] {
        let registered = manager
            .builtin(name)
            .unwrap_or_else(|| panic!("{name} must be on the typed execution surface"));
        assert_eq!(registered.descriptor.name.as_str(), name, "{name} name");
        // descriptor 与子 crate DescriptorRegistry 单测同源：schema 由 Args 类型生成。
        assert!(
            registered.descriptor.input_schema.get("properties").is_some(),
            "{name} input schema must be type-generated: {}",
            registered.descriptor.input_schema
        );
    }
    // lookup 与 builtin 同层（description 供模型面）。
    assert!(manager.lookup("apply_patch").expect("lookup").description.contains("FIRST hit"));
}

#[test]
fn process_tool_registrar_lands_exec_command_and_write_stdin() {
    let mut manager = ToolManager::new();
    qaqh_process_tools::exec_session::register(&mut manager);
    for name in ["exec_command", "write_stdin"] {
        assert!(
            manager.builtin(name).is_some(),
            "{name} must be registered by exec_session::register"
        );
    }
}
