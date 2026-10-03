//! BUG-2026-09-13-15 + 2026-10-03 三档制:`PermissionLevel::from_u8` fail-closed 回归测试。
//!
//! 历史:旧实现 `from_u8` 的 `_ => Self::Unrestricted` 分支把任何非法档位
//! 静默升级为旧 L4(免审批),fail-open 反向放大权限;修复后收敛到最严档。
//! 2026-10-03 起档位为三档(read-only=1 / workspace-write=2 / skip-permissions=3),
//! 取代旧 L1–L4;非法值 fail-closed 落 read-only(最严档)。
//!
//! 配置载荷/加载面(`permissionLevel` patch、磁盘 config.toml 旧值迁移)的回归
//! 用例见 `qaqh-config` 侧(`permission_level_fail_closed.rs`)。

use qaqh_workspace::permission::{PermissionDecision, ToolCategory, needs_permission};
use qaqh_workspace::{Admission, PermissionLevel, ToolInvocation, admit};
use std::collections::HashSet;
use std::path::Path;

/// 非法档位必须保守降级到 read-only(最严档),绝不允许拿到 skip-permissions。
#[test]
fn invalid_levels_never_resolve_to_skip_permissions() {
    // 0 与 4..=255 全部非法(文档口径:1-3)。
    let invalid: Vec<u8> = std::iter::once(0u8).chain(4u8..=255).collect();

    for raw in invalid {
        let level = PermissionLevel::from_u8(raw);
        assert_ne!(
            level,
            PermissionLevel::SkipPermissions,
            "permission_level={raw} must not fail open into skip-permissions"
        );
        assert_eq!(
            level,
            PermissionLevel::ReadOnly,
            "permission_level={raw} must conservatively downgrade to read-only"
        );
        // 严格解析口对同一输入必须报错。
        assert!(
            PermissionLevel::try_from_u8(raw).is_err(),
            "try_from_u8({raw}) must reject out-of-range tier"
        );
    }
}

/// 合法档位(1..=3)映射稳定。
#[test]
fn valid_levels_keep_their_meaning() {
    assert_eq!(PermissionLevel::from_u8(1), PermissionLevel::ReadOnly);
    assert_eq!(PermissionLevel::from_u8(2), PermissionLevel::WorkspaceWrite);
    assert_eq!(PermissionLevel::from_u8(3), PermissionLevel::SkipPermissions);
    for raw in 1..=3u8 {
        assert_eq!(PermissionLevel::try_from_u8(raw).unwrap().to_u8(), raw);
    }
}

/// 旧 L1–L4 数值经 from_legacy_u8 迁移(配置 load 的迁移底座)。
#[test]
fn legacy_levels_migrate_monotonically() {
    let cases = [
        (1u8, PermissionLevel::ReadOnly),
        (2, PermissionLevel::ReadOnly),
        (3, PermissionLevel::WorkspaceWrite),
        (4, PermissionLevel::SkipPermissions),
    ];
    for (legacy, tier) in cases {
        assert_eq!(PermissionLevel::from_legacy_u8(legacy), Some(tier));
        // 迁移方向必须单调:旧档越宽,新档不窄。
        if legacy > 1 {
            let prev = PermissionLevel::from_legacy_u8(legacy - 1).unwrap();
            assert!(tier >= prev, "migration must not narrow at legacy={legacy}");
        }
    }
    assert_eq!(PermissionLevel::from_legacy_u8(0), None);
    assert_eq!(PermissionLevel::from_legacy_u8(5), None);
}

/// 端到端:非法档位下的 workspace 内写操作必须要求审批(fail-open 修复前按免审放行)。
#[test]
fn invalid_level_requires_approval_for_workspace_write() {
    let workspace = std::env::temp_dir().join("qaqh-permission-fail-closed-ws");
    std::fs::create_dir_all(&workspace).expect("create temp workspace");
    let target = workspace.join("src").join("lib.rs");

    for raw in [0u8, 4, 5, 128, 255] {
        let level = PermissionLevel::from_u8(raw);
        let decision = needs_permission(
            level,
            "write",
            &serde_json::json!({ "path": target }),
            &workspace,
            &HashSet::new(),
            ToolCategory::Write,
        );
        assert!(
            matches!(decision, PermissionDecision::AskUser { .. }),
            "permission_level={raw} (resolved to {level:?}) must ask the user for a workspace write"
        );
    }
}

/// 端到端:非法档位经 `admit` 单一准入面后必须落到 ApprovalRequired,
/// 而不是免审的 Authorized(fail-open 的真实爆炸半径)。
#[test]
fn invalid_level_admits_write_as_approval_required_not_authorized() {
    let workspace = std::env::temp_dir().join("qaqh-permission-fail-closed-admit");
    std::fs::create_dir_all(&workspace).expect("create temp workspace");

    for raw in [0u8, 4, 5, 255] {
        let invocation = ToolInvocation {
            session_id: "fail-closed".into(),
            call_id: format!("call-{raw}"),
            tool_name: "write".into(),
            action: String::new(),
            args: serde_json::json!({ "path": workspace.join("inside.txt") }),
            category: ToolCategory::Write,
        };
        let admission = admit(invocation, raw, &workspace, &HashSet::new());
        assert!(
            matches!(admission, Admission::ApprovalRequired(_)),
            "permission_level={raw} must not auto-authorize a write"
        );
    }
}

/// 守卫:read-only 档(最严档)对变更类操作(write/exec/net)仍要求审批——
/// 「降级到 read-only」是真正的 fail-closed 方向;同时锁定三档制的默认放行面
/// (工作区内读自动)不回退成旧 L1 的全审批。
#[test]
fn read_only_tier_asks_for_mutations_but_autos_workspace_reads() {
    let workspace = Path::new(".");
    let mutation = needs_permission(
        PermissionLevel::ReadOnly,
        "write",
        &serde_json::json!({ "path": "src/lib.rs" }),
        workspace,
        &HashSet::new(),
        ToolCategory::Write,
    );
    assert!(
        matches!(mutation, PermissionDecision::AskUser { .. }),
        "read-only tier must ask for writes"
    );

    let read = needs_permission(
        PermissionLevel::ReadOnly,
        "read",
        &serde_json::json!({ "path": "src/lib.rs" }),
        workspace,
        &HashSet::new(),
        ToolCategory::Read,
    );
    assert!(
        matches!(read, PermissionDecision::AutoApprove),
        "read-only tier auto-approves workspace reads (default open face)"
    );
}
