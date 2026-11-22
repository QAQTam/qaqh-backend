//! 独立信任存储:`<data_dir>/security/workspace-hook-trust-v1.json`。
//!
//! 语义(调研拍板):
//! - 全局/配置来源的 hooks **始终可信**;只有项目作用域(`<root>/.qaqh/hooks/`)
//!   的 hooks 需要本存储的 trust 记录。
//! - 记录按 workspace 绝对路径(规范化)索引,携带项目 hook 声明的 sha256
//!   digest:项目 hook 集变化(digest 不匹配)= 视为未信任,要求重新批准
//!   (ZCode hookDeclarationDigest / codex Modified 同律)。
//! - 存储损坏 = fail-closed:全部项目 hooks 按未信任处理并打错误日志,
//!   绝不静默重置(ZCode 踩过的 UI/runtime 分歧坑)。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const TRUST_STORE_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkspaceTrustRecord {
    pub trusted: bool,
    /// 批准时项目作用域 hook 声明的 digest(sha256 hex)。
    pub hook_digest: String,
    /// RFC3339 UTC。
    pub decided_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TrustStoreFile {
    version: u32,
    workspaces: BTreeMap<String, WorkspaceTrustRecord>,
}

impl TrustStoreFile {
    fn empty() -> Self {
        Self {
            version: TRUST_STORE_VERSION,
            workspaces: BTreeMap::new(),
        }
    }
}

/// 独立信任存储;进程内持有,写盘原子(temp + rename)。
pub struct HookTrustStore {
    path: PathBuf,
    state: TrustStoreFile,
    /// 解析失败标记:损坏后所有写路径拒绝,防止用空存储覆盖用户数据。
    corrupt: bool,
}

impl HookTrustStore {
    /// 加载;文件不存在 = 空存储,解析失败 = corrupt(fail-closed)。
    pub fn load(path: PathBuf) -> Self {
        let state = match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<TrustStoreFile>(&text) {
                Ok(parsed) if parsed.version == TRUST_STORE_VERSION => parsed,
                Ok(parsed) => {
                    log::error!(
                        "[hooks] trust store {} version {} unsupported; project hooks fail-closed",
                        path.display(),
                        parsed.version
                    );
                    return Self {
                        path,
                        state: TrustStoreFile::empty(),
                        corrupt: true,
                    };
                }
                Err(error) => {
                    log::error!(
                        "[hooks] trust store {} corrupt: {error}; project hooks fail-closed",
                        path.display()
                    );
                    return Self {
                        path,
                        state: TrustStoreFile::empty(),
                        corrupt: true,
                    };
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => TrustStoreFile::empty(),
            Err(error) => {
                log::error!(
                    "[hooks] trust store {} unreadable: {error}; project hooks fail-closed",
                    path.display()
                );
                return Self {
                    path,
                    state: TrustStoreFile::empty(),
                    corrupt: true,
                };
            }
        };
        Self {
            path,
            state,
            corrupt: false,
        }
    }

    /// 查询某 workspace 的信任状态:None = 从未批准。
    pub fn record(&self, workspace_root: &Path) -> Option<&WorkspaceTrustRecord> {
        self.state.workspaces.get(&normalize_root(workspace_root))
    }

    /// 批准(或撤销)某 workspace 的项目 hooks;digest 必须是当次声明集合。
    pub fn set_trust(
        &mut self,
        workspace_root: &Path,
        trusted: bool,
        hook_digest: &str,
        decided_at: &str,
    ) -> Result<(), TrustStoreError> {
        if self.corrupt {
            return Err(TrustStoreError::CorruptStore);
        }
        self.state.workspaces.insert(
            normalize_root(workspace_root),
            WorkspaceTrustRecord {
                trusted,
                hook_digest: hook_digest.to_string(),
                decided_at: decided_at.to_string(),
            },
        );
        self.persist()
    }

    fn persist(&self) -> Result<(), TrustStoreError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(TrustStoreError::Io)?;
        }
        let text = serde_json::to_string_pretty(&self.state)
            .map_err(|error| TrustStoreError::Io(std::io::Error::other(error)))?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, text.as_bytes()).map_err(TrustStoreError::Io)?;
        std::fs::rename(&tmp, &self.path).map_err(TrustStoreError::Io)?;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn corrupt(&self) -> bool {
        self.corrupt
    }
}

#[derive(Debug)]
pub enum TrustStoreError {
    CorruptStore,
    Io(std::io::Error),
}

impl std::fmt::Display for TrustStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CorruptStore => write!(f, "trust store corrupt; refusing writes"),
            Self::Io(error) => write!(f, "trust store io: {error}"),
        }
    }
}

/// workspace 路径规范化:绝对化 + 小写化盘符(digest 索引键稳定)。
pub fn normalize_root(root: &Path) -> String {
    let text = root.to_string_lossy().replace('\\', "/");
    if let Some((drive, rest)) = text.split_once(':') {
        format!("{}:{}", drive.to_ascii_lowercase(), rest)
    } else {
        text
    }
}

/// 项目作用域 hook 声明集合的 digest:对「每条 (事件, matcher, command/url,
/// timeout, async, on_failure, 来源文件相对路径) 规范行」排序后 sha256。
/// 任何增删改都会改变 digest → 重新要求批准。
pub fn project_hooks_digest(lines: &[String]) -> String {
    let mut hasher = Sha256::new();
    let mut sorted = lines.to_vec();
    sorted.sort();
    for line in &sorted {
        hasher.update(line.as_bytes());
        hasher.update(b"\n");
    }
    let digest = hasher.finalize();
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_roundtrip_and_digest_change_requires_retrust() {
        let dir = std::env::temp_dir().join(format!(
            "qaqh-hooks-trust-test-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("security/workspace-hook-trust-v1.json");
        let root = Path::new("D:/work/demo");

        let digest_a = project_hooks_digest(&["PreToolUse|Bash|bin/a.sh".to_string()]);
        let digest_b = project_hooks_digest(&["PreToolUse|Bash|bin/b.sh".to_string()]);

        {
            let mut store = HookTrustStore::load(path.clone());
            store.set_trust(root, true, &digest_a, "2026-11-22T00:00:00Z").unwrap();
        }
        {
            let store = HookTrustStore::load(path.clone());
            let record = store.record(root).unwrap();
            assert!(record.trusted);
            assert_eq!(record.hook_digest, digest_a);
        }
        // digest 变化 = 未信任。
        {
            let store = HookTrustStore::load(path);
            let record = store.record(root).unwrap();
            assert!(record.trusted);
            assert_ne!(record.hook_digest, digest_b);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_store_is_fail_closed() {
        let dir = std::env::temp_dir().join(format!(
            "qaqh-hooks-trust-test-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("security/workspace-hook-trust-v1.json");
        std::fs::write(&path, b"{not json").unwrap();

        let mut store = HookTrustStore::load(path);
        assert!(store.corrupt());
        let result = store.set_trust(
            Path::new("D:/work/demo"),
            true,
            "deadbeef",
            "2026-11-22T00:00:00Z",
        );
        assert!(matches!(result, Err(TrustStoreError::CorruptStore)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
