//! Workspace directory resolution.
//!
//! Resolves `.qaqh/` — the project-local hidden directory for PLAN.md,
//! trash, tasks, and project-scoped memory. Falls back to a subdirectory
//! of `data_dir()` when no workspace is active.

use std::path::{Path, PathBuf};

/// Return the `.qaqh/` directory for the current workspace.
///
/// Priority:
/// 1. `{workspace}/.qaqh/` if workspace is set and not "."
/// 2. `{data_dir}/workspace/` as fallback (headless / no workspace mode)
///
/// The fallback is intentionally NOT `home_dir()/.qaqh/` to avoid
/// conflating workspace artifacts with global config/sessions data.
pub fn qaqh_dir() -> PathBuf {
    let ws = crate::current_workspace();
    if !ws.is_empty() && ws != "." {
        Path::new(&ws).join(".qaqh")
    } else {
        qaqh_types::platform::data_dir().join("workspace")
    }
}

/// Bind the global session identifier used by tools and code-delta tracking.
pub fn set_current_session(session_id: &str) {
    crate::set_current_session(session_id);
}

/// Update tool path resolution (and, when safe, the process cwd).
///
/// `set_workspace` always writes the data layer: actor thread-locals when an
/// actor context is installed (multi-session daemon), the process-global
/// otherwise. The physical `std::env::set_current_dir` is only performed when
/// NO actor context is present — in a multi-actor daemon the process cwd is
/// a single shared resource and calling it from concurrent actors would make
/// every session's fallback cwd silently drift to the latest actor's
/// workspace. Path resolution never depends on the process cwd while an
/// actor context is active, so skipping the cd is behavior-preserving there.
pub fn set_process_workspace(path: &str) {
    let in_actor_context = crate::is_actor_context();
    crate::set_workspace(path);
    if in_actor_context {
        return;
    }
    if let Err(error) = std::env::set_current_dir(path) {
        log::warn!("set_process_workspace: cannot cd to '{}': {error}", path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qaqh_dir_returns_workspace_subdir_when_set() {
        let _guard = crate::TEST_RUNTIME_SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        crate::set_workspace("/home/user/project");
        let dir = qaqh_dir();
        assert_eq!(dir, Path::new("/home/user/project/.qaqh"));
    }

    #[test]
    fn qaqh_dir_falls_back_when_empty() {
        let _guard = crate::TEST_RUNTIME_SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        crate::set_workspace("");
        let dir = qaqh_dir();
        let expected = qaqh_types::platform::data_dir().join("workspace");
        assert_eq!(dir, expected);
    }

    #[test]
    fn qaqh_dir_falls_back_when_dot() {
        let _guard = crate::TEST_RUNTIME_SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        crate::set_workspace(".");
        let dir = qaqh_dir();
        let expected = qaqh_types::platform::data_dir().join("workspace");
        assert_eq!(dir, expected);
    }

    /// Multi-actor safety (cwd drift fix): inside an actor context
    /// set_process_workspace must update ONLY the actor thread-local; the
    /// process cwd is a shared resource and concurrent actors must not
    /// overwrite each other's fallback. Outside an actor context (serve/CLI
    /// single-session processes) the physical cd is preserved.
    #[test]
    fn set_process_workspace_skips_cwd_inside_actor_context() {
        let _guard = crate::TEST_RUNTIME_SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let before = std::env::current_dir().unwrap();
        let ws = tempfile::tempdir().unwrap();

        // Without actor context: physical cd happens (serve/CLI semantics).
        set_process_workspace(&ws.path().to_string_lossy());
        assert_eq!(
            std::env::current_dir().unwrap().canonicalize().unwrap(),
            ws.path().canonicalize().unwrap(),
            "non-actor path must keep the physical cd"
        );

        // Inside actor context: data layer updates, process cwd untouched.
        crate::set_actor_context(&ws.path().to_string_lossy(), "actor-cwd-test-seed");
        let other = tempfile::tempdir().unwrap();
        set_process_workspace(&other.path().to_string_lossy());
        assert_eq!(
            std::env::current_dir().unwrap().canonicalize().unwrap(),
            ws.path().canonicalize().unwrap(),
            "actor path must NOT move the shared process cwd"
        );
        assert_eq!(
            crate::current_workspace(),
            other.path().to_string_lossy(),
            "actor data layer must still update"
        );
        crate::clear_actor_context();

        // Restore: non-actor cd back for other tests.
        set_process_workspace(&before.to_string_lossy());
    }
}
