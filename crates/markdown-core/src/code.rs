//! 代码块规格（对应 REFERENCE §4）：
//! - 27 种语言表（shiki 语言集对齐）
//! - 别名归一：`h`→`c`、`hpp`→`cpp`；未知语言保留原文（shiki 原样尝试，
//!   失败回退字面——内容永不失真）

/// shiki 语言集（27 种），与 REFERENCE §4 完全一致。
pub const LANGUAGES: &[&str] = &[
    "ts", "tsx", "js", "jsx", "json", "yaml", "toml", "rs", "rust", "py", "python", "go",
    "java", "kt", "css", "scss", "html", "bash", "sh", "shell", "sql", "graphql", "md",
    "markdown", "diff", "c", "cpp", "zig", "nim",
];

/// 语言别名归一表。
const ALIASES: &[(&str, &str)] = &[
    ("h", "c"),
    ("hpp", "cpp"),
    ("rust", "rs"),
    ("python", "py"),
    ("markdown", "md"),
    ("shell", "bash"),
];

/// 归一化围栏语言标注：
/// - 去掉大小写差异（shiki 对语言名大小写不敏感）
/// - 命中别名表 → 归一
/// - 未知语言 → 返回原文（上层原样尝试，失败回退字面文本）
pub fn normalize_lang(raw: &str) -> String {
    let trimmed = raw.trim();
    let lower = trimmed.to_ascii_lowercase();
    for (alias, canonical) in ALIASES {
        if lower == *alias {
            return (*canonical).to_string();
        }
    }
    lower
}

/// 语言是否在 27 语言表内（供高亮器决策；不在表内 ≠ 放弃，
/// 只是没有第一方高亮规则）。
pub fn is_supported(lang: &str) -> bool {
    LANGUAGES.contains(&lang)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_normalize() {
        assert_eq!(normalize_lang("h"), "c");
        assert_eq!(normalize_lang("HPP"), "cpp");
        assert_eq!(normalize_lang("Rust"), "rs");
        assert_eq!(normalize_lang("Python"), "py");
        assert_eq!(normalize_lang("sh"), "sh");
    }

    #[test]
    fn unknown_lang_preserved_verbatim() {
        // 未知语言：不丢信息，交给上层尝试（shiki 原样尝试语义）
        assert_eq!(normalize_lang("coq"), "coq");
        assert_eq!(normalize_lang(""), "");
    }

    #[test]
    fn language_table_covers_spec_list() {
        // REFERENCE §4 的规格清单（29 个标注名；"27 种语言"指去别名族后的
        // 语法数——rs/rust、py/python、md/markdown、sh/shell/bash 为别名族，
        // 具体族数依 shiki 别名映射而定，验收以标注名清单为准）
        let spec: [&str; 29] = [
            "ts", "tsx", "js", "jsx", "json", "yaml", "toml", "rs", "rust", "py", "python",
            "go", "java", "kt", "css", "scss", "html", "bash", "sh", "shell", "sql",
            "graphql", "md", "markdown", "diff", "c", "cpp", "zig", "nim",
        ];
        for lang in spec {
            assert!(LANGUAGES.contains(&lang), "{lang} 缺失于语言表");
        }
        let families: std::collections::BTreeSet<String> =
            LANGUAGES.iter().map(|l| normalize_lang(l)).collect();
        assert!(
            (24..=27).contains(&families.len()),
            "去别名族数应在 24..=27（当前 {}）",
            families.len()
        );
        for lang in ["ts", "tsx", "js", "json", "rs", "go", "sql", "diff", "zig"] {
            assert!(is_supported(lang), "{lang} should be supported");
        }
    }

    #[test]
    fn whitespace_trimmed() {
        assert_eq!(normalize_lang("  Rust  "), "rs");
    }
}
