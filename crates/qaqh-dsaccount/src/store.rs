//! 账号凭据的本地存储（单账号）。
//!
//! 与 `qaqh-siwc::store` 同构：凭据是 **DPAPI/0600 密文**（复用
//! `qaqh_config::secrets::{protect, unprotect}`，全仓唯一实现），明文只在
//! [`StoredAccount::token`] 返回的那一刻出现在内存里。
//!
//! 三条纪律：
//!
//! 1. **解密失败不是「没登录」**：它是 [`DsCode::Storage`]。把解不开当成「未登录」
//!    会让 UI 显示一个干净的空态，而真相是本地凭据已损坏——前者用户会去点登录，
//!    后者用户会去查为什么（这正是我们要的）。
//! 2. **绑定 inference origin**：凭据与签发它的推理端点写在一起，读回时校验；
//!    换了端点的旧凭据一律不认。
//! 3. **投影里没有凭据**（连密文都没有）：[`StoredAccount::to_public_json`] 是服务面
//!    与 UI 唯一看得到的东西。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{DsCode, DsError};
use crate::util::now_ms;

/// 存储格式版本（未来的迁移入口）。
pub const STORE_VERSION: u32 = 1;

/// 已登录的账号（本机只保存最近一次成功登录的那一个）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoredAccount {
    /// 平台 user id（稳定标识）。
    pub user_id: String,
    /// 昵称（平台可能不给）。
    #[serde(default)]
    pub label: String,
    /// 登录方式（`WECHAT`…）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// 结算币种（UI 的金额单位）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
    /// 这条凭据**允许**使用的推理 origin（签发时写死）。
    pub inference_origin: String,
    /// 凭据密文（DPAPI/0600）。**唯一**存放 token 的地方。
    pub credential: String,
    /// 保存时刻（epoch ms）。
    pub saved_at_ms: u64,
}

impl StoredAccount {
    /// 新建一条记录（token 立即加密；构造失败即不落盘）。
    pub fn new(
        user_id: impl Into<String>,
        label: impl Into<String>,
        provider: Option<String>,
        currency: Option<String>,
        inference_origin: impl Into<String>,
        token: &str,
    ) -> Result<Self, DsError> {
        let mut account = Self {
            user_id: user_id.into(),
            label: label.into(),
            provider,
            currency,
            inference_origin: inference_origin.into(),
            credential: String::new(),
            saved_at_ms: 0,
        };
        account.set_token(token)?;
        Ok(account)
    }

    /// 解密凭据，并**校验 origin 绑定**。这是给调用方用的主入口。
    pub fn token_for(&self, inference_origin: &str) -> Result<String, DsError> {
        if self.inference_origin.trim_end_matches('/') != inference_origin.trim_end_matches('/') {
            return Err(DsError::new(
                DsCode::Protocol,
                format!(
                    "本机凭据是给 {} 签发的，不能用于 {inference_origin}（换端点不等于转发凭据）",
                    self.inference_origin
                ),
            ));
        }
        self.token()
    }

    /// 解密凭据。失败即 [`DsCode::Storage`]（见模块文档纪律 1）。
    pub fn token(&self) -> Result<String, DsError> {
        let plain = qaqh_config::secrets::unprotect(&self.credential).map_err(|error| {
            DsError::new(
                DsCode::Storage,
                format!("凭据解密失败（本地凭据已损坏，需要重新登录）: {error}"),
            )
        })?;
        String::from_utf8(plain)
            .map_err(|_| DsError::new(DsCode::Storage, "凭据解密后不是合法 UTF-8（视为损坏）"))
    }

    /// 更新凭据（重新登录取到新 token）。
    pub fn set_token(&mut self, token: &str) -> Result<(), DsError> {
        self.credential = qaqh_config::secrets::protect(token.as_bytes())
            .map_err(|error| DsError::new(DsCode::Storage, format!("凭据加密失败: {error}")))?;
        self.saved_at_ms = now_ms();
        Ok(())
    }

    /// 服务面/UI 可见的投影：**没有凭据，也没有密文**。
    pub fn to_public_json(&self) -> Value {
        serde_json::json!({
            "user_id": self.user_id,
            "label": self.label,
            "provider": self.provider,
            "currency": self.currency,
            "inference_origin": self.inference_origin,
            "saved_at_ms": self.saved_at_ms,
            "has_credential": !self.credential.is_empty(),
        })
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StoreFile {
    #[serde(default)]
    version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    account: Option<StoredAccount>,
}

/// 凭据文件（默认 `deepseek-account.json`，与 config 同目录）。
#[derive(Debug, Clone)]
pub struct AccountStore {
    path: PathBuf,
}

impl AccountStore {
    /// 默认位置：`<config 目录>/deepseek-account.json`。
    pub fn default_location() -> Self {
        Self::new(qaqh_types::platform::config_path().with_file_name("deepseek-account.json"))
    }

    /// 指定路径（测试与多环境用）。
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// 文件路径。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 读取。文件不存在 = `Ok(None)`（**没登录**）；文件损坏 = `Err`（**坏了**，两者不同）。
    pub fn load(&self) -> Result<Option<StoredAccount>, DsError> {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return Ok(None);
        };
        if text.trim().is_empty() {
            return Ok(None);
        }
        let file: StoreFile = serde_json::from_str(&text)
            .map_err(|error| DsError::new(DsCode::Storage, format!("凭据文件解析失败: {error}")))?;
        Ok(file.account)
    }

    /// 原子保存（临时文件 + rename 替换）。
    pub fn save(&self, account: &StoredAccount) -> Result<(), DsError> {
        self.write(&StoreFile {
            version: STORE_VERSION,
            account: Some(account.clone()),
        })
    }

    /// 清空（删文件；幂等）。**先清本地再通知远端**——见 `manager::sign_out`。
    pub fn clear(&self) -> Result<(), DsError> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(DsError::new(
                DsCode::Storage,
                format!("删除凭据文件失败: {error}"),
            )),
        }
    }

    fn write(&self, file: &StoreFile) -> Result<(), DsError> {
        let text = serde_json::to_string_pretty(file)
            .map_err(|error| DsError::new(DsCode::Storage, format!("凭据序列化失败: {error}")))?;
        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|error| {
                DsError::new(DsCode::Storage, format!("创建配置目录失败: {error}"))
            })?;
        }
        let temp = self.path.with_extension("json.tmp");
        std::fs::write(&temp, text.as_bytes())
            .map_err(|error| DsError::new(DsCode::Storage, format!("写入凭据失败: {error}")))?;
        restrict_permissions(&temp);
        // `fs::rename` 在 Windows 与 Unix 都是**替换式**的原子改名。
        std::fs::rename(&temp, &self.path).map_err(|error| {
            let _ = std::fs::remove_file(&temp);
            DsError::new(DsCode::Storage, format!("替换凭据文件失败: {error}"))
        })
    }
}

/// 尽力收紧权限（Unix 0600；Windows 上真正的保护是 DPAPI 密文本身）。
fn restrict_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "secret-account-token-value";

    fn store() -> (tempfile::TempDir, AccountStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = AccountStore::new(dir.path().join("deepseek-account.json"));
        (dir, store)
    }

    fn account(origin: &str) -> StoredAccount {
        StoredAccount::new(
            "user-1",
            "测试用户",
            Some("WECHAT".to_string()),
            Some("CNY".to_string()),
            origin,
            TOKEN,
        )
        .expect("构造")
    }

    #[test]
    fn a_missing_file_means_not_signed_in() {
        let (_dir, store) = store();
        assert!(store.load().expect("读").is_none());
    }

    #[test]
    fn round_trip_keeps_the_token_out_of_the_file() {
        let (_dir, store) = store();
        let record = account(crate::protocol::INFERENCE_ORIGIN);
        store.save(&record).expect("保存");

        let raw = std::fs::read_to_string(store.path()).expect("读文件");
        assert!(!raw.contains(TOKEN), "凭据**绝不能**以明文落盘: {raw}");

        let loaded = store.load().expect("读").expect("已登录");
        assert_eq!(loaded.user_id, "user-1");
        assert_eq!(loaded.label, "测试用户");
        assert_eq!(loaded.provider.as_deref(), Some("WECHAT"));
        assert_eq!(
            loaded.token().expect("解密"),
            TOKEN,
            "密文必须可逆（DPAPI/0600 同源）"
        );
        // 再存一次必须幂等（同名临时文件不会互相踩）。
        store.save(&loaded).expect("重存");
        assert_eq!(
            store
                .load()
                .expect("读")
                .expect("已登录")
                .token()
                .expect("解密"),
            TOKEN
        );
    }

    #[test]
    fn a_tampered_cipher_is_a_storage_failure_not_a_silent_signed_out() {
        let (_dir, store) = store();
        let mut record = account(crate::protocol::INFERENCE_ORIGIN);
        record.credential = "not-a-real-cipher".to_string();
        store.save(&record).expect("保存");

        let loaded = store.load().expect("读").expect("记录仍在");
        let error = loaded.token().expect_err("解不开必须报错");
        assert_eq!(error.code(), DsCode::Storage);
        assert!(
            !error.requires_reauth(),
            "本地损坏不该直接清凭据（用户可能只是想看一眼为什么）"
        );
    }

    #[test]
    fn a_credential_cannot_be_used_against_another_origin() {
        let record = account(crate::protocol::INFERENCE_ORIGIN);
        assert_eq!(
            record
                .token_for(crate::protocol::INFERENCE_ORIGIN)
                .expect("同 origin"),
            TOKEN
        );
        // 尾斜杠差异不算换端点。
        assert!(record.token_for("https://api.deepseek.com/").is_ok());

        for other in [
            "https://evil.example",
            "http://api.deepseek.com",
            "https://api.deepseek.com/v1",
        ] {
            let error = record.token_for(other).expect_err("换端点必须拒绝");
            assert_eq!(error.code(), DsCode::Protocol, "{other}");
            assert!(!error.requires_reauth(), "{other} 不该被当成凭据失效");
            assert!(!error.message().contains(TOKEN), "错误文案里不得出现凭据");
        }
    }

    #[test]
    fn the_public_projection_carries_neither_the_token_nor_the_cipher() {
        let record = account(crate::protocol::INFERENCE_ORIGIN);
        let text = record.to_public_json().to_string();
        assert!(!text.contains(TOKEN));
        assert!(!text.contains(&record.credential), "密文同样不进投影");
        assert!(text.contains("has_credential"));
    }

    #[test]
    fn clear_removes_the_file_and_is_idempotent() {
        let (_dir, store) = store();
        store
            .save(&account(crate::protocol::INFERENCE_ORIGIN))
            .expect("保存");
        store.clear().expect("清空");
        assert!(store.load().expect("读").is_none());
        store.clear().expect("再清一次也必须成功");
        // 空文件同样表达「没登录」，不该报错。
        std::fs::write(store.path(), "   ").expect("写空");
        assert!(store.load().expect("读空").is_none());
    }

    #[test]
    fn a_corrupt_store_file_is_reported_as_damage() {
        let (_dir, store) = store();
        std::fs::write(store.path(), "{ this is not json").expect("写坏");
        let error = store.load().expect_err("坏文件必须报错");
        assert_eq!(error.code(), DsCode::Storage);
    }
}
