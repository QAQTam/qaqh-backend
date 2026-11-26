//! Secret store: API keys never touch `config.toml`.
//!
//! Audit P0-1: credentials previously lived as plaintext in `config.toml`
//! (including the `.toml.tmp` atomic-write residue). This module moves them
//! to a dedicated `secrets.toml` next to the config file:
//!
//! - **Windows**: each value is a DPAPI-encrypted blob (`dpapi:<base64>`),
//!   protected by the current user's DPAPI key (no new dependency — the
//!   `windows` crate is already in the workspace tree).
//! - **Other platforms**: plaintext with 0600 file permissions (TODO:
//!   keyring integration; at least separated from config and permission
//!   restricted).
//!
//! `config.toml` only ever stores the opaque marker `"set"` (or nothing) for
//! configured keys. Decryption failure never falls back to reading an old
//! plaintext value.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
/// Which credential slot a secret belongs to.
/// 历史的 `Subagent` 槽随模型配置统一退役（子代理凭据走 provider 槽）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretSlot {
    Main,
}

impl SecretSlot {
    fn key(self) -> &'static str {
        match self {
            SecretSlot::Main => "main",
        }
    }
}

/// MCP secret 名校验：非空、仅 `[a-z0-9_-]`、≤64（与 server 名同规）。
/// `${secret:name}` 占位符的解析也按同一字符集扫描（见 config.rs）。
fn validate_mcp_secret_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 64 {
        return Err(format!("[secrets.mcp] 名长度必须 1..=64（得到 {name:?}）"));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
    {
        return Err(format!(
            "[secrets.mcp] 名 {name:?} 含非法字符：仅允许小写字母、数字、'_'、'-'"
        ));
    }
    Ok(())
}

/// 槽位 id 校验（provider id 等命名槽共用）：非空、仅 `[a-z0-9_-]`、≤64。
fn validate_profile_secret_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 64 {
        return Err(format!("profile 名长度必须 1..=64（得到 {name:?}）"));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
    {
        return Err(format!(
            "profile 名 {name:?} 不能自带密钥：仅允许小写字母、数字、'_'、'-'"
        ));
    }
    Ok(())
}

/// Opaque marker stored in `config.toml` for a configured key.
pub const CONFIG_MARKER: &str = "set";

/// Per-slot secret store backed by `secrets.toml` next to the config file.
#[derive(Clone)]
pub struct SecretStore {
    path: PathBuf,
}

impl SecretStore {
    /// Store at `{config_dir}/secrets.toml` (same directory as config.toml).
    pub fn default_location() -> Self {
        Self::new(qaqh_types::platform::config_path().with_file_name("secrets.toml"))
    }

    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load and decrypt a slot. `None` when unset, unreadable, or when the
    /// DPAPI decryption fails (never falls back to legacy plaintext).
    pub fn load(&self, slot: SecretSlot) -> Option<String> {
        let data = std::fs::read_to_string(&self.path).ok()?;
        let doc: toml::Value = toml::from_str(&data).ok()?;
        let raw = doc
            .get(slot.key())
            .and_then(|s| s.get("api_key"))
            .and_then(|k| k.as_str())?;
        decrypt(raw).ok()
    }

    /// Whether the slot has a stored secret (does not decrypt).
    pub fn has(&self, slot: SecretSlot) -> bool {
        let Ok(data) = std::fs::read_to_string(&self.path) else {
            return false;
        };
        let Ok(doc) = toml::from_str::<toml::Value>(&data) else {
            return false;
        };
        doc.get(slot.key())
            .and_then(|s| s.get("api_key"))
            .and_then(|k| k.as_str())
            .is_some_and(|raw| !raw.is_empty())
    }

    /// Encrypt and store a plaintext value for a slot (idempotent).
    ///
    /// 读-改-写是一个**跨进程**事务：全程持 `secrets.toml.lock`（BUG-11），
    /// 否则 daemon 与 CLI 的并发 `read → mutate → write` 会互相覆盖丢密钥。
    pub fn set(&self, slot: SecretSlot, plaintext: &str) -> Result<(), String> {
        let encoded = encrypt(plaintext.as_bytes())?;
        self.transaction(|doc| {
            let mut table = match doc.get(slot.key()).cloned() {
                Some(toml::Value::Table(t)) => t,
                _ => toml::map::Map::new(),
            };
            table.insert("api_key".to_owned(), toml::Value::String(encoded));
            doc.insert(slot.key().to_owned(), toml::Value::Table(table));
            Ok(true)
        })
        .map(|_| ())
    }

    /// Remove a slot's secret.
    pub fn delete(&self, slot: SecretSlot) -> Result<(), String> {
        // 未配置时删除是幂等 no-op：`transaction` 的 `Ok(false)` 同时兜住
        // 并发写（BUG-11：另一进程刚在槽位写入密钥）。
        self.transaction(|doc| {
            if doc.remove(slot.key()).is_some() {
                return Ok(true);
            }
            Ok(false)
        })
        .map(|_| ())
    }

    // ── MCP 通用命名 secret（设计 §6/E-4：`[secrets.mcp]` map 段）──
    //
    // `config.toml` 的 `[mcp.servers.*]` env/headers 里用 `${secret:name}`
    // 占位符引用这里的名条目；本段沿用与槽位相同的加密/权限机制
    // （Windows DPAPI 加密，其余 0600），区别仅在“命名 map”而非固定槽。

    /// Load a named MCP secret (decrypts). `None` when unset/unreadable or
    /// when decryption fails (same no-fallback policy as slots).
    pub fn load_mcp(&self, name: &str) -> Option<String> {
        if validate_mcp_secret_name(name).is_err() {
            return None;
        }
        let data = std::fs::read_to_string(&self.path).ok()?;
        let doc: toml::Value = toml::from_str(&data).ok()?;
        let raw = doc
            .get("secrets")
            .and_then(|s| s.get("mcp"))
            .and_then(|m| m.get(name))
            .and_then(|v| v.as_str())?;
        decrypt(raw).ok()
    }

    /// Whether a named MCP secret is registered (does not decrypt) — the
    /// startup fail-fast check（未注册的 secret 名 → 启动时校验报错）。
    pub fn has_mcp(&self, name: &str) -> bool {
        if validate_mcp_secret_name(name).is_err() {
            return false;
        }
        let Ok(data) = std::fs::read_to_string(&self.path) else {
            return false;
        };
        let Ok(doc) = toml::from_str::<toml::Value>(&data) else {
            return false;
        };
        doc.get("secrets")
            .and_then(|s| s.get("mcp"))
            .and_then(|m| m.get(name))
            .and_then(|v| v.as_str())
            .is_some_and(|raw| !raw.is_empty())
    }

    /// All registered MCP secret names, sorted（fail-fast 报错提示用）。
    pub fn list_mcp(&self) -> Vec<String> {
        let Ok(data) = std::fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        let Ok(doc) = toml::from_str::<toml::Value>(&data) else {
            return Vec::new();
        };
        let mut names: Vec<String> = doc
            .get("secrets")
            .and_then(|s| s.get("mcp"))
            .and_then(|m| m.as_table())
            .map(|table| {
                table
                    .iter()
                    .filter(|(_, v)| v.as_str().is_some_and(|raw| !raw.is_empty()))
                    .map(|(k, _)| k.to_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    /// Encrypt and store a named MCP secret (idempotent).
    ///
    /// 与 [`Self::set`] 同为跨进程事务（BUG-11）：CLI 的 `mcp import --exec`
    /// 与 daemon 的 webUI 保存并发时，任何一方都不得丢掉对方的键。
    pub fn set_mcp(&self, name: &str, plaintext: &str) -> Result<(), String> {
        validate_mcp_secret_name(name)?;
        let encoded = encrypt(plaintext.as_bytes())?;
        self.transaction(|doc| {
            let mut secrets = match doc.get("secrets").cloned() {
                Some(toml::Value::Table(t)) => t,
                _ => toml::map::Map::new(),
            };
            let mut mcp = match secrets.get("mcp").cloned() {
                Some(toml::Value::Table(t)) => t,
                _ => toml::map::Map::new(),
            };
            mcp.insert(name.to_owned(), toml::Value::String(encoded));
            secrets.insert("mcp".to_owned(), toml::Value::Table(mcp));
            doc.insert("secrets".to_owned(), toml::Value::Table(secrets));
            Ok(true)
        })
        .map(|_| ())
    }

    /// Remove a named MCP secret.
    ///
    /// 删除同为跨进程事务；段不存在时直接返回 Ok（幂等，不产生空写）。
    pub fn delete_mcp(&self, name: &str) -> Result<(), String> {
        validate_mcp_secret_name(name)?;
        let first = self.transaction(|doc| {
            let Some(toml::Value::Table(mut secrets)) = doc.get("secrets").cloned() else {
                return Ok(false); // 段不存在 = 已删除
            };
            let Some(toml::Value::Table(mut mcp)) = secrets.get("mcp").cloned() else {
                return Ok(false);
            };
            mcp.remove(name);
            secrets.insert("mcp".to_owned(), toml::Value::Table(mcp));
            doc.insert("secrets".to_owned(), toml::Value::Table(secrets));
            Ok(true)
        })?;
        if first {
            return Ok(());
        }
        // 首次读到的文档里没有 `[secrets.mcp]`——但并发写者可能刚好在同一
        // 瞬间提交了 `[secrets.mcp]`；锁内复查一次，避免"已删除"假象下留下
        // 对方刚提交的键。稳态下（无并发）第二次事务同样命中 false 即返回。
        // 该复查不改变对外语义：原实现"段不存在直接 Ok"同样不保证与并发
        // 写入的 happens-after 顺序。
        self.transaction(|doc| {
            let Some(toml::Value::Table(mut secrets)) = doc.get("secrets").cloned() else {
                return Ok(false);
            };
            let Some(toml::Value::Table(mut mcp)) = secrets.get("mcp").cloned() else {
                return Ok(false);
            };
            if mcp.remove(name).is_some() {
                secrets.insert("mcp".to_owned(), toml::Value::Table(mcp));
                doc.insert("secrets".to_owned(), toml::Value::Table(secrets));
                return Ok(true);
            }
            Ok(false)
        })
        .map(|_| ())
    }

    // 新 BYOK 目录体系（ProfilesDocument）的凭据槽。明文只进本文件加密存储，
    // providers.toml 只落 "set" 标记；id 字符集约束与 profile 名一致。

    /// Load a provider's own key (decrypts). `None` = 未存储（继承 main）或
    /// id 不可表示。
    pub fn load_provider_key(&self, provider_id: &str) -> Option<String> {
        validate_profile_secret_name(provider_id).ok()?;
        self.load_named_slot("providers", provider_id)
    }

    /// True when the provider has its own stored key.
    pub fn has_provider_key(&self, provider_id: &str) -> bool {
        if validate_profile_secret_name(provider_id).is_err() {
            return false;
        }
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|data| toml::from_str::<toml::Value>(&data).ok())
            .and_then(|doc| {
                doc.get("secrets")?
                    .get("providers")?
                    .get(provider_id)?
                    .as_str()
                    .map(|raw| !raw.is_empty())
            })
            .unwrap_or(false)
    }

    /// Encrypt and store a provider's own key (idempotent).
    pub fn set_provider_key(&self, provider_id: &str, plaintext: &str) -> Result<(), String> {
        validate_profile_secret_name(provider_id)?;
        self.set_named_slot("providers", provider_id, plaintext)
    }

    /// Remove a provider's own key (idempotent).
    pub fn delete_provider_key(&self, provider_id: &str) -> Result<(), String> {
        validate_profile_secret_name(provider_id)?;
        self.delete_named_slot("providers", provider_id)
    }

    // ── 命名 map 段的通用实现（新调用方用；MCP 侧保留自己的实现不动）──

    fn load_named_slot(&self, section: &str, name: &str) -> Option<String> {
        let data = std::fs::read_to_string(&self.path).ok()?;
        let doc: toml::Value = toml::from_str(&data).ok()?;
        let raw = doc.get("secrets")?.get(section)?.get(name)?.as_str()?;
        decrypt(raw).ok()
    }

    fn set_named_slot(&self, section: &str, name: &str, plaintext: &str) -> Result<(), String> {
        let encoded = encrypt(plaintext.as_bytes())?;
        self.transaction(|doc| {
            let mut secrets = match doc.get("secrets").cloned() {
                Some(toml::Value::Table(t)) => t,
                _ => toml::map::Map::new(),
            };
            let mut map = match secrets.get(section).cloned() {
                Some(toml::Value::Table(t)) => t,
                _ => toml::map::Map::new(),
            };
            map.insert(name.to_owned(), toml::Value::String(encoded));
            secrets.insert(section.to_owned(), toml::Value::Table(map));
            doc.insert("secrets".to_owned(), toml::Value::Table(secrets));
            Ok(true)
        })
        .map(|_| ())
    }

    fn delete_named_slot(&self, section: &str, name: &str) -> Result<(), String> {
        let first = self.transaction(|doc| {
            let Some(toml::Value::Table(mut secrets)) = doc.get("secrets").cloned() else {
                return Ok(false); // 段不存在 = 已删除
            };
            let Some(toml::Value::Table(mut map)) = secrets.get(section).cloned() else {
                return Ok(false);
            };
            map.remove(name);
            secrets.insert(section.to_owned(), toml::Value::Table(map));
            doc.insert("secrets".to_owned(), toml::Value::Table(secrets));
            Ok(true)
        })?;
        if first {
            return Ok(());
        }
        // 同 [`Self::delete_mcp`]：并发写者可能刚提交了同一段，锁内复查一次，
        // 避免"已删除"假象下留下对方刚写入的键。
        self.transaction(|doc| {
            let Some(toml::Value::Table(mut secrets)) = doc.get("secrets").cloned() else {
                return Ok(false);
            };
            let Some(toml::Value::Table(mut map)) = secrets.get(section).cloned() else {
                return Ok(false);
            };
            if map.remove(name).is_some() {
                secrets.insert(section.to_owned(), toml::Value::Table(map));
                doc.insert("secrets".to_owned(), toml::Value::Table(secrets));
                return Ok(true);
            }
            Ok(false)
        })
        .map(|_| ())
    }

    fn read_doc(&self) -> toml::map::Map<String, toml::Value> {
        let Ok(data) = std::fs::read_to_string(&self.path) else {
            return toml::map::Map::new();
        };
        toml::from_str(&data).unwrap_or_default()
    }

    // ── 跨进程并发安全（BUG-2026-09-13-11）──
    //
    // 缺陷：中间文件名**固定**为 `secrets.toml.tmp`，且 read-modify-write
    // 的互斥只有进程内的 `config_io_lock`（config.rs 的 `Mutex`）——daemon
    // （webUI 保存）与 CLI（`qaqh-daemon mcp import --exec`）并发时：
    //
    //   A 写 tmp ──► B 用同名的自己那份文档覆盖 tmp ──► A rename 成功
    //   ──► B rename ENOENT，且落地文档只有 A 的键 → B 的密钥静默丢失。
    //
    // 修复（Codex `secrets/src/local.rs:295` 同款）：
    //   ① tmp 名带 pid + 单调 nonce，跨进程互不覆盖；
    //   ② 写完 `sync_all` 再 rename（与会话/工作区原子写一致）；
    //   ③ 整个 read-modify-write 持 `secrets.toml.lock` 跨进程 OS 文件锁
    //      （`std::fs::File::lock`：Windows `LockFileEx` / Unix `flock`）
    //      → 并发事务串行化，不再丢更新。

    /// 跨进程 read-modify-write 事务：持 `secrets.toml.lock` 独占锁，
    /// `mutate` 返回 `Ok(false)` 表示"无变更"（跳过写盘，保证幂等删除不产生
    /// 空写与多余的 rename）。
    fn transaction<F>(&self, mutate: F) -> Result<bool, String>
    where
        F: FnOnce(&mut toml::map::Map<String, toml::Value>) -> Result<bool, String>,
    {
        let _guard = self.lock_exclusive()?;
        let mut doc = self.read_doc();
        if !mutate(&mut doc)? {
            return Ok(false);
        }
        self.write_doc(&doc)?;
        Ok(true)
    }

    /// 取跨进程独占锁。锁文件独立于被替换的 `secrets.toml`（rename 会换 inode，
    /// 锁在目标文件上不可靠）；锁随 `_guard` 释放，进程退出（含崩溃）由内核
    /// 兜底释放，因此无需 pid 判活与 stale 锁接管。
    fn lock_exclusive(&self) -> Result<File, String> {
        let lock_path = self.lock_path();
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("secrets create_dir_all failed: {e}"))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|e| format!("secrets open lock {:?} failed: {e}", lock_path))?;
        // 锁文件无内容，0600（Unix）；Windows ACL 同步收紧。
        restrict_permissions(&lock_path);
        file.lock()
            .map_err(|e| format!("secrets lock {:?} failed: {e}", lock_path))?;
        Ok(file)
    }

    /// 锁文件路径：`secrets.toml` → `secrets.toml.lock`。
    fn lock_path(&self) -> PathBuf {
        sibling_path(&self.path, "secrets.toml", ".lock")
    }

    fn write_doc(&self, doc: &toml::map::Map<String, toml::Value>) -> Result<(), String> {
        let content = toml::to_string_pretty(doc)
            .map_err(|e| format!("secrets serialization failed: {e}"))?;
        let parent = self
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("secrets create_dir_all failed: {e}"))?;
        // BUG-2026-09-13-11：临时文件必须与目标同目录（rename 同卷才原子）且
        // 逐次唯一（跨进程不互覆）。自 v4.5 P2-2a 起由 `tempfile::NamedTempFile`
        // 承担——内部以 `O_CREAT|O_EXCL` + 随机后缀创建（Unix 直接 0600），
        // 失败时 Drop 自动清理，无需手写 nonce/计数器/remove。
        let mut temp = tempfile::NamedTempFile::new_in(parent)
            .map_err(|e| format!("secrets write failed: {e}"))?;
        let result = (|| -> std::io::Result<()> {
            restrict_permissions(temp.path());
            temp.write_all(content.as_bytes())?;
            temp.flush()?;
            // 落盘后再 persist：崩溃不会留下"重命名成功但内容未持久化"的空壳。
            temp.as_file().sync_all()?;
            Ok(())
        })();
        if let Err(e) = result {
            return Err(format!("secrets write failed: {e}"));
        }
        // `persist` 内部即原子替换（Unix rename；Windows 带
        // MOVEFILE_REPLACE_EXISTING）——原手写 `replace_file` 双分支随本行退役。
        temp.persist(self.path.clone())
            .map_err(|e| format!("secrets rename failed: {}", e.error))?;
        restrict_permissions(&self.path);
        Ok(())
    }
}

/// 由目标路径推同目录姊妹路径（`secrets.toml` → `secrets.toml{suffix}`）。
///
/// 注意：`PathBuf::with_extension` 在此不可用——它把 `secrets.toml` 的"扩展名"
/// （`toml`）**整段替换**，且无法表达多段后缀；这里显式拼到完整文件名之后，
/// 保证 `secrets.toml` 前缀恒在、且与目标同目录（rename 必须同卷）。
fn sibling_path(path: &Path, fallback: &str, suffix: &str) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| fallback.to_owned());
    path.with_file_name(format!("{name}{suffix}"))
}

/// 把任意载荷加密成可落盘的密文串（Windows: `dpapi:<base64>`；其它平台：明文 + 0600）。
///
/// 公开入口的动机：**同一台机器上不该有两套凭据加密实现**。SIWC 的
/// OAuth token（`crates/qaqh-siwc`）复用这里，从而与 `secrets.toml`
/// 的密钥享有一条一致的策略与一次代码审查。
pub fn protect(plain: &[u8]) -> Result<String, String> {
    encrypt(plain)
}

/// [`protect`] 的逆操作。**解密失败不得回退到读取旧的明文值**（与 secrets 同纪律）。
pub fn unprotect(cipher: &str) -> Result<Vec<u8>, String> {
    decrypt(cipher).map(String::into_bytes)
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn encrypt(plain: &[u8]) -> Result<String, String> {
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Cryptography::{CRYPT_INTEGER_BLOB, CryptProtectData};
    use base64::{Engine as _, engine::general_purpose::STANDARD};

    // CRYPTPROTECT_UI_FORBIDDEN: never show a prompt; CurrentUser scope.
    const CRYPTPROTECT_UI_FORBIDDEN: u32 = 0x1;

    let in_blob = CRYPT_INTEGER_BLOB {
        cbData: plain.len() as u32,
        pbData: plain.as_ptr() as *mut u8,
    };
    let mut out_blob = CRYPT_INTEGER_BLOB::default();
    // SAFETY: in_blob 指向存活的 plain 缓冲，out_blob 由本次调用填充，
    // 读出后立即 LocalFree，无悬垂。
    unsafe {
        CryptProtectData(
            &in_blob,
            windows::core::PCWSTR::null(),
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out_blob,
        )
        .map_err(|e| format!("CryptProtectData failed: {e}"))?;
        let bytes = std::slice::from_raw_parts(out_blob.pbData, out_blob.cbData as usize).to_vec();
        let _ = LocalFree(Some(HLOCAL(out_blob.pbData as *mut _)));
        Ok(format!("dpapi:{}", STANDARD.encode(&bytes)))
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn decrypt(raw: &str) -> Result<String, String> {
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Cryptography::{CRYPT_INTEGER_BLOB, CryptUnprotectData};
    use base64::{Engine as _, engine::general_purpose::STANDARD};

    let b64 = raw
        .strip_prefix("dpapi:")
        .ok_or_else(|| "secret is not a dpapi blob".to_string())?;
    let blob = STANDARD
        .decode(b64)
        .map_err(|e| format!("secret base64 解码失败: {e}"))?;
    let in_blob = CRYPT_INTEGER_BLOB {
        cbData: blob.len() as u32,
        pbData: blob.as_ptr() as *mut u8,
    };
    let mut out_blob = CRYPT_INTEGER_BLOB::default();
    // SAFETY: in_blob 指向存活的 blob 缓冲，out_blob 由本次调用填充，
    // 读出后立即 LocalFree，无悬垂。
    unsafe {
        CryptUnprotectData(&in_blob, None, None, None, None, 0, &mut out_blob)
            .map_err(|e| format!("CryptUnprotectData failed: {e}"))?;
        let bytes = std::slice::from_raw_parts(out_blob.pbData, out_blob.cbData as usize).to_vec();
        let _ = LocalFree(Some(HLOCAL(out_blob.pbData as *mut _)));
        String::from_utf8(bytes).map_err(|_| "decrypted secret is not utf-8".to_string())
    }
}

#[cfg(not(windows))]
fn encrypt(plain: &[u8]) -> Result<String, String> {
    // No DPAPI outside Windows: keep the value separated from config.toml and
    // rely on 0600 file permissions. TODO: keyring integration.
    String::from_utf8(plain.to_vec()).map_err(|_| "secret is not utf-8".to_string())
}

#[cfg(not(windows))]
fn decrypt(raw: &str) -> Result<String, String> {
    Ok(raw.to_string())
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

    // DPAPI blob 的线格式（RFC 4648 §4 标准字母表 + canonical padding）自 v4.5
    // P2-2a 起由 `base64` crate 保证（plan §三.4-1：手写轮子退役）。
    use base64::{Engine as _, engine::general_purpose::STANDARD};

    #[test]
    fn base64_roundtrip() {
        for sample in [
            b"".as_slice(),
            b"f",
            b"fo",
            b"foo",
            b"foob",
            b"fooba",
            b"foobar",
            b"sk-1234567890-abcdefghijklmnopqrstuvwxyz",
        ] {
            let encoded = STANDARD.encode(sample);
            assert_eq!(
                STANDARD.decode(&encoded).unwrap(),
                sample,
                "sample: {sample:?}"
            );
        }
        assert_eq!(STANDARD.encode(b"foo"), "Zm9v");
        assert_eq!(STANDARD.encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let secret = "sk-test-secret-value";
        let encoded = encrypt(secret.as_bytes()).expect("encrypt");
        #[cfg(windows)]
        assert!(
            encoded.starts_with("dpapi:"),
            "windows secrets are dpapi blobs"
        );
        assert_eq!(decrypt(&encoded).expect("decrypt"), secret);
    }

    #[test]
    fn store_roundtrip_delete() {
        let dir = std::env::temp_dir().join(format!("qaqh-secrets-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("secrets.toml");
        let store = SecretStore::new(path.clone());

        assert!(!store.has(SecretSlot::Main));
        assert!(store.load(SecretSlot::Main).is_none());

        store.set(SecretSlot::Main, "sk-main").expect("set main");
        assert!(store.has(SecretSlot::Main));
        assert_eq!(store.load(SecretSlot::Main).as_deref(), Some("sk-main"));

        store
            .set(SecretSlot::Main, "sk-main-2")
            .expect("re-set main");
        assert_eq!(store.load(SecretSlot::Main).as_deref(), Some("sk-main-2"));

        store.delete(SecretSlot::Main).expect("delete main");
        assert!(!store.has(SecretSlot::Main));
        assert!(store.load(SecretSlot::Main).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn marker_value_is_opaque() {
        assert_eq!(CONFIG_MARKER, "set");
    }
}
