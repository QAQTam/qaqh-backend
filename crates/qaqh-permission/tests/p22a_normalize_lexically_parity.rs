//! P2-2a parity：`path-clean` 换手写 `normalize_lexically` 的等价性。
//!
//! `normalize_lexically` 是审批/沙箱边界比较的路径键派生单源（全仓 28 处），
//! 本测试复刻退役前的手写 `components()+pop()` 循环，断言：
//! - 绝对路径语料（审批层实际输入形态）逐条输出与旧实现一致；
//! - 空输入契约（返回空 PathBuf，调用方 `is_empty()` 守卫）不变；
//! - 相对路径越界 `..` 的新语义（保留前导 `..`）被显式钉死——这是对旧
//!   实现"静默吞逃逸"的故意修正，若未来有人想改回去，此测试会拦住。

use std::path::{Component, Path, PathBuf};

use qaqh_permission::normalize_lexically;

/// 退役前 `permission::normalize_lexically` 逐字复刻。
fn old_normalize_lexically(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
        }
    }
    normalized
}

#[test]
fn absolute_paths_are_byte_identical_to_retired_impl() {
    let corpus = [
        "/",
        "/a",
        "/a/b/c",
        "/a/./b",
        "/a/./b/../c",
        "/a/b/../c",
        "/a/b/../../c",
        "/a/../../b", // 越深 ..：锚在文件系统根
        "/..",
        "/../..",
        "//a/b", // 双分隔符（RootDir 折叠）
        "/a/b/", // 尾部分隔符
        "/a//..//b/./",
        "/tmp/qaqh/../etc/passwd",
    ];
    for raw in corpus {
        let path = Path::new(raw);
        assert_eq!(
            normalize_lexically(path),
            old_normalize_lexically(path),
            "input: {raw}"
        );
        // 独立钉死绝对路径根锚点语义（不依赖复刻件）：
        let out = normalize_lexically(path);
        assert!(out.is_absolute(), "absolute input must stay absolute: {raw}");
        assert!(!out.to_string_lossy().contains("/./"), "no `.` residue: {raw}");
    }
}

#[test]
fn over_deep_dotdot_clamps_at_filesystem_root() {
    // 与 apply_patch_engine_scenarios 的语义备忘同锚点：clamp 锚是文件系统根。
    assert_eq!(normalize_lexically(Path::new("/a/../../b")), Path::new("/b"));
    assert_eq!(normalize_lexically(Path::new("/..")), Path::new("/"));
}

#[test]
fn empty_input_contract_is_preserved() {
    // `is_skill_instruction_path` 等调用方依赖空→空的守卫契约。
    assert!(normalize_lexically(Path::new("")).as_os_str().is_empty());
}

#[test]
fn relative_over_escape_keeps_leading_dotdot() {
    // 显式登记的行为修正：旧实现把 `a/../../x` 静默吞成 `x`（伪装成不逃逸），
    // path-clean 保留前导 `..`。生产链路先经 resolve_target_path 锚成绝对
    // 路径，此形态不进入审批比较；相对语境下保留 `..` 才诚实。
    assert_eq!(normalize_lexically(Path::new("a/../../x")), Path::new("../x"));
    assert_ne!(
        normalize_lexically(Path::new("a/../../x")),
        old_normalize_lexically(Path::new("a/../../x")),
    );
}

#[test]
fn ordinary_relative_paths_still_clean() {
    assert_eq!(normalize_lexically(Path::new("a/./b")), Path::new("a/b"));
    assert_eq!(normalize_lexically(Path::new("a/b/../c")), Path::new("a/c"));
}
