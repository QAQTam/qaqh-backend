//! Workspace path normalization shared by SQLite registration and runtime checks.

use std::path::Path;

/// Canonicalize a workspace path while preserving native separators.
pub fn canonical_cwd(path: &Path) -> String {
    #[cfg(windows)]
    {
        match std::fs::canonicalize(path) {
            Ok(path) => {
                let mut value = path.to_string_lossy().replace('/', "\\");
                if let Some(stripped) = value.strip_prefix("\\\\?\\") {
                    value = stripped.to_string();
                }
                value
            }
            Err(_) => path.to_string_lossy().replace('/', "\\"),
        }
    }
    #[cfg(not(windows))]
    {
        std::fs::canonicalize(path)
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|_| path.to_string_lossy().into_owned())
    }
}

/// Normalize cwd values written by older non-Windows versions.
pub fn repair_legacy_backslash_cwd(cwd: &str) -> String {
    #[cfg(windows)]
    {
        cwd.to_string()
    }
    #[cfg(not(windows))]
    {
        cwd.replace('\\', "/")
    }
}

pub(crate) fn normalize_path(path: &str) -> String {
    let mut normalized = path.replace('/', "\\");
    while normalized.ends_with('\\') {
        normalized.pop();
    }
    if cfg!(windows) {
        normalized.to_lowercase()
    } else {
        normalized
    }
}

pub(crate) fn cwd_belongs(cwd: &str, workspace_path: &str) -> bool {
    let cwd = normalize_path(cwd);
    let workspace_path = normalize_path(workspace_path);
    !cwd.is_empty()
        && !workspace_path.is_empty()
        && (cwd == workspace_path || cwd.starts_with(&format!("{workspace_path}\\")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(windows)]
    fn cwd_belongs_matches_self_and_children() {
        assert!(cwd_belongs(r"C:\proj\a", r"C:\proj\a"));
        assert!(cwd_belongs(r"C:\proj\a\src", r"C:\proj\a"));
        assert!(cwd_belongs(r"c:\PROJ\a\src", r"C:\proj\a"));
        assert!(!cwd_belongs(r"C:\proj\ab", r"C:\proj\a"));
    }

    #[test]
    #[cfg(not(windows))]
    fn repair_legacy_cwd_preserves_native_paths_and_repairs_backslashes() {
        assert_eq!(
            repair_legacy_backslash_cwd("/already/fine"),
            "/already/fine"
        );
        assert_eq!(
            repair_legacy_backslash_cwd(r"\home\user\project"),
            "/home/user/project"
        );
    }
}
