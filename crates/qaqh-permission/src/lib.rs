//! Permission engine and execution-scoped workspace/session context.

#![cfg_attr(not(test), forbid(unsafe_code))]
pub mod command_class;
pub mod conflict;
pub mod permission;
pub mod workspace;

pub use command_class::{
    ExecCommandClass, ExecShellKind, classify_exec_args, classify_exec_command, default_shell_kind,
    exec_argument_tokens, exec_shell_kind,
};
pub use permission::{
    PermissionDecision, PermissionLevel, PermissionRisk, ToolCategory, TrustedFolderSet,
    classify_risk, extract_target_paths_in, is_self_administration_path, normalize_lexically,
    patch_target_paths, path_within_dir, resolve_target_path, resolve_target_path_in,
};

/// Resolve a path against an explicit workspace root.
/// If the path is already absolute, return it lexically normalized.
/// If the workspace is empty or ".", return the path as-is (OS cwd resolution).
/// Otherwise, join the workspace root with the relative path.
///
/// T-8-3②（安全审查 P1-3）：绝对路径统一做词法归一（去 `.` / `..` / 冗余分隔符），
/// 确保 read/write/edit 派生同形账本键，且不走 `canonicalize` 以免在 Windows
/// 混入 `\\?\` verbatim 前缀（BUG-2026-09-13-16）。
pub fn resolve_workspace_path(workspace: &str, path: &str) -> String {
    use std::path::Path;
    if path.is_empty() {
        return path.to_string();
    }
    let p = Path::new(path);
    if p.is_absolute() {
        return crate::permission::normalize_lexically(p)
            .to_string_lossy()
            .to_string();
    }
    if workspace.is_empty() || workspace == "." {
        return path.to_string();
    }
    let joined = Path::new(workspace).join(p);
    // Normalise: strip redundant `.`/`..` components via iterator
    // (e.g. D:\foo\./bar → D:\foo\bar).
    crate::permission::normalize_lexically(&joined)
        .to_string_lossy()
        .to_string()
}

/// Convert an absolute path into a display-friendly path relative to `workspace`.
///
/// Strips the workspace root prefix and uses `/` separators for
/// cross-platform consistency (like Git, VS Code, etc.).
pub fn display_path(abs_path: &str, workspace: &str) -> String {
    // Normalise input: strip redundant . components
    let normalized: std::path::PathBuf = std::path::Path::new(abs_path).components().collect();
    let norm_str = normalized.to_string_lossy();

    let ws = workspace.trim_end_matches(['/', '\\']);

    if !ws.is_empty() && ws != "." {
        // Case-insensitive prefix match on Windows。注意不能用 to_lowercase
        // 副本做 starts_with 再按原长切片：Unicode 小写映射可能改变字节长度，
        // 非 ASCII 路径会 panic。NTFS 大小写折叠仅限 ASCII，逐字节等长比较
        // 同时保证切点是 char boundary。
        if norm_str.len() >= ws.len()
            && norm_str.as_bytes()[..ws.len()].eq_ignore_ascii_case(ws.as_bytes())
        {
            let (_, tail) = norm_str.split_at(ws.len());
            let rel = tail.trim_start_matches(['/', '\\']);
            return rel.replace('\\', "/");
        }
    }
    // Not under workspace — return normalised path with forward slashes
    norm_str.replace('\\', "/")
}
