//! 凭据与账户注册的持久化。
//!
//! 三条纪律（都来自文档，不是我们的偏好）：
//! 1. **token 三件套只以密文落盘**——access / refresh / id_token 走
//!    `qaqh-config::secrets` 的 DPAPI（Windows）/0600（其它平台）盒子，
//!    与 `secrets.toml` 同一实现。明文 token 不得出现在磁盘、日志、服务面响应里。
//! 2. **一次注册一条记录**：注册身份 = `sub` + 下发的 `client_id`，
//!    邮箱只是标签（同邮箱的两次注册必须互不妨碍）。
//! 3. **原子写 + 损坏即空启动**：`.json.tmp` + rename，与
//!    `ringing-devices.json`（`qaqh-runtime/src/ringing/device_registry.rs`）同范式。
//!
//! **单写者假设**：只有 daemon 持凭据（Tauri 宿主经服务面读状态，绝不另存一份）。
//! 进程内由 [`crate::manager::SiwcManager`] 的锁串行化。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{SiwcCode, SiwcError};
use crate::util::now_ms;

const FILE_NAME: &str = "chatgpt-auth.json";
const STORE_VERSION: u32 = 1;

/// 一个登录 profile 的连接状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
#[serde(rename_all = "snake_case")]
pub enum ProfileStatus {
    /// 有可用凭据（注意 `sharing` 另判：可能「已登录但套餐未授权」）。
    Connected,
    /// 凭据已被判定不可用（`invalid_grant` 家族）：保留注册，等用户重登。
    ReauthRequired,
    /// 用户显式登出：凭据已清、注册保留。
    Disconnected,
}

/// token 三件套 + 到期信息。**只在进程内存里以明文存在**。
///
/// 三个字段都是 `Option` 是有意的：**「登录成功但未授权套餐用量」是一个真实状态**
/// （文档：保留登录，标记套餐用量不可用，并给出别的付款路径）。把它压成
/// 「必须有 access_token」会让这个状态无处安放，进而诱导实现去伪造一个空 token。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenSet {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<u64>,
    /// 文档字段：早于该时刻的刷新会被拒（配合 `refresh_not_ready`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub earliest_refresh_at_ms: Option<u64>,
}

impl TokenSet {
    pub fn expires_within(&self, now_ms: u64, window_ms: u64) -> bool {
        self.expires_at_ms
            .map(|at| at <= now_ms.saturating_add(window_ms))
            .unwrap_or(false)
    }
}

/// 一条账户注册（文档 §「Store credentials in a local file」的本地形态）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredProfile {
    pub id: String,
    pub label: String,
    pub status: ProfileStatus,
    /// OpenAI 在注册回调里**下发**的 client_id（形如 `oaiapp_…`）。
    /// **绝不等于** `dynamic_agent_client`。
    pub client_id: String,
    /// 生成本记录时的 host id（同 client 可跨 host，故记下）。
    pub host_id: String,
    pub subject: String,
    pub issuer: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// 实际授予的 scope 集合（来自 token 响应的 `scope`，不是我们请求的）。
    #[serde(default)]
    pub scopes: Vec<String>,
    pub saved_at_ms: u64,
    /// DPAPI/0600 密文形态的 [`TokenSet`]（JSON）。`None` = 未完成/已登出。
    ///
    /// `pub(crate)` 只为让同 crate 的 manager 能构造新记录（初值必为 `None`）；
    /// 写入一律经 [`StoredProfile::set_token_set`]，避免明文绕过加密路径。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) credentials: Option<String>,
    /// 刷新轮换的**落盘检查点**标记：新 token 已落盘、调用方尚未确认使用。
    /// 崩溃/取消后据此判定「有待确认的轮换」，绝不丢失唯一的后继 refresh token。
    #[serde(default, skip_serializing_if = "is_false")]
    pub pending_refresh: bool,
}

/// `skip_serializing_if` 需要 `&bool` 谓词（不是 `Option` 的那一套）。
fn is_false(value: &bool) -> bool {
    !*value
}

impl StoredProfile {
    /// 套餐用量是否可用：已连接 + 有凭据 + 授予了 `chatgpt.tokens.use.direct`。
    ///
    /// 这与「已登录」是两件事：只登录未授权套餐的 profile，`sharing == false`，
    /// UI 必须显示成「已登录·套餐未启用」而不是「已连接」。
    pub fn sharing(&self) -> bool {
        self.status == ProfileStatus::Connected
            && self.credentials.is_some()
            && self.has_sharing_scope()
    }

    pub fn has_sharing_scope(&self) -> bool {
        self.scopes
            .iter()
            .any(|s| s == crate::protocol::SHARING_SCOPE)
    }

    pub fn has_credentials(&self) -> bool {
        self.credentials.is_some()
    }

    /// 解密读回 token 三件套。
    pub fn token_set(&self) -> Result<Option<TokenSet>, SiwcError> {
        let Some(cipher) = &self.credentials else {
            return Ok(None);
        };
        let plain = qaqh_config::secrets::unprotect(cipher)
            .map_err(|e| SiwcError::new(SiwcCode::StoreFailure, format!("凭据解密失败: {e}")))?;
        let text = String::from_utf8(plain)
            .map_err(|_| SiwcError::new(SiwcCode::StoreFailure, "凭据明文非 UTF-8".to_string()))?;
        let set: TokenSet = serde_json::from_str(&text)
            .map_err(|e| SiwcError::new(SiwcCode::StoreFailure, format!("凭据形状非法: {e}")))?;
        Ok(Some(set))
    }

    /// 加密写回 token 三件套（刷新后**先落盘再使用**的检查点也走这里）。
    pub fn set_token_set(&mut self, set: &TokenSet) -> Result<(), SiwcError> {
        let text = serde_json::to_string(set)
            .map_err(|e| SiwcError::new(SiwcCode::StoreFailure, format!("凭据序列化失败: {e}")))?;
        self.credentials =
            Some(qaqh_config::secrets::protect(text.as_bytes()).map_err(|e| {
                SiwcError::new(SiwcCode::StoreFailure, format!("凭据加密失败: {e}"))
            })?);
        self.saved_at_ms = now_ms();
        Ok(())
    }

    /// 清掉凭据但**保留注册**（`client_id`/`sub`/`host_id`）：登出或需重登时用。
    /// 保留注册是为了下次登录复用 client_id，而不是重新注册一个新 app。
    pub fn clear_credentials(&mut self, status: ProfileStatus) {
        self.credentials = None;
        self.pending_refresh = false;
        self.scopes.clear();
        self.status = status;
        self.saved_at_ms = now_ms();
    }

    /// 服务面投影：**结构上不含 token 字段**。
    pub fn to_public_json(&self) -> serde_json::Value {
        let mut value = serde_json::json!({
            "id": self.id,
            "label": self.label,
            "status": self.status,
            "sharing": self.sharing(),
            "clientId": self.client_id,
            "scopes": self.scopes,
            "savedAtMs": self.saved_at_ms,
        });
        if let Some(map) = value.as_object_mut() {
            if let Some(email) = &self.email {
                map.insert("email".into(), serde_json::json!(email));
            }
            if let Some(name) = &self.name {
                map.insert("name".into(), serde_json::json!(name));
            }
        }
        value
    }
}

/// 已登记但尚未换码成功的注册（新注册回调下发 client_id 后立即落盘，
/// 这样「码已过期」的后续重试不会又注册一个新 app）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingRegistration {
    pub id: String,
    pub label: String,
    pub client_id: String,
    pub host_id: String,
    pub saved_at_ms: u64,
}

/// 磁盘形状。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreFile {
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_profile: Option<String>,
    #[serde(default)]
    pub profiles: Vec<StoredProfile>,
    #[serde(default)]
    pub pending_registrations: Vec<PendingRegistration>,
}

impl StoreFile {
    pub fn empty() -> Self {
        Self {
            version: STORE_VERSION,
            ..Self::default()
        }
    }

    pub fn active(&self) -> Option<&StoredProfile> {
        let id = self.active_profile.as_deref()?;
        self.profiles.iter().find(|p| p.id == id)
    }

    pub fn profile(&self, id: &str) -> Option<&StoredProfile> {
        self.profiles.iter().find(|p| p.id == id)
    }

    pub fn profile_mut(&mut self, id: &str) -> Option<&mut StoredProfile> {
        self.profiles.iter_mut().find(|p| p.id == id)
    }

    pub fn pending(&self, id: &str) -> Option<&PendingRegistration> {
        self.pending_registrations.iter().find(|p| p.id == id)
    }

    pub fn upsert(&mut self, profile: StoredProfile) {
        match self.profiles.iter_mut().find(|p| p.id == profile.id) {
            Some(slot) => *slot = profile,
            None => self.profiles.push(profile),
        }
    }

    /// 生成一个不与既有 label 冲突的展示名（用户可见，必须稳定且易分辨）。
    pub fn next_label(&self, requested: Option<&str>) -> String {
        let used: Vec<&str> = self
            .profiles
            .iter()
            .map(|p| p.label.as_str())
            .chain(self.pending_registrations.iter().map(|p| p.label.as_str()))
            .collect();
        if let Some(raw) = requested {
            let base = raw.trim();
            if !base.is_empty() && !used.contains(&base) {
                return base.to_string();
            }
            if !base.is_empty() {
                let mut n = 2;
                loop {
                    let candidate = format!("{base} ({n})");
                    if !used.contains(&candidate.as_str()) {
                        return candidate;
                    }
                    n += 1;
                }
            }
        }
        let mut n = 1;
        loop {
            let candidate = format!("ChatGPT 账户 {n}");
            if !used.contains(&candidate.as_str()) {
                return candidate;
            }
            n += 1;
        }
    }
}

/// 凭据文件句柄（读/写整文件；策略在 manager）。
#[derive(Debug, Clone)]
pub struct CredentialStore {
    path: PathBuf,
}

impl CredentialStore {
    pub fn new(dir: &Path) -> Self {
        Self {
            path: dir.join(FILE_NAME),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 读取；文件缺失或损坏即空启动（**不**因为解析失败就删文件：可能含可恢复凭据）。
    pub fn load(&self) -> StoreFile {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return StoreFile::empty();
        };
        match serde_json::from_str::<StoreFile>(&text) {
            Ok(mut file) => {
                file.version = STORE_VERSION;
                file
            }
            Err(e) => {
                log::warn!(
                    "[siwc] 凭据文件解析失败（按空启动，原文件保留）：{} {e}",
                    self.path.display()
                );
                StoreFile::empty()
            }
        }
    }

    pub fn save(&self, file: &StoreFile) -> Result<(), SiwcError> {
        let text = serde_json::to_string_pretty(file)
            .map_err(|e| SiwcError::new(SiwcCode::StoreFailure, format!("凭据序列化失败: {e}")))?;
        write_json_atomic(&self.path, &text)
    }
}

/// `.tmp` + rename 原子写（Windows 的 rename 不覆盖既有文件，故先删）。
pub(crate) fn write_json_atomic(path: &Path, text: &str) -> Result<(), SiwcError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            SiwcError::new(
                SiwcCode::StoreFailure,
                format!("创建目录失败 {}: {e}", parent.display()),
            )
        })?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).map_err(|e| {
        SiwcError::new(
            SiwcCode::StoreFailure,
            format!("写临时文件失败 {}: {e}", tmp.display()),
        )
    })?;
    restrict_permissions(&tmp);
    #[cfg(windows)]
    {
        let _ = std::fs::remove_file(path);
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        SiwcError::new(
            SiwcCode::StoreFailure,
            format!("落盘失败 {}: {e}", path.display()),
        )
    })
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(id: &str, label: &str) -> StoredProfile {
        StoredProfile {
            id: id.to_string(),
            label: label.to_string(),
            status: ProfileStatus::Connected,
            client_id: "oaiapp_test".into(),
            host_id: "urn:uuid:0191f0a0-1111-4222-8333-444455556666".into(),
            subject: "sub-1".into(),
            issuer: "https://auth.openai.com".into(),
            email: Some("user@example.com".into()),
            name: Some("User".into()),
            scopes: vec!["openid".into(), crate::protocol::SHARING_SCOPE.into()],
            saved_at_ms: 1,
            credentials: None,
            pending_refresh: false,
        }
    }

    fn token_set() -> TokenSet {
        TokenSet {
            access_token: Some("ACCESS-TOKEN-PLAINTEXT".into()),
            refresh_token: Some("REFRESH-TOKEN-PLAINTEXT".into()),
            id_token: Some("ID-TOKEN-PLAINTEXT".into()),
            expires_at_ms: Some(10_000),
            earliest_refresh_at_ms: None,
        }
    }

    #[test]
    fn tokens_are_never_plaintext_on_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = CredentialStore::new(dir.path());
        let mut file = StoreFile::empty();
        let mut entry = profile("p1", "ChatGPT 账户 1");
        entry.set_token_set(&token_set()).expect("protect");
        file.profiles.push(entry);
        file.active_profile = Some("p1".into());
        store.save(&file).expect("save");

        let raw = std::fs::read_to_string(store.path()).expect("read back");
        for banned in [
            "ACCESS-TOKEN-PLAINTEXT",
            "REFRESH-TOKEN-PLAINTEXT",
            "ID-TOKEN-PLAINTEXT",
        ] {
            assert!(!raw.contains(banned), "磁盘出现明文 {banned}: {raw}");
        }
        let reloaded = store.load();
        let set = reloaded
            .profile("p1")
            .expect("profile")
            .token_set()
            .expect("decrypt")
            .expect("some");
        assert_eq!(set, token_set());
    }

    #[test]
    fn sharing_requires_scope_credentials_and_connected_status() {
        let mut entry = profile("p1", "a");
        assert!(!entry.sharing(), "无凭据不算 sharing");
        entry.set_token_set(&token_set()).expect("protect");
        assert!(entry.sharing());

        entry.scopes = vec!["openid".into(), "email".into()];
        assert!(
            !entry.sharing(),
            "缺 chatgpt.tokens.use.direct 即未授权套餐"
        );

        entry.scopes.push(crate::protocol::SHARING_SCOPE.into());
        entry.status = ProfileStatus::ReauthRequired;
        assert!(!entry.sharing(), "需重登时不得声称可用");
    }

    #[test]
    fn identity_only_sign_in_is_representable() {
        // 文档路径：登录成功但没授权套餐用量 → 保留登录、标记不可用。
        let mut entry = profile("p1", "a");
        entry.scopes = vec!["openid".into(), "profile".into(), "email".into()];
        entry
            .set_token_set(&TokenSet {
                access_token: None,
                refresh_token: None,
                id_token: Some("id".into()),
                expires_at_ms: None,
                earliest_refresh_at_ms: None,
            })
            .expect("protect");
        assert!(entry.has_credentials());
        assert!(!entry.sharing());
        assert!(!entry.has_sharing_scope());
    }

    #[test]
    fn clear_credentials_keeps_registration() {
        let mut entry = profile("p1", "a");
        entry.set_token_set(&token_set()).expect("protect");
        entry.clear_credentials(ProfileStatus::ReauthRequired);
        assert!(!entry.has_credentials());
        assert!(entry.token_set().expect("read").is_none());
        assert_eq!(
            entry.client_id, "oaiapp_test",
            "注册必须保留以复用 client_id"
        );
        assert_eq!(entry.subject, "sub-1");
        assert!(!entry.sharing());
    }

    #[test]
    fn corrupt_file_starts_empty_without_deleting_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = CredentialStore::new(dir.path());
        std::fs::write(store.path(), "{not json").expect("seed");
        let loaded = store.load();
        assert!(loaded.profiles.is_empty());
        assert!(
            store.path().exists(),
            "损坏文件不得被删除（可能仍可人工恢复）"
        );
    }

    #[test]
    fn labels_are_unique_and_human_readable() {
        let mut file = StoreFile::empty();
        assert_eq!(file.next_label(None), "ChatGPT 账户 1");
        file.profiles.push(profile("p1", "ChatGPT 账户 1"));
        assert_eq!(file.next_label(None), "ChatGPT 账户 2");
        assert_eq!(file.next_label(Some("我的 Plus")), "我的 Plus");
        file.profiles.push(profile("p2", "我的 Plus"));
        assert_eq!(file.next_label(Some("我的 Plus")), "我的 Plus (2)");
        // 空白 label 等同「未指定」：沿用自动编号（当前占用的只有 1 号）。
        assert_eq!(file.next_label(Some("   ")), "ChatGPT 账户 2");
    }

    #[test]
    fn public_projection_has_no_token_fields() {
        let mut entry = profile("p1", "a");
        entry.set_token_set(&token_set()).expect("protect");
        let projected = entry.to_public_json().to_string();
        for banned in ["access_token", "refresh_token", "id_token", "credentials"] {
            assert!(
                !projected.contains(banned),
                "投影泄漏 {banned}: {projected}"
            );
        }
        assert!(projected.contains("\"sharing\":true"));
    }
}
