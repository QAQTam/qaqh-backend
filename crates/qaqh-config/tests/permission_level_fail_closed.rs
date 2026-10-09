//! BUG-2026-09-13-15 + 2026-10-03 三档迁移:permission_level 配置面回归测试。
//!
//! 2026-10-03 起权限为三档制(read-only / workspace-write / skip-permissions),
//! 取代旧 L1–L4。本文件守卫两条不变式:
//! 1. 写入口(`ConfigPatch.permissionLevel`)拒绝 1..=3 之外的值,且不污染已加载
//!    配置(校验先于变更);
//! 2. 磁盘旧数值在 load 阶段迁移(1/2→read-only,3→workspace-write,
//!    4→skip-permissions);无法识别的值 fail-closed 收敛到最严档 1,绝不静默
//!    放大权限。

use std::io::Write;

use qaqh_workspace::PermissionLevel;
use qaqh_workspace::permission::{ToolCategory, needs_permission};

/// 写入口:非法 permissionLevel 必须被拒绝,且不污染配置。
#[test]
fn config_patch_rejects_invalid_permission_level_without_mutating() {
    let mut cfg = qaqh_config::Config {
        permission_level: 2,
        ..Default::default()
    };

    for invalid in [0u64, 4, 5, 255] {
        let patch: qaqh_config_api::ConfigPatch =
            serde_json::from_value(serde_json::json!({ "permissionLevel": invalid }))
                .expect("deserialize patch");
        let result = qaqh_config::dto::apply_patch(&mut cfg, &patch);
        assert!(
            result.is_err(),
            "permissionLevel={invalid} must be rejected by the config write port"
        );
        assert_eq!(
            cfg.permission_level, 2,
            "rejected patch must not leave a poisoned permission_level behind"
        );
    }

    // 合法值 1..=3 照常写入。
    for valid in [1u64, 2, 3] {
        let patch_ok: qaqh_config_api::ConfigPatch =
            serde_json::from_value(serde_json::json!({ "permissionLevel": valid }))
                .expect("deserialize patch");
        qaqh_config::dto::apply_patch(&mut cfg, &patch_ok).expect("valid level applies");
        assert_eq!(cfg.permission_level, valid as u8);
    }
}

/// 加载面:磁盘上的旧四档数值迁移到新三档,语义单调不放大。
#[test]
fn config_load_migrates_legacy_four_tier_values() {
    for (legacy, expect_tier) in [
        (1u8, PermissionLevel::ReadOnly),      // 旧 MaxLockdown
        (2, PermissionLevel::ReadOnly),        // 旧 ReadFree ≈ read-only
        (3, PermissionLevel::WorkspaceWrite),  // 旧 WorkspaceFree
        (4, PermissionLevel::SkipPermissions), // 旧 Unrestricted
    ] {
        let temp_home = std::env::temp_dir().join(format!(
            "qaqh-permission-mig-{}-{}-{}",
            std::process::id(),
            legacy,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let config_dir = temp_home.join("qaqh");
        std::fs::create_dir_all(&config_dir).expect("create config dir");
        let config_path = config_dir.join("config.toml");
        let mut file = std::fs::File::create(&config_path).expect("create config.toml");
        writeln!(file, "permission_level = {legacy}").expect("write config.toml");
        drop(file);

        let store = qaqh_types::ConfigStore::new(config_path);
        let cfg = qaqh_config::Config::load_from_paths_with(
            store,
            qaqh_config::secrets::SecretStore::new(temp_home.join("secrets.toml")),
        )
        .expect("load config");

        assert_eq!(
            cfg.permission_level,
            expect_tier.to_u8(),
            "legacy permission_level={legacy} must migrate to {}",
            expect_tier.as_str()
        );

        let _ = std::fs::remove_dir_all(&temp_home);
    }
}

/// 加载面:新键 permission_tier 严格 1..=3;同时存在时新键优先于旧键
/// (防旧键数字歧义越权)。
#[test]
fn config_load_prefers_tier_key_and_validates_range() {
    for (tier, level, expect) in [
        (1u8, Some(4u8), PermissionLevel::ReadOnly), // 新键胜过旧键
        (3, Some(1), PermissionLevel::SkipPermissions), // 新键胜过旧键
        (4, None, PermissionLevel::SandboxRun),      // 档位 4 = sandbox-run（ADR 2026-10-09 决策 5）
        (5, None, PermissionLevel::ReadOnly),        // 新键越界 → 最严档
        (0, Some(3), PermissionLevel::ReadOnly),     // 新键越界 → 最严档
    ] {
        let temp_home = std::env::temp_dir().join(format!(
            "qaqh-permission-tier-{}-{}-{}",
            std::process::id(),
            tier,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let config_dir = temp_home.join("qaqh");
        std::fs::create_dir_all(&config_dir).expect("create config dir");
        let config_path = config_dir.join("config.toml");
        let mut file = std::fs::File::create(&config_path).expect("create config.toml");
        if let Some(pl) = level {
            writeln!(file, "permission_tier = {tier}").unwrap();
            writeln!(file, "permission_level = {pl}").unwrap();
        } else {
            writeln!(file, "permission_tier = {tier}").unwrap();
        }
        drop(file);

        let store = qaqh_types::ConfigStore::new(config_path);
        let cfg = qaqh_config::Config::load_from_paths_with(
            store,
            qaqh_config::secrets::SecretStore::new(temp_home.join("secrets.toml")),
        )
        .expect("load config");
        assert_eq!(
            cfg.permission_level,
            expect.to_u8(),
            "tier={tier} level={level:?} must resolve to {}",
            expect.as_str()
        );
        let _ = std::fs::remove_dir_all(&temp_home);
    }
}

/// 加载面:磁盘上手写的非法 permission_level 不得静默变成免审档。
#[test]
fn config_load_clamps_invalid_persisted_permission_level() {
    for raw in [0u8, 5, 255] {
        let temp_home = std::env::temp_dir().join(format!(
            "qaqh-permission-load-{}-{}-{}",
            std::process::id(),
            raw,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let config_dir = temp_home.join("qaqh");
        std::fs::create_dir_all(&config_dir).expect("create config dir");
        let config_path = config_dir.join("config.toml");
        let mut file = std::fs::File::create(&config_path).expect("create config.toml");
        writeln!(file, "permission_level = {raw}").expect("write config.toml");
        drop(file);

        let store = qaqh_types::ConfigStore::new(config_path);
        let cfg = qaqh_config::Config::load_from_paths_with(
            store,
            qaqh_config::secrets::SecretStore::new(temp_home.join("secrets.toml")),
        )
        .expect("load config");

        let resolved = PermissionLevel::from_u8(cfg.permission_level);
        assert_ne!(
            resolved,
            PermissionLevel::SkipPermissions,
            "persisted permission_level={raw} must not load as skip-permissions"
        );
        assert_eq!(
            resolved,
            PermissionLevel::ReadOnly,
            "persisted permission_level={raw} must conservatively resolve to read-only"
        );

        // 端到端反证:加载出来的最严档对 workspace 内写仍要求审批。
        let workspace = temp_home.join("ws");
        std::fs::create_dir_all(&workspace).expect("create workspace");
        let decision = needs_permission(
            resolved,
            "write",
            &serde_json::json!({ "path": workspace.join("a.txt") }),
            &workspace,
            &std::collections::HashSet::new(),
            ToolCategory::Write,
        );
        assert!(
            matches!(decision, qaqh_workspace::PermissionDecision::AskUser { .. }),
            "loaded level for permission_level={raw} must still ask for approval"
        );

        let _ = std::fs::remove_dir_all(&temp_home);
    }
}
