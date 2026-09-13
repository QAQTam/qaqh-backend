//! BUG-2026-09-13-15：`PermissionLevel::from_u8` fail-open 回归测试。
//!
//! 缺陷：`from_u8` 的 `_ => Self::Unrestricted` 分支把**任何**非法档位
//! （0、5..=255）静默升级为 Level 4（免审批）。配置面 `permission_level`
//! 是裸 `u8` 且 config load 不校验范围，手写 config.toml 的一个笔误即可
//! 让全部工具调用绕过审批——fail-open 反向放大权限。
//!
//! 修复预期（fail-closed）：
//! 1. 非法标量解析保守降级 `MaxLockdown`（最严档），绝不落到 `Unrestricted`；
//! 2. 严格解析口 `try_from_u8` 显式拒绝非法值。
//!
//! 配置载荷/加载面（`permissionLevel` patch 与磁盘 config.toml 校验）的回归
//! 用例见 `qaqh-config` 侧（`config_patch_rejects_invalid_permission_level`）。

use qaqh_workspace::permission::{PermissionDecision, ToolCategory, needs_permission};
use qaqh_workspace::{Admission, PermissionLevel, ToolInvocation, admit};
use std::collections::HashSet;
use std::path::Path;

/// 非法档位必须保守降级到 MaxLockdown，绝不允许拿到 Unrestricted。
#[test]
fn invalid_levels_never_resolve_to_unrestricted() {
    // 0 与 5..=255 全部非法（文档口径：1-4）。
    let invalid: Vec<u8> = std::iter::once(0u8).chain(5u8..=255).collect();

    for raw in invalid {
        let level = PermissionLevel::from_u8(raw);
        assert_ne!(
            level,
            PermissionLevel::Unrestricted,
            "permission_level={raw} must not fail open into Unrestricted"
        );
        assert_eq!(
            level,
            PermissionLevel::MaxLockdown,
            "permission_level={raw} must conservatively downgrade to MaxLockdown"
        );
        // 严格解析口对同一输入必须报错。
        assert!(
            PermissionLevel::try_from_u8(raw).is_err(),
            "try_from_u8({raw}) must reject out-of-range level"
        );
    }
}

/// 合法档位（1..=4）语义不变——修复不得改变既有档位映射。
#[test]
fn valid_levels_keep_their_meaning() {
    assert_eq!(PermissionLevel::from_u8(1), PermissionLevel::MaxLockdown);
    assert_eq!(PermissionLevel::from_u8(2), PermissionLevel::ReadFree);
    assert_eq!(PermissionLevel::from_u8(3), PermissionLevel::WorkspaceFree);
    assert_eq!(PermissionLevel::from_u8(4), PermissionLevel::Unrestricted);
    for raw in 1..=4u8 {
        assert_eq!(PermissionLevel::try_from_u8(raw).unwrap().to_u8(), raw);
    }
}

/// 端到端：非法档位下的 workspace 内写操作必须要求审批（修复前按 Level 4 免审批）。
#[test]
fn invalid_level_requires_approval_for_workspace_write() {
    let workspace = std::env::temp_dir().join("qaqh-permission-fail-closed-ws");
    std::fs::create_dir_all(&workspace).expect("create temp workspace");
    let target = workspace.join("src").join("lib.rs");

    for raw in [0u8, 5, 128, 255] {
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

/// 端到端：非法档位经 `admit` 单一准入面后必须落到 ApprovalRequired，
/// 而不是 Level 4 的 Authorized（fail-open 的真实爆炸半径）。
#[test]
fn invalid_level_admits_write_as_approval_required_not_authorized() {
    let workspace = std::env::temp_dir().join("qaqh-permission-fail-closed-admit");
    std::fs::create_dir_all(&workspace).expect("create temp workspace");

    for raw in [0u8, 5, 255] {
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

/// 守卫：修复后的 `needs_permission` 在 MaxLockdown 下对 `read` 也要求审批，
/// 反证「降级到 MaxLockdown」是真正的 fail-closed 方向。
#[test]
fn max_lockdown_asks_even_for_reads_proving_fail_closed_direction() {
    let decision = needs_permission(
        PermissionLevel::MaxLockdown,
        "read",
        &serde_json::json!({ "path": "src/lib.rs" }),
        Path::new("."),
        &HashSet::new(),
        ToolCategory::Read,
    );
    assert!(matches!(decision, PermissionDecision::AskUser { .. }));
}
