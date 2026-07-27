//! Code-delta calculation for successful file mutations.

pub(crate) fn compute(
    tool_name: &str,
    args: &serde_json::Value,
) -> Option<deepx_proto::CodeDeltaRecord> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let action = args
        .get("action")
        .and_then(|value| value.as_str())
        .unwrap_or(tool_name);
    let file_path = args.get("path").and_then(|value| value.as_str());

    // Compute text-based line counts from args (cheap, no git2 pathspec bug).
    let mut delta = match (tool_name, action) {
        ("file", "write") => {
            let content = args
                .get("content")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            Some(deepx_proto::CodeDeltaRecord {
                timestamp: now,
                lines_added: content.lines().count(),
                lines_removed: 0,
                files_created: 1,
                files_deleted: 0,
                file: file_path.map(String::from),
            })
        }
        ("edit", _) => {
            let old = args
                .get("old_string")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            let new = args
                .get("new_string")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            Some(deepx_proto::CodeDeltaRecord {
                timestamp: now,
                lines_added: new.lines().count(),
                lines_removed: old.lines().count(),
                files_created: 0,
                files_deleted: 0,
                file: file_path.map(String::from),
            })
        }
        ("delete", _) => Some(deepx_proto::CodeDeltaRecord {
            timestamp: now,
            lines_added: 0,
            lines_removed: 0,
            files_created: 0,
            files_deleted: 1,
            file: file_path.map(String::from),
        }),
        ("edit_block", _) => {
            let old_count = args
                .get("old_lines")
                .and_then(|value| value.as_array())
                .map(Vec::len)
                .unwrap_or(0);
            let new_count = args
                .get("new_lines")
                .and_then(|value| value.as_array())
                .map(Vec::len)
                .unwrap_or(0);
            Some(deepx_proto::CodeDeltaRecord {
                timestamp: now,
                lines_added: new_count,
                lines_removed: old_count,
                files_created: 0,
                files_deleted: 0,
                file: file_path.map(String::from),
            })
        }
        _ => None,
    };

    // Override files_created / files_deleted from git when available
    // (git2::Repository::open is a cheap metadata op — no diff, no
    // pathspec bug since we only check HEAD tree existence).
    if let (Some(path), Some(d)) = (file_path, &mut delta) {
        if let Some(git) = git_file_meta(path) {
            d.files_created = git.files_created;
            d.files_deleted = git.files_deleted;
        }
    }

    delta
}

/// Lightweight git file metadata — only checks HEAD tree existence, no diff.
/// Avoids the git2 pathspec bug that inflated lines_added / lines_removed.
struct GitFileMeta {
    files_created: usize,
    files_deleted: usize,
}

fn git_file_meta(file_path: &str) -> Option<GitFileMeta> {
    let seed = crate::CURRENT_SESSION.lock().ok()?.clone()?;
    let directory = deepx_types::platform::sessions_dir().join(seed);
    let workspace = std::fs::read_to_string(directory.join("workspace.txt")).ok()?;
    let workspace = workspace.trim();
    if workspace.is_empty() {
        return None;
    }
    let repo = git2::Repository::open(workspace).ok()?;
    let head_tree = repo.head().ok()?.peel_to_tree().ok()?;
    let is_new = head_tree
        .get_path(std::path::Path::new(file_path))
        .is_err();
    Some(GitFileMeta {
        files_created: usize::from(is_new),
        files_deleted: 0,
    })
}
