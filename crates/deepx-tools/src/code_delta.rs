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

    if let Some(path) = file_path {
        if let Some(delta) = git_code_delta(now, path, action) {
            return Some(delta);
        }
    }

    match (tool_name, action) {
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
    }
}

fn git_code_delta(now: u64, file_path: &str, action: &str) -> Option<deepx_proto::CodeDeltaRecord> {
    let seed = crate::CURRENT_SESSION.lock().ok()?.clone()?;
    let directory = deepx_types::platform::sessions_dir().join(seed);
    let workspace = std::fs::read_to_string(directory.join("workspace.txt")).ok()?;
    let workspace = workspace.trim();
    if workspace.is_empty() {
        return None;
    }

    let repository = git2::Repository::open(workspace).ok()?;
    match action {
        "write" | "edit" | "edit_diff" => {
            let head_tree = repository.head().ok()?.peel_to_tree().ok()?;
            let mut options = git2::DiffOptions::new();
            options.pathspec(file_path);
            let diff = repository
                .diff_tree_to_workdir(Some(&head_tree), Some(&mut options))
                .ok()?;
            let stats = diff.stats().ok()?;
            let is_new = head_tree.get_path(std::path::Path::new(file_path)).is_err();
            Some(deepx_proto::CodeDeltaRecord {
                timestamp: now,
                lines_added: stats.insertions(),
                lines_removed: stats.deletions(),
                files_created: usize::from(is_new),
                files_deleted: 0,
                file: Some(file_path.to_string()),
            })
        }
        "delete" => {
            let head_tree = repository.head().ok()?.peel_to_tree().ok()?;
            let entry = head_tree.get_path(std::path::Path::new(file_path)).ok()?;
            let blob = repository.find_blob(entry.id()).ok()?;
            Some(deepx_proto::CodeDeltaRecord {
                timestamp: now,
                lines_added: 0,
                lines_removed: String::from_utf8_lossy(blob.content()).lines().count(),
                files_created: 0,
                files_deleted: 1,
                file: Some(file_path.to_string()),
            })
        }
        _ => None,
    }
}
