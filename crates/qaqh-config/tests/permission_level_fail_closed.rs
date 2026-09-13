//! BUG-2026-09-13-15：`permission_level` 配置面 fail-open 回归测试。
//!
//! 缺陷：`permission_level` 是裸 `u8`，写入/加载路径均不校验值域；磁盘上
//! 手写 `permission_level = 0`（或越界值）会被静默当作 Level 4（免审批），
//! 反向放大工具调用权限。
//!
//! 修复预期（fail-closed）：
//! 1. `ConfigPatch.permissionLevel` 写入口显式拒绝 1..=4 之外的值，且不污染
//!    已加载配置（校验先于变更）；
//! 2. 磁盘 config.toml 中已存在的非法值在 load 阶段被收敛到最严档，不静默
//!    落成 Unrestricted。

use std::io::Write;

use qaqh_workspace::PermissionLevel;
use qaqh_workspace::permission::{ToolCategory, needs_permission};

/// 写入口：非法 permissionLevel 必须被拒绝，且不污染配置。
#[test]
fn config_patch_rejects_invalid_permission_level_without_mutating() {
    let mut cfg = qaqh_config::Config {
        permission_level: 3,
        ..Default::default()
    };

    for invalid in [0u64, 5, 255] {
        let patch: qaqh_config_api::ConfigPatch =
            serde_json::from_value(serde_json::json!({ "permissionLevel": invalid }))
                .expect("deserialize patch");
        let result = qaqh_config::dto::apply_patch(&mut cfg, &patch);
        assert!(
            result.is_err(),
            "permissionLevel={invalid} must be rejected by the config write port"
        );
        assert_eq!(
            cfg.permission_level, 3,
            "rejected patch must not leave a poisoned permission_level behind"
        );
    }

    // 合法值照常写入。
    let patch_ok: qaqh_config_api::ConfigPatch =
        serde_json::from_value(serde_json::json!({ "permissionLevel": 2 }))
            .expect("deserialize patch");
    qaqh_config::dto::apply_patch(&mut cfg, &patch_ok).expect("valid level applies");
    assert_eq!(cfg.permission_level, 2);
}

/// 加载面：磁盘上手写的非法 permission_level 不得静默变成 Level 4。
#[test]
fn config_load_clamps_invalid_persisted_permission_level() {
    for (raw, expect_locked_down) in [(0u8, true), (5, true), (255, true)] {
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
            PermissionLevel::Unrestricted,
            "persisted permission_level={raw} must not load as Unrestricted"
        );
        assert_eq!(
            resolved,
            PermissionLevel::MaxLockdown,
            "persisted permission_level={raw} must conservatively resolve to MaxLockdown"
        );

        if expect_locked_down {
            // 端到端反证：加载出来的档位对 workspace 内写仍要求审批。
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
        }

        let _ = std::fs::remove_dir_all(&temp_home);
    }
}
