//! worktree 目录名派生：branch → slug + 短 hash（ADR-0033 决策 2）。
//!
//! 目录名**不等于** branch 名：Windows 文件系统的非法字符、保留设备名、尾部
//! 点/空格在这里归一；uuid 前 4 hex 防碰撞（codex `paths.rs` 的 bucket 手法）。
//! 非 ASCII branch 名（如中文）保留字母数字语义，其余标点折叠为 `-`。

use std::path::PathBuf;

/// slug 最大长度（字符数，非字节——截断按 char 边界，绝不做字节切片）。
pub const SLUG_MAX_LEN: usize = 40;

/// Windows 保留设备名（大小写不敏感，含带扩展名形态）。
const WINDOWS_RESERVED: [&str; 22] = [
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// 把 branch 名规整为跨平台安全的目录 slug。
///
/// 规则：路径分隔符与空白 → `-`；保留字母（含非 ASCII）与数字、`-`、`_`、`.`；
/// 折叠连续 `-`；去两端 `-`/`.`/空格（Windows 尾部点空格非法）；命中保留设备名
/// 加 `wt-` 前缀；结果为空兜底 `worktree`；超长按 char 截断。
#[must_use]
pub fn slugify_branch(branch: &str) -> String {
    let mut slug = String::with_capacity(branch.len());
    let mut last_dash = false;
    for ch in branch.chars() {
        let mapped = if ch == '/' || ch == '\\' || ch.is_whitespace() {
            Some('-')
        } else if ch.is_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            Some(ch)
        } else {
            // 其余标点/符号（`~^:?*[]` 等 ref 非法或 Windows 非法字符）折叠成 `-`。
            Some('-')
        };
        let mapped = mapped.unwrap_or('-');
        if mapped == '-' && last_dash {
            continue;
        }
        last_dash = mapped == '-';
        slug.push(mapped);
    }
    let trimmed = slug.trim_matches(['-', '.', ' ']);
    let mut slug = trimmed.chars().take(SLUG_MAX_LEN).collect::<String>();
    // 截断可能把尾部停在 `-`/`.` 上（Windows 非法），再修一次边。
    let trimmed_again = slug.trim_matches(['-', '.', ' ']);
    slug = trimmed_again.to_string();
    // Windows 保留名作用于**扩展名前的 stem**（`aux.txt`、`con.log` 同样非法）。
    let stem = slug.split('.').next().unwrap_or(slug.as_str());
    if WINDOWS_RESERVED.contains(&stem.to_lowercase().as_str()) {
        slug = format!("wt-{slug}");
    }
    if slug.is_empty() {
        slug = "worktree".to_string();
    }
    slug
}

/// 4 位 hex 防碰撞后缀（uuid v7 simple 形式的前 4 个字符）。
#[must_use]
pub fn new_collision_suffix() -> String {
    uuid::Uuid::now_v7()
        .simple()
        .to_string()
        .chars()
        .take(4)
        .collect()
}

/// worktree 池基目录：`{source_root}/.qaqh/worktree`（ADR-0033 决策 2）。
#[must_use]
pub fn worktree_base_dir(source_root: &std::path::Path) -> PathBuf {
    source_root.join(".qaqh").join("worktree")
}

/// `.qaqh/.gitignore` 中应有的忽略行（嵌套 worktree 树必须对 git 全链路不可见）。
pub const WORKTREE_IGNORE_ENTRY: &str = "worktree/";

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn slug_folds_separators_and_whitespace() {
        assert_eq!(slugify_branch("feature/auth-v2"), "feature-auth-v2");
        assert_eq!(slugify_branch("feature/long name here"), "feature-long-name-here");
        assert_eq!(slugify_branch("back\\slash"), "back-slash");
    }

    #[test]
    fn slug_keeps_non_ascii_letters() {
        assert_eq!(slugify_branch("修复/登录"), "修复-登录");
    }

    #[test]
    fn slug_handles_windows_reserved_and_edges() {
        assert_eq!(slugify_branch("con"), "wt-con");
        assert_eq!(slugify_branch("aux.txt"), "wt-aux.txt");
        // 尾部点/空格在 Windows 非法，必须去掉。
        assert_eq!(slugify_branch("trailing."), "trailing");
        assert_eq!(slugify_branch("...---..."), "worktree");
        assert_eq!(slugify_branch(""), "worktree");
    }

    #[test]
    fn slug_truncates_on_char_boundary() {
        let long = "长".repeat(SLUG_MAX_LEN + 10);
        let slug = slugify_branch(&long);
        assert_eq!(slug.chars().count(), SLUG_MAX_LEN);
    }

    #[test]
    fn collision_suffix_is_four_hex_chars() {
        let suffix = new_collision_suffix();
        assert_eq!(suffix.chars().count(), 4);
        assert!(suffix.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn base_dir_is_inside_qaqh() {
        let base = worktree_base_dir(std::path::Path::new("D:/proj"));
        assert!(base.ends_with(".qaqh/worktree") || base.ends_with(".qaqh\\worktree"));
    }
}
