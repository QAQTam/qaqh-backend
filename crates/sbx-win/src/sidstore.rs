//! capability SID 按 workspace 持久化(spec cross-review §3.2 设计点)。
//!
//! 键 = 规范化工作区路径的 hex(避免引入哈希依赖);存储 =
//! `%LOCALAPPDATA%\win-sbx\sids\<key>.json`。复用同一 SID → 已有的 ACE
//! 持续有效(has_ace 幂等跳过),不会在用户文件上堆积垃圾 ACE;
//! 不同工作区不同 SID → 跨工作区横向隔离。

use std::io;
use std::path::{Path, PathBuf};

pub fn data_root() -> PathBuf {
    std::env::var("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir())
        .join("win-sbx")
}

pub fn workspace_key(root: &Path) -> String {
    let norm = sbx_nt::normalize(&root.display().to_string())
        .unwrap_or_else(|_| root.display().to_string().to_lowercase());
    norm.as_bytes().iter().map(|b| format!("{b:02X}")).collect()
}

pub fn ledger_path(root: &Path) -> PathBuf {
    data_root()
        .join("ledgers")
        .join(format!("{}.jsonl", workspace_key(root)))
}

pub fn journal_path(root: &Path) -> PathBuf {
    data_root()
        .join("journals")
        .join(format!("{}.jsonl", workspace_key(root)))
}

/// 读取(或创建)工作区绑定的 capability SID。返回 (sid, 是否新建)。
pub fn load_or_create_sid(root: &Path) -> io::Result<(Vec<u8>, bool)> {
    let (id, _) = load_or_create_identity(root, crate::policy::IsolationKind::Token)?;
    Ok((id.sid, id.created))
}

/// 工作区身份:受限令牌的 cap-SID 或 AppContainer 的 AC-SID(带容器名)。
pub struct Identity {
    pub sid: Vec<u8>,
    pub sid_text: String,
    /// appcontainer 后端才有的容器名(profile 复用键)
    pub ac_name: Option<String>,
    pub created: bool,
}

/// 按 workspace + 隔离种类读取(或创建)持久身份。
/// 隔离种类切换时重绑:旧记录被覆盖,旧 ACE 由 ledger 逐行 sid 撤销。
pub fn load_or_create_identity(
    root: &Path,
    isolation: crate::policy::IsolationKind,
) -> io::Result<(Identity, bool)> {
    let file = data_root()
        .join("sids")
        .join(format!("{}.json", workspace_key(root)));
    let want_kind = match isolation {
        crate::policy::IsolationKind::Token => "token",
        crate::policy::IsolationKind::AppContainer => "appcontainer",
    };
    if let Ok(text) = std::fs::read_to_string(&file) {
        if let Some(v) = serde_json::from_str::<serde_json::Value>(&text).ok() {
            let sid_text = v.get("sid").and_then(|s| s.as_str()).map(String::from);
            let kind = v.get("isolation").and_then(|s| s.as_str()).unwrap_or("token");
            let ac_name = v.get("ac_name").and_then(|s| s.as_str()).map(String::from);
            if kind == want_kind && ac_name.is_some() == (isolation == crate::policy::IsolationKind::AppContainer) {
                if let Some(sid_text) = sid_text {
                    if let Ok(sid) = crate::sid::parse_sid(&sid_text) {
                        return Ok((
                            Identity {
                                sid,
                                sid_text,
                                ac_name,
                                created: false,
                            },
                            false,
                        ));
                    }
                }
            }
        }
    }
    let (sid, sid_text, ac_name) = match isolation {
        crate::policy::IsolationKind::Token => {
            let sid = crate::sid::capability_sid().map_err(io::Error::other)?;
            let text = crate::sid::sid_to_string(&sid);
            (sid, text, None)
        }
        crate::policy::IsolationKind::AppContainer => {
            let name = crate::appcontainer::container_name(&workspace_key(root));
            let profile = crate::appcontainer::ensure_profile(&name)
                .map_err(|e| io::Error::other(format!("appcontainer profile: {e}")))?;
            (profile.sid, profile.sid_text, Some(profile.name))
        }
    };
    let record = serde_json::json!({
        "sid": sid_text,
        "isolation": want_kind,
        "ac_name": ac_name,
        "workspace": root.display().to_string(),
        "created_ms": crate::events::now_millis(),
    });
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&file, record.to_string())?;
    Ok((
        Identity {
            sid,
            sid_text,
            ac_name,
            created: true,
        },
        true,
    ))
}

/// 只读读取工作区身份(不创建)。文件缺失/损坏/类型不符 → Ok(None)。
pub fn read_identity(root: &Path) -> io::Result<Option<Identity>> {
    let file = data_root()
        .join("sids")
        .join(format!("{}.json", workspace_key(root)));
    let Ok(text) = std::fs::read_to_string(&file) else {
        return Ok(None);
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Ok(None);
    };
    let Some(sid_text) = v.get("sid").and_then(|s| s.as_str()).map(String::from) else {
        return Ok(None);
    };
    let ac_name = v.get("ac_name").and_then(|s| s.as_str()).map(String::from);
    let Ok(sid) = crate::sid::parse_sid(&sid_text) else {
        return Ok(None);
    };
    Ok(Some(Identity {
        sid,
        sid_text,
        ac_name,
        created: false,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_is_stable_and_distinct() {
        let a = workspace_key(Path::new("E:\\Repo\\A"));
        let a2 = workspace_key(Path::new("E:\\repo\\a\\"));
        assert_eq!(a, a2, "大小写/尾分隔符不敏感");
        assert_ne!(a, workspace_key(Path::new("E:\\Repo\\B")));
    }

    #[test]
    fn sid_persist_roundtrip() {
        // SID 存储是持久设施:工作区路径必须唯一,否则会读到上一次运行的记录
        let ws = std::env::temp_dir().join(format!(
            "sbx-sidstore-probe-{}-{}",
            std::process::id(),
            crate::events::now_millis()
        ));
        std::fs::create_dir_all(&ws).unwrap();
        let (sid1, created1) = load_or_create_sid(&ws).unwrap();
        let (sid2, created2) = load_or_create_sid(&ws).unwrap();
        assert_eq!(sid1, sid2);
        assert!(created1 && !created2, "首次应创建,复用应命中存储");
        // 清理本用例在持久存储里的记录
        let _ = std::fs::remove_file(
            data_root()
                .join("sids")
                .join(format!("{}.json", workspace_key(&ws))),
        );
        let _ = std::fs::remove_dir_all(&ws);
    }
}
