//! ADR-0018：退役 [subagent] 段的一次性迁移回归。
//! 该段由 daemon 历次 save 自写，存量安装人人都有；deny_unknown_fields
//! 不得因此拒启动——读入口剥离并写回，此后磁盘永不再现。
//! 其余未知键仍 fail-closed（不做静默兼容）。

use std::path::{Path, PathBuf};

fn setup(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "qaqh-config-subagent-migration-{}-{}",
        std::process::id(),
        tag
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create temp data root");
    // SAFETY: this test file is a single serial test function; no parallel env var races.
    unsafe { std::env::set_var("QAQH_DATA_DIR", &root) };
    root
}

fn write_config(root: &Path, toml: &str) {
    std::fs::write(root.join("config.toml"), toml).expect("write test config");
}

#[test]
fn retired_subagent_section_is_stripped_once_and_other_keys_stay_fail_closed() {
    // 1) 存量配置（daemon 自写的 [subagent] 段 + 用户策略）必须可加载。
    let root = setup("strip");
    write_config(
        &root,
        r#"theme = "dark"
session_idle_unload_secs = 900

[subagent]
timeout_secs = 240
default_tools = ["read", "exec_command"]
max_depth = 2
message_in_flight_per_pair = 16
message_outbound_per_sender = 1024
"#,
    );
    let cfg = qaqh_config::Config::load().expect("存量 [subagent] 段不得拒启动");
    assert_eq!(cfg.theme.as_deref(), Some("dark"));
    assert_eq!(cfg.session_idle_unload_secs, 900);

    // 2) 迁移写回：磁盘上该段就此消失，重载不再触发迁移。
    let on_disk = std::fs::read_to_string(root.join("config.toml")).expect("read back");
    assert!(
        !on_disk.contains("[subagent]"),
        "退役段必须被一次性剥离: {on_disk}"
    );
    let reloaded = qaqh_config::Config::load().expect("reload after migration");
    assert_eq!(reloaded.theme.as_deref(), Some("dark"));
    assert_eq!(reloaded.session_idle_unload_secs, 900);

    // 3) save 不再写出该段（穷举映射已删）。
    qaqh_config::Config::update(|cfg| {
        cfg.session_idle_unload_secs = 60;
        Ok(())
    })
    .expect("update");
    let after_save = std::fs::read_to_string(root.join("config.toml")).expect("read after save");
    assert!(!after_save.contains("subagent"), "save 不得复活退役段: {after_save}");

    // 4) 其它未知段仍 fail-closed：迁移不是通用兼容层。
    let root = setup("unknown-still-rejected");
    write_config(
        &root,
        r#"[some_retired_other_section]
foo = 1
"#,
    );
    let error = qaqh_config::Config::load().expect_err("未知段必须被拒绝");
    assert!(
        error.to_string().contains("重新配置"),
        "报错必须提示重新配置: {error}"
    );
}
