//! service::fs_git — 远端文件浏览 + workspace/git 自由函数。

use serde_json::{Value, json};

/// 平台中立的"绝对路径"判定。
///
/// Windows 的 `Path::is_absolute` 对 POSIX 风格的 `/etc/hostname` 返回 false，
/// 会导致同一请求在 Linux daemon 上走 FORBIDDEN、在 Windows daemon 上走
/// "非绝对路径" —— 行为分叉。daemon 是跨平台的，前导 `/` 一律按绝对路径
/// 处理，交由 allowlist 判生死（POSIX 风格路径在 Windows 的 allowed_roots
/// 里匹配不上，自然落到 FORBIDDEN）。
fn is_absolute_like(path: &str) -> bool {
    path.starts_with('/') || std::path::Path::new(path).is_absolute()
}

use std::io::Read;
use std::path::{Path, PathBuf};

use qaqh_workspace::permission::{
    is_sensitive_session_path, normalize_lexically, path_within_dir, resolve_target_path,
};

/// 远端文件接口的允许根（**上界**）：会话工作区根 + daemon 数据根。
///
/// `fs.list`/`fs.read` 的合法用途是浏览会话工作区（前端远端文件选择器）。
/// 数据根承载会话/配置目录，其下的敏感会话文件（`sessions/**`、`meta.json`、
/// `secrets.toml` …）仍由 [`is_sensitive_session_path`] 单独拦掉。
pub(crate) fn allowed_roots(sessions: &qaqh_session::SessionManager) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    // 数据根（会话/配置目录）。
    roots.push(qaqh_types::platform::data_dir());
    // UI 工作区注册表（组织语义，与运行环境 workspace 解耦）。注册表尚未
    // 装配时跳过，而不是 panic（远端浏览不应因初始化顺序失败）。
    if let Some(store) = qaqh_session::WorkspaceStore::try_global() {
        for ws in store.list() {
            if !ws.path.is_empty() {
                roots.push(PathBuf::from(ws.path));
            }
        }
    }
    // 会话工作区：meta.cwd（覆盖未注册为 UI workspace 的会话）。旧版在非
    // Windows 上写坏的 `\` 形态 cwd 读取时归一化，避免合法工作区被漏判。
    for meta in sessions.list() {
        if let Some(cwd) = meta.cwd.as_deref().filter(|c| !c.is_empty()) {
            roots.push(PathBuf::from(
                qaqh_session::grouping::repair_legacy_backslash_cwd(cwd),
            ));
        }
    }
    roots
}

/// 远端文件接口的路径白名单判定（T-2-1 / 安全审查 P0-1）。
///
/// - 先按 `is_sensitive_session_path` 拒掉会话私有数据（与 `needs_permission`
///   共用同一名单，单一事实源）；
/// - 再把路径与每个允许根做**组件级**比较（`path_within_dir`），软链接经
///   `resolve_target_path` 解析、`..` 逃逸由 `normalize_lexically` 吃掉；
/// - 任一根命中即放行；无一命中返回 `false`（fail-closed）。
fn path_allowed(
    sessions: &qaqh_session::SessionManager,
    path: &Path,
    scope_seed: Option<&str>,
) -> bool {
    if is_sensitive_session_path(path) {
        return false;
    }
    let normalized = normalize_lexically(&resolve_target_path(path.to_path_buf()));
    if let Some(seed) = scope_seed {
        let Some(cwd) = sessions.workspace_cwd(seed).filter(|cwd| !cwd.is_empty()) else {
            return false;
        };
        let scoped_root = normalize_lexically(&resolve_target_path(PathBuf::from(
            qaqh_session::grouping::repair_legacy_backslash_cwd(&cwd),
        )));
        if !path_within_dir(&normalized, &scoped_root) {
            return false;
        }
    }
    allowed_roots(sessions).iter().any(|root| {
        let root = normalize_lexically(&resolve_target_path(root.clone()));
        path_within_dir(&normalized, &root)
    })
}

/// 白名单拒绝的统一错误串：daemon 侧据 `FORBIDDEN` 前缀映射为 `forbidden`
/// 码，与 IO 失败（文件不存在/权限不足）区分开。
fn forbidden(kind: &str, path: &str) -> String {
    format!("FORBIDDEN: {kind} {path}: path is outside the allowed roots")
}

/// `fs.list`：目录条目（目录优先 + 名称排序），返回 daemon 侧绝对路径。
///
/// 路径必须落在 [`allowed_roots`]（会话工作区根 + 数据根）内。
pub(crate) fn list_remote_directory(
    sessions: &qaqh_session::SessionManager,
    path: &str,
    scope_seed: Option<&str>,
) -> Result<Value, String> {
    let dir = std::path::Path::new(path);
    if !is_absolute_like(path) {
        return Err("fs.list requires an absolute path".to_string());
    }
    if !path_allowed(sessions, dir, scope_seed) {
        return Err(forbidden("fs.list", path));
    }
    let mut entries: Vec<Value> = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|e| format!("fs.list {path}: {e}"))? {
        let entry = entry.map_err(|e| format!("fs.list {path}: {e}"))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let entry_path = entry.path();
        let meta = match entry.metadata() {
            Ok(meta) => meta,
            // 软链接/权限问题不阻塞整个目录，标记 unknown 继续。
            Err(_) => {
                entries.push(json!({
                    "name": name,
                    "path": entry_path.to_string_lossy(),
                    "is_dir": false,
                    "is_file": false,
                    "size": 0,
                    "modified_ms": null,
                }));
                continue;
            }
        };
        let modified_ms = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64);
        entries.push(json!({
            "name": name,
            "path": entry_path.to_string_lossy(),
            "is_dir": meta.is_dir(),
            "is_file": meta.is_file(),
            "size": meta.len(),
            "modified_ms": modified_ms,
        }));
    }
    entries.sort_by(|a, b| {
        let (ad, bd) = (
            a["is_dir"].as_bool().unwrap_or(false),
            b["is_dir"].as_bool().unwrap_or(false),
        );
        match bd.cmp(&ad) {
            std::cmp::Ordering::Equal => a["name"]
                .as_str()
                .unwrap_or_default()
                .to_ascii_lowercase()
                .cmp(&b["name"].as_str().unwrap_or_default().to_ascii_lowercase()),
            other => other,
        }
    });
    Ok(Value::Array(entries))
}

/// `fs.read`：文本预览。读满 `max_bytes + 1` 以判断截断；内容按 UTF-8
/// lossy 返回（临时版不处理二进制编码协商）。
///
/// 路径必须落在 [`allowed_roots`]（会话工作区根 + 数据根）内。
pub(crate) fn read_remote_file(
    sessions: &qaqh_session::SessionManager,
    path: &str,
    max_bytes: u64,
    scope_seed: Option<&str>,
) -> Result<Value, String> {
    let file_path = std::path::Path::new(path);
    if !is_absolute_like(path) {
        return Err("fs.read requires an absolute path".to_string());
    }
    if !path_allowed(sessions, file_path, scope_seed) {
        return Err(forbidden("fs.read", path));
    }
    let meta = std::fs::metadata(file_path).map_err(|e| format!("fs.read {path}: {e}"))?;
    if !meta.is_file() {
        return Err(format!("fs.read {path}: not a file"));
    }
    let cap = max_bytes.clamp(1, 8 * 1024 * 1024) as usize;
    let file = std::fs::File::open(file_path).map_err(|e| format!("fs.read {path}: {e}"))?;
    let mut data = Vec::new();
    std::io::Read::take(file, (cap + 1) as u64)
        .read_to_end(&mut data)
        .map_err(|e| format!("fs.read {path}: {e}"))?;
    let truncated = data.len() > cap;
    data.truncate(cap);
    Ok(json!({
        "path": path,
        "size": meta.len(),
        "truncated": truncated,
        "content": String::from_utf8_lossy(&data),
    }))
}

pub(crate) fn workspace(sessions: &qaqh_session::SessionManager, seed: &str) -> String {
    if seed.is_empty() {
        return String::new();
    }
    // 统一数据源：meta.cwd（workspace.txt 退役，读取侧惰性迁移）。
    sessions.workspace_cwd(seed).unwrap_or_default()
}

pub(crate) fn git<F>(
    sessions: &qaqh_session::SessionManager,
    seed: &str,
    operation: F,
    empty: Value,
) -> Result<Value, String>
where
    F: FnOnce(&str) -> Result<String, String>,
{
    let workspace = workspace(sessions, seed);
    if workspace.is_empty() {
        return Ok(empty);
    }
    let value = operation(&workspace)?;
    serde_json::from_str(&value).or_else(|_| Ok(json!(value)))
}

pub(crate) fn qaqh_dir(sessions: &qaqh_session::SessionManager, seed: &str) -> std::path::PathBuf {
    let workspace = workspace(sessions, seed);
    if workspace.is_empty() || workspace == "." {
        qaqh_types::platform::data_dir().join("workspace")
    } else {
        std::path::Path::new(&workspace).join(".qaqh")
    }
}
