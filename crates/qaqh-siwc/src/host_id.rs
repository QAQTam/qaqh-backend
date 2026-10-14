//! `ext_agent_host_id`：**一个运行实例一个标识**。
//!
//! 文档要点（`/siwc/token-sharing-open-source`）：
//! - host = 你的工具跑在哪台机器（本机安装 / 自托管 VM），**不是**用户、不是凭据；
//! - 必须在**首次登录之前**生成并持久化，跨重启与换账户复用；
//! - 一个 client（用户授权的注册）可跨多个 host；host id 只做归属区分；
//! - 形态：`urn:ietf:params:oauth:jwk-thumbprint:…`（推荐）/ `urn:uuid:…` / `did:key:<key>`。
//!   本 M0 用 `urn:uuid:`（host id 当前只是标识，OpenAI 不校验私钥持有）。
//!
//! 落盘：`<dir>/chatgpt-host.json`，与 ringing-devices.json 同范式（原子写、损坏即重建）。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{SiwcCode, SiwcError};
use crate::store::write_json_atomic;
use crate::util::{new_host_id, now_ms};

const FILE_NAME: &str = "chatgpt-host.json";

#[derive(Debug, Serialize, Deserialize)]
struct HostFile {
    version: u32,
    host_id: String,
    created_at_ms: u64,
}

/// host id 的持久化盒子。**只保存 id**，不含任何凭据。
#[derive(Debug, Clone)]
pub struct HostIdStore {
    path: PathBuf,
}

impl HostIdStore {
    pub fn new(dir: &Path) -> Self {
        Self {
            path: dir.join(FILE_NAME),
        }
    }

    /// 读取既有 host id；缺失/损坏/形态非法则生成新的并落盘。
    ///
    /// 形态非法的既有值**不沿用**：文档明确 host id 不透明且不可是邮箱/用户 ID，
    /// 沿用一份可疑值会把归属关系绑到别的东西上。
    pub fn load_or_create(&self) -> Result<String, SiwcError> {
        if let Some(existing) = self.load_existing()
            && validate_host_id(&existing).is_ok()
        {
            return Ok(existing);
        }
        let host_id = new_host_id();
        let file = HostFile {
            version: 1,
            host_id: host_id.clone(),
            created_at_ms: now_ms(),
        };
        let text = serde_json::to_string_pretty(&file).map_err(|e| {
            SiwcError::new(SiwcCode::StoreFailure, format!("host id 序列化失败: {e}"))
        })?;
        write_json_atomic(&self.path, &text)?;
        log::info!("[siwc] host id 已生成并持久化（{}）", self.path.display());
        Ok(host_id)
    }

    fn load_existing(&self) -> Option<String> {
        let text = std::fs::read_to_string(&self.path).ok()?;
        let file: HostFile = serde_json::from_str(&text).ok()?;
        Some(file.host_id)
    }
}

/// 校验 host id 形态（文档列出的三种之一）。
pub fn validate_host_id(raw: &str) -> Result<(), String> {
    const JWK_THUMBPRINT: &str = "urn:ietf:params:oauth:jwk-thumbprint:";
    if raw.starts_with(JWK_THUMBPRINT) && raw.len() > JWK_THUMBPRINT.len() {
        return Ok(());
    }
    if raw.starts_with("did:key:") && raw.len() > "did:key:".len() {
        return Ok(());
    }
    if let Some(uuid) = raw.strip_prefix("urn:uuid:")
        && uuid.len() == 36
        && uuid.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
    {
        return Ok(());
    }
    Err(format!("不支持的 ext_agent_host_id 形态: {raw}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_id_is_created_once_and_reused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = HostIdStore::new(dir.path());
        let first = store.load_or_create().expect("create");
        let second = store.load_or_create().expect("reuse");
        assert_eq!(first, second, "同一 host 必须复用同一 id");
        assert!(first.starts_with("urn:uuid:"));
    }

    #[test]
    fn corrupt_file_is_replaced_instead_of_reused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = HostIdStore::new(dir.path());
        std::fs::write(
            dir.path().join(FILE_NAME),
            "{\"version\":1,\"host_id\":\"user@example.com\"}",
        )
        .expect("seed corrupt");
        let host_id = store.load_or_create().expect("recover");
        assert!(host_id.starts_with("urn:uuid:"), "不得沿用非法形态: {host_id}");
        // 二次读取拿到的是修复后的值。
        assert_eq!(store.load_or_create().expect("stable"), host_id);
    }

    #[test]
    fn accepted_forms_match_documentation() {
        assert!(validate_host_id("urn:uuid:0191f0a0-1111-4222-8333-444455556666").is_ok());
        assert!(validate_host_id("urn:ietf:params:oauth:jwk-thumbprint:sha-256:abc").is_ok());
        assert!(validate_host_id("did:key:z6Mkexample").is_ok());
        assert!(validate_host_id("did:web:example.com").is_err(), "只允许 did:key");
        assert!(validate_host_id("0191f0a0-1111-4222-8333-444455556666").is_err());
        assert!(validate_host_id("urn:uuid:short").is_err());
        assert!(validate_host_id("").is_err());
    }
}
