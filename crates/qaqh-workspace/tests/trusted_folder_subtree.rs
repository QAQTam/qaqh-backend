//! BUG-2026-09-13-14：trust folder 精确匹配 → 信任目录子树仍弹审批。
//!
//! 缺陷：`needs_permission` 的跨 workspace 分支用
//! `resolve_target_path(trusted) == dir`（`dir = outside.parent()`）做**精确相等**
//! 比较。用户在 Level 3 下把 `D:\shared` 标记为「信任此文件夹」（one-time trust）
//! 后，写 `D:\shared\sub\new.rs`（`sub` 尚不存在）时 `parent()` = `D:\shared\sub`
//! ≠ 信任目录 → 每次都重新弹审批，与信任语义相反。
//!
//! 第二个缺陷：两侧路径比较未做大小写归一化。Windows 路径大小写不敏感，
//! `D:\Shared` 与 `d:\shared` 是同一目录，但字节比较判定不等 → 同一目录反复
//! 弹审批（信任记录与实际调用形态不一致时）。
//!
//! 修复预期：信任目录的**子树**（含自身）自动批准，比较按大小写不敏感归一做；
//! 前缀匹配必须是路径分量级，不能把 `D:\shared-other` 误判为 `D:\shared` 的子目录
//! （否则越权放行到信任目录之外）。

use qaqh_workspace::permission::{PermissionDecision, ToolCategory, needs_permission};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

fn temp_dir(tag: &str) -> PathBuf {
    let unique = format!(
        "qaqh-trust-subtree-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos()
    );
    let dir = std::env::temp_dir().join(unique);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn decision_for(path: &Path, trusted: &[PathBuf]) -> PermissionDecision {
    let json_path = path.to_str().unwrap_or_else(|| {
        panic!(
            "test target path must be valid UTF-8 (tool args arrive as JSON strings): {}",
            path.display()
        )
    });
    decision_for_json_path(json_path, trusted)
}

/// 调用侧路径以 JSON 字符串下发（恒为 UTF-8）；信任侧 `trusted` 是
/// `PathBuf`，可携带非 UTF-8 字节。非 UTF-8 用例需要二者分离构造。
fn decision_for_json_path(json_path: &str, trusted: &[PathBuf]) -> PermissionDecision {
    let workspace = std::env::temp_dir().join("qaqh-trust-subtree-workspace");
    needs_permission(
        qaqh_workspace::PermissionLevel::WorkspaceWrite,
        "write",
        &serde_json::json!({ "path": json_path }),
        &workspace,
        &trusted.iter().cloned().collect::<HashSet<_>>(),
        ToolCategory::Write,
    )
}

fn assert_auto_approved(decision: PermissionDecision, what: &str) {
    assert!(
        matches!(decision, PermissionDecision::AutoApprove),
        "{what} must be auto-approved under a trusted ancestor, got {decision:?}"
    );
}

fn assert_asks_user(decision: PermissionDecision, what: &str) {
    assert!(
        matches!(decision, PermissionDecision::AskUser { .. }),
        "{what} must still ask the user, got {decision:?}"
    );
}

/// 验收标准 1：信任 `D:\shared` 后写 `D:\shared\sub\new.rs`（sub 新建）不再弹审批。
#[test]
fn trusted_folder_covers_newly_created_subdirectory() {
    let root = temp_dir("new-subdir");
    let shared = root.join("shared");
    std::fs::create_dir_all(&shared).expect("create trusted folder");
    let target = shared.join("sub").join("new.rs");

    assert_auto_approved(
        decision_for(&target, std::slice::from_ref(&shared)),
        "write to a missing child directory of the trusted folder",
    );
}

/// 信任目录自身及其更深层子孙均自动批准。
#[test]
fn trusted_folder_covers_deep_descendants_and_itself() {
    let root = temp_dir("descendants");
    let shared = root.join("shared");
    std::fs::create_dir_all(shared.join("a").join("b")).expect("create nested tree");

    for target in [
        shared.clone(),
        shared.join("direct.txt"),
        shared.join("a").join("b").join("deep.txt"),
    ] {
        assert_auto_approved(
            decision_for(&target, std::slice::from_ref(&shared)),
            &format!("path under trusted folder: {}", target.display()),
        );
    }
}

/// 验收标准 2：大小写变体路径归一 —— **仅 Windows**。
///
/// Windows 文件系统（NTFS）大小写不敏感，`D:\Shared` 与 `d:\shared` 是同一目录。
/// Linux/macOS、WSL `/mnt/*` 上大小写**敏感**，二者是不同目录，靠本用例的
/// 折叠逻辑会越权放行；该平台的正/负语义由
/// [`case_sensitive_sibling_directory_is_not_trusted`] 覆盖。
#[cfg(windows)]
#[test]
fn trusted_folder_comparison_is_case_insensitive() {
    let root = temp_dir("case");
    let shared = root.join("shared");
    std::fs::create_dir_all(shared.join("sub")).expect("create nested tree");

    // 信任记录为小写形态，调用侧给出大小写变体（反之亦然）。
    let lowercase_trust = PathBuf::from(shared.to_string_lossy().to_lowercase());
    for target in [
        PathBuf::from(shared.to_string_lossy().to_uppercase())
            .join("sub")
            .join("new.rs"),
        shared.join("SUB").join("New.RS"),
    ] {
        assert_auto_approved(
            decision_for(&target, std::slice::from_ref(&lowercase_trust)),
            &format!("case variant under trusted folder: {}", target.display()),
        );
    }
}

/// 安全边界：份量级前缀匹配，`shared-other` 不是 `shared` 的子树。
#[test]
fn sibling_directory_with_same_prefix_is_not_trusted() {
    let root = temp_dir("sibling");
    let shared = root.join("shared");
    let sibling = root.join("shared-other");
    std::fs::create_dir_all(&shared).expect("create trusted folder");
    std::fs::create_dir_all(&sibling).expect("create sibling folder");

    assert_asks_user(
        decision_for(&sibling.join("new.rs"), &[shared]),
        "write into a sibling directory sharing a name prefix",
    );
}

/// 安全边界：向上逃逸到信任目录之外仍需审批。
#[test]
fn parent_traversal_out_of_trusted_folder_still_asks() {
    let root = temp_dir("traversal");
    let shared = root.join("shared");
    std::fs::create_dir_all(&shared).expect("create trusted folder");

    let escaped = shared.join("..").join("outside.rs");
    assert_asks_user(
        decision_for(&escaped, &[shared]),
        "write escaping the trusted folder via '..'",
    );
}

/// 未信任任何目录时行为不变（不得因前缀匹配引入意外放行）。
#[test]
fn untrusted_folder_still_asks() {
    let root = temp_dir("untrusted");
    let shared = root.join("shared");
    std::fs::create_dir_all(shared.join("sub")).expect("create nested tree");

    assert_asks_user(
        decision_for(&shared.join("sub").join("new.rs"), &[]),
        "write outside the workspace without any trust record",
    );
}

/// 【reviewer 阻断项 3】大小写**敏感**平台上的负向用例：同一父目录下的
/// `Shared/` 与 `shared/` 是两个**不同目录**（不同 inode），
/// 仅信任 `Shared` 时写 `shared/` 必须仍弹审批。
///
/// 旧实现无条件 `to_lowercase()` 折叠两侧路径，会把二者判为同一目录 →
/// `AutoApprove`（越权）。Windows 文件系统大小写不敏感，`Shared/` 与
/// `shared/` 本就是同一目录，故本用例仅在非 Windows 上断言。
#[cfg(not(windows))]
#[test]
fn case_sensitive_sibling_directory_is_not_trusted() {
    let root = temp_dir("case-sensitive-sibling");
    let upper = root.join("Shared");
    let lower = root.join("shared");
    std::fs::create_dir_all(&upper).expect("create Shared");
    std::fs::create_dir_all(&lower).expect("create shared");

    // 前置守卫：确认这两个目录在大小写敏感文件系统上确实是不同目录。
    // 若二者被文件系统判为同一目录（大小写不敏感挂载），本用例的前提不成立。
    let upper_canon = std::fs::canonicalize(&upper).expect("canonicalize Shared");
    let lower_canon = std::fs::canonicalize(&lower).expect("canonicalize shared");
    assert_ne!(
        upper_canon, lower_canon,
        "precondition: Shared/ and shared/ must be distinct directories on this filesystem"
    );

    // 正向：信任 Shared 后写 Shared/... 自动批准（子树语义仍生效）。
    assert_auto_approved(
        decision_for(&upper.join("new.rs"), std::slice::from_ref(&upper)),
        "write to the trusted folder itself",
    );

    // 负向：仅信任 Shared 时，写 shared/ 必须仍弹审批。
    assert_asks_user(
        decision_for(&lower.join("new.rs"), std::slice::from_ref(&upper)),
        "write into the case-variant sibling directory (different directory)",
    );
    assert_asks_user(
        decision_for(
            &lower.join("sub").join("new.rs"),
            std::slice::from_ref(&upper),
        ),
        "write into a newly created child of the case-variant sibling directory",
    );

    // 反向：仅信任 shared 时，写 Shared/ 也必须仍弹审批。
    assert_asks_user(
        decision_for(&upper.join("new.rs"), std::slice::from_ref(&lower)),
        "reverse direction: write into the case-variant sibling directory",
    );
}

/// 【reviewer 阻断项 2】非 UTF-8 目录名不得被有损折叠成同一路径。
///
/// `sh\xFFared` 与 `sh\u{FFFD}ared` 是两个不同目录，旧实现经
/// `to_string_lossy()` 把前者折叠为后者（非法字节 → `U+FFFD`）→ 误判相等 →
/// `AutoApprove`（越权）。修复后按 `OsStr` 原始字节比较，二者不相等。
///
/// 说明：工具入参是 JSON 字符串（恒为 UTF-8），因此**调用侧路径不可能携带
/// 非 UTF-8 字节**。本用例固定住两条语义：
/// 1. 与「被信任非 UTF-8 目录有损同名」的 UTF-8 目录 → 必须弹审批（核心回归）；
/// 2. 非 UTF-8 信任目录与任何 UTF-8 调用路径都不可能字节相等 → 恒弹审批，
///    即 fail-closed 而非 fail-open（宁多弹一次，不误放行）。
#[cfg(unix)]
#[test]
fn non_utf8_sibling_directory_is_not_trusted() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let root = temp_dir("non-utf8-sibling");
    let trusted = root.join(OsString::from_vec(b"sh\xFFared".to_vec()));
    std::fs::create_dir_all(&trusted).expect("create non-UTF-8 trusted folder");
    // UTF-8 合法，但字节序列与被信任目录不同，恰等于其「有损转换后」的形态。
    let collision = root.join("sh\u{FFFD}ared");
    std::fs::create_dir_all(&collision).expect("create lossy-collision folder");

    // 核心回归：有损同名的另一个目录 → 必须仍弹审批。
    assert_asks_user(
        decision_for_json_path(
            &collision.join("inner.rs").to_string_lossy(),
            std::slice::from_ref(&trusted),
        ),
        "a different directory that lossy-converts to the same string \
         must not be auto-approved",
    );

    // fail-closed：UTF-8 调用路径无法与非 UTF-8 信任目录字节相等 → 弹审批。
    assert_asks_user(
        decision_for_json_path(
            &collision.join("inner.rs").to_string_lossy(),
            std::slice::from_ref(&trusted),
        ),
        "a UTF-8 call path never byte-matches a non-UTF-8 trusted folder",
    );
}

/// 【reviewer 建议项 4】空/纯分隔符信任目录不得成为空前缀而 fail-open。
#[test]
fn empty_or_separator_only_trusted_dir_is_not_trusted() {
    let root = temp_dir("empty-trusted");
    let target = root.join("shared").join("new.rs");

    for bogus in [PathBuf::from(""), PathBuf::from("/"), PathBuf::from("\\")] {
        assert_asks_user(
            decision_for(&target, std::slice::from_ref(&bogus)),
            &format!(
                "write with a fail-open trust entry {:?} must still ask",
                bogus
            ),
        );
    }
}

/// 【reviewer 建议项 4】尾分隔符等价：信任 `/tmp/x/` 与 `/tmp/x` 应同效。
#[test]
fn trailing_separator_in_trusted_dir_is_equivalent() {
    let root = temp_dir("trailing-sep");
    let shared = root.join("shared");
    std::fs::create_dir_all(&shared).expect("create trusted folder");

    let with_sep = PathBuf::from(format!(
        "{}{}",
        shared.to_string_lossy(),
        std::path::MAIN_SEPARATOR
    ));
    assert_auto_approved(
        decision_for(&shared.join("new.rs"), &[with_sep]),
        "write under a trusted folder recorded with a trailing separator",
    );
}

/// 【reviewer 建议项 5】多路径调用不得只看首个在外路径。
///
/// `paths=[trusted/ok.rs, untrusted/evil.rs]` 时，未信任路径不得被同批次里
/// 已信任的那条连带放行（旧实现 `first_outside_workspace` 只取首个 → fail-open）。
#[test]
fn one_untrusted_path_in_batch_blocks_auto_approval() {
    let root = temp_dir("multi-path");
    let shared = root.join("shared");
    let other = root.join("other");
    std::fs::create_dir_all(&shared).expect("create trusted folder");
    std::fs::create_dir_all(&other).expect("create untrusted folder");

    let workspace = std::env::temp_dir().join("qaqh-trust-subtree-workspace");
    let trusted: HashSet<PathBuf> = [shared.clone()].into_iter().collect();
    let args = serde_json::json!({
        "paths": [
            shared.join("ok.rs").to_string_lossy(),
            other.join("evil.rs").to_string_lossy(),
        ]
    });

    let decision = needs_permission(
        qaqh_workspace::PermissionLevel::WorkspaceWrite,
        "write",
        &args,
        &workspace,
        &trusted,
        ToolCategory::Write,
    );
    assert_asks_user(
        decision,
        "a batch mixing a trusted path with an untrusted one",
    );
}
