//! Workspace directory resolution.
//!
//! Resolves `.deepx/` — the project-local hidden directory for PLAN.md,
//! trash, tasks, and project-scoped memory. Falls back to a subdirectory
//! of `data_dir()` when no workspace is active.

use std::path::{Path, PathBuf};

use crate::CURRENT_WORKSPACE;

/// Return the `.deepx/` directory for the current workspace.
///
/// Priority:
/// 1. `{workspace}/.deepx/` if workspace is set and not "."
/// 2. `{data_dir}/workspace/` as fallback (headless / no workspace mode)
///
/// The fallback is intentionally NOT `home_dir()/.deepx/` to avoid
/// conflating workspace artifacts with global config/sessions data.
pub fn deepx_dir() -> PathBuf {
    let ws = CURRENT_WORKSPACE.read().unwrap_or_else(|e| e.into_inner());
    if !ws.is_empty() && *ws != "." {
        Path::new(&*ws).join(".deepx")
    } else {
        deepx_types::platform::data_dir().join("workspace")
    }
}

/// Like [`deepx_dir`] but ensures the directory (and its `trash/` subdir)
/// exist on disk. Returns the path on success.
pub fn ensure_deepx_dir() -> std::io::Result<PathBuf> {
    let dir = deepx_dir();
    std::fs::create_dir_all(dir.join("trash"))?;

    // Auto-add .deepx/ to .gitignore if workspace is a git repo
    auto_gitignore(&dir);

    Ok(dir)
}

/// Append `.deepx/` to workspace `.gitignore` if missing.
fn auto_gitignore(deepx_dir: &Path) {
    let ws = deepx_dir.parent().unwrap_or(Path::new("."));
    if !ws.join(".git").exists() {
        return;
    }

    let gitignore_path = ws.join(".gitignore");
    let current = std::fs::read_to_string(&gitignore_path).unwrap_or_default();

    if !current
        .lines()
        .any(|l| l.trim() == ".deepx/" || l.trim() == ".deepx")
    {
        let new_content = if current.is_empty() || current.ends_with('\n') {
            format!("{current}.deepx/\n")
        } else {
            format!("{current}\n.deepx/\n")
        };
        if std::fs::write(&gitignore_path, new_content).is_ok() {
            log::info!("workspace: added .deepx/ to {}", gitignore_path.display());
        }
    }
}

/// Bind the global session identifier used by tools and code-delta tracking.
pub fn set_current_session(seed: &str) {
    crate::set_current_session(seed);
}

/// Load and activate the workspace persisted for a session.
pub fn load_session_workspace(seed: &str) {
    let dir = deepx_types::platform::sessions_dir().join(seed);
    let workspace = std::fs::read_to_string(dir.join("workspace.txt")).unwrap_or_default();
    let workspace = workspace.trim();
    set_process_workspace(if workspace.is_empty() { "." } else { workspace });
}

/// Update tool path resolution and the process working directory together.
pub fn set_process_workspace(path: &str) {
    crate::set_workspace(path);
    if let Err(error) = std::env::set_current_dir(path) {
        log::warn!("set_process_workspace: cannot cd to '{}': {error}", path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deepx_dir_returns_workspace_subdir_when_set() {
        let _guard = crate::TEST_RUNTIME_SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        crate::set_workspace("/home/user/project");
        let dir = deepx_dir();
        assert_eq!(dir, Path::new("/home/user/project/.deepx"));
    }

    #[test]
    fn deepx_dir_falls_back_when_empty() {
        let _guard = crate::TEST_RUNTIME_SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        crate::set_workspace("");
        let dir = deepx_dir();
        let expected = deepx_types::platform::data_dir().join("workspace");
        assert_eq!(dir, expected);
    }

    #[test]
    fn deepx_dir_falls_back_when_dot() {
        let _guard = crate::TEST_RUNTIME_SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        crate::set_workspace(".");
        let dir = deepx_dir();
        let expected = deepx_types::platform::data_dir().join("workspace");
        assert_eq!(dir, expected);
    }
}
