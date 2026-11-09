//! theme / notifications_enabled 的持久化回归。
//! 旧格式（顶层模型族键）按统一模型配置 Spec §3.5 拒绝并提示重新配置。

use std::path::{Path, PathBuf};

fn setup(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "qaqh-config-theme-notifications-{}-{}",
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
fn theme_and_notifications_roundtrip_and_legacy_rejected() {
    // 1) 新契约配置：theme + notifications_enabled 正确载入。
    let root = setup("roundtrip");
    write_config(
        &root,
        r#"theme = "dark"
notifications_enabled = false
"#,
    );
    let cfg = qaqh_config::Config::load().expect("load with new fields");
    assert_eq!(cfg.theme.as_deref(), Some("dark"));
    assert_eq!(cfg.notifications_enabled, Some(false));

    // 2) 单写口更新后重载，验证落盘。
    let root = setup("update");
    write_config(&root, "");
    let saved = qaqh_config::Config::update(|cfg| {
        cfg.theme = Some("dark-gray".to_string());
        cfg.notifications_enabled = Some(true);
        Ok(())
    })
    .expect("update ok");
    assert_eq!(saved.theme.as_deref(), Some("dark-gray"));
    assert_eq!(saved.notifications_enabled, Some(true));
    let reloaded = qaqh_config::Config::load().expect("reload after update");
    assert_eq!(reloaded.theme.as_deref(), Some("dark-gray"));
    assert_eq!(reloaded.notifications_enabled, Some(true));

    // 3) 空 theme = 跟随系统。
    let root = setup("empty-theme");
    write_config(
        &root,
        r#"theme = ""
notifications_enabled = false
"#,
    );
    let cfg = qaqh_config::Config::load().expect("load empty theme");
    assert_eq!(
        cfg.theme, None,
        "empty theme should normalize to None/system"
    );
    assert_eq!(cfg.notifications_enabled, Some(false));

    // 4) 旧格式（顶层模型族键）必须被拒绝并提示重新配置，而不是静默加载。
    let root = setup("legacy-rejected");
    write_config(
        &root,
        r#"provider_id = "deepseek"
max_tokens = 16384
"#,
    );
    let error = qaqh_config::Config::load().expect_err("旧格式配置必须被拒绝");
    assert!(
        error.to_string().contains("重新配置"),
        "报错必须提示重新配置: {error}"
    );
}
