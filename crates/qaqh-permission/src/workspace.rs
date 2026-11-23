//! Workspace directory resolution.
//!
//! Resolves `.qaqh/` — the project-local hidden directory for PLAN.md,
//! trash, tasks, and project-scoped memory. Falls back to a subdirectory
//! of `data_dir()` when no workspace is active.

use std::path::{Path, PathBuf};

/// Return the `.qaqh/` directory for an explicit workspace.
///
/// Priority:
/// 1. `{workspace}/.qaqh/` if workspace is set and not "."
/// 2. `{data_dir}/workspace/` as fallback (headless / no workspace mode)
///
/// The fallback is intentionally NOT `home_dir()/.qaqh/` to avoid
/// conflating workspace artifacts with global config/sessions data.
pub fn qaqh_dir_in(workspace: &str) -> PathBuf {
    if !workspace.is_empty() && workspace != "." {
        Path::new(workspace).join(".qaqh")
    } else {
        qaqh_types::platform::data_dir().join("workspace")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qaqh_dir_returns_workspace_subdir_when_set() {
        let dir = qaqh_dir_in("/home/user/project");
        assert_eq!(dir, Path::new("/home/user/project/.qaqh"));
    }

    #[test]
    fn qaqh_dir_falls_back_when_empty() {
        let dir = qaqh_dir_in("");
        let expected = qaqh_types::platform::data_dir().join("workspace");
        assert_eq!(dir, expected);
    }

    #[test]
    fn qaqh_dir_falls_back_when_dot() {
        let dir = qaqh_dir_in(".");
        let expected = qaqh_types::platform::data_dir().join("workspace");
        assert_eq!(dir, expected);
    }
}
