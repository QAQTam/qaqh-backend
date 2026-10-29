//! profiles.toml 持久化存储与安全原子修改（彻底消除全局污染）。
//!
//! 核心安全策略：
//! 1. `create_profile` / `update_profile` 严格只作用于单个 Profile 实体，绝不碰 `default_profile`；
//! 2. `delete_provider` 前校验外键完整性，杜绝悬挂引用；
//! 3. 采用 tmp-write + rename 原子写入，防止断电导致配置损坏。

use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;
use crate::model::{ProfileDef, ProfilesDocument, ProviderDef};

#[derive(Debug, Error)]
pub enum ProfileStoreError {
    #[error("配置文件 I/O 错误: {0}")]
    Io(#[from] std::io::Error),
    #[error("配置文件 TOML 格式错误: {0}")]
    TomlDe(#[from] toml::de::Error),
    #[error("配置文件序列化错误: {0}")]
    TomlSer(#[from] toml::ser::Error),
    #[error("Profile「{0}」已存在")]
    ProfileAlreadyExists(String),
    #[error("Profile「{0}」不存在")]
    ProfileNotFound(String),
    #[error("引用的 Provider「{0}」不存在")]
    ProviderNotFound(String),
    #[error("Provider「{0}」仍被 Profile「{1}」引用，无法删除")]
    ProviderInUse(String, String),
    #[error("无法删除当前全局默认 Profile「{0}」，请先指定新的默认预设")]
    CannotDeleteDefaultProfile(String),
}

pub struct ProfilesStore {
    path: PathBuf,
    doc: ProfilesDocument,
}

impl ProfilesStore {
    /// 载入已有配置文件；若文件不存在，则写入标准默认模板
    pub fn load_or_create(path: &Path) -> Result<Self, ProfileStoreError> {
        if path.exists() {
            let content = fs::read_to_string(path)?;
            let doc: ProfilesDocument = toml::from_str(&content)?;
            Ok(Self {
                path: path.to_path_buf(),
                doc,
            })
        } else {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let doc = ProfilesDocument::create_builtin_template();
            let store = Self {
                path: path.to_path_buf(),
                doc,
            };
            store.save()?;
            Ok(store)
        }
    }

    /// 当前完整的配置只读引用
    pub fn document(&self) -> &ProfilesDocument {
        &self.doc
    }

    /// 原子保存至磁盘
    pub fn save(&self) -> Result<(), ProfileStoreError> {
        let serialized = toml::to_string_pretty(&self.doc)?;
        let tmp_path = self.path.with_extension("toml.tmp");
        fs::write(&tmp_path, serialized)?;
        fs::rename(&tmp_path, &self.path)?;
        Ok(())
    }

    /// 创建一个全新的 Profile 预设（【绝对零污染】：完全不改变全局默认预设指针）
    pub fn create_profile(&mut self, id: String, def: ProfileDef) -> Result<(), ProfileStoreError> {
        let trimmed_id = id.trim();
        if trimmed_id.is_empty() {
            return Err(ProfileStoreError::ProfileNotFound("id 不能为空".to_string()));
        }
        if self.doc.profiles.contains_key(trimmed_id) {
            return Err(ProfileStoreError::ProfileAlreadyExists(trimmed_id.to_string()));
        }
        if !self.doc.providers.contains_key(&def.provider) {
            return Err(ProfileStoreError::ProviderNotFound(def.provider.clone()));
        }

        self.doc.profiles.insert(trimmed_id.to_string(), def);
        self.save()?;
        Ok(())
    }

    /// 更新一个已存在的 Profile
    pub fn update_profile(&mut self, id: &str, def: ProfileDef) -> Result<(), ProfileStoreError> {
        if !self.doc.profiles.contains_key(id) {
            return Err(ProfileStoreError::ProfileNotFound(id.to_string()));
        }
        if !self.doc.providers.contains_key(&def.provider) {
            return Err(ProfileStoreError::ProviderNotFound(def.provider.clone()));
        }

        self.doc.profiles.insert(id.to_string(), def);
        self.save()?;
        Ok(())
    }

    /// 删除一个 Profile（不能是当前的全局默认）
    pub fn delete_profile(&mut self, id: &str) -> Result<(), ProfileStoreError> {
        if id == self.doc.default_profile {
            return Err(ProfileStoreError::CannotDeleteDefaultProfile(id.to_string()));
        }
        if self.doc.profiles.remove(id).is_none() {
            return Err(ProfileStoreError::ProfileNotFound(id.to_string()));
        }
        self.save()?;
        Ok(())
    }

    /// 显式切换全局默认 Profile 指针
    pub fn set_default_profile(&mut self, id: &str) -> Result<(), ProfileStoreError> {
        if !self.doc.profiles.contains_key(id) {
            return Err(ProfileStoreError::ProfileNotFound(id.to_string()));
        }
        self.doc.default_profile = id.to_string();
        self.save()?;
        Ok(())
    }

    /// 新增或更新一个 Provider
    pub fn upsert_provider(&mut self, id: String, def: ProviderDef) -> Result<(), ProfileStoreError> {
        self.doc.providers.insert(id, def);
        self.save()?;
        Ok(())
    }

    /// 删除一个 Provider（先检查是否有 Profile 在引用）
    pub fn delete_provider(&mut self, id: &str) -> Result<(), ProfileStoreError> {
        for (prof_id, prof) in &self.doc.profiles {
            if prof.provider == id {
                return Err(ProfileStoreError::ProviderInUse(id.to_string(), prof_id.clone()));
            }
        }
        if !self.doc.providers.contains_key(id) {
            return Err(ProfileStoreError::ProviderNotFound(id.to_string()));
        }
        self.doc.providers.remove(id);
        self.save()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_store_creation_and_zero_side_effect() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("profiles.toml");

        let mut store = ProfilesStore::load_or_create(&path).expect("create store");
        assert_eq!(store.document().default_profile, "deepseek-r1");

        // 新建一个自定义 Profile B
        let new_prof = ProfileDef {
            name: "我的测试预设".to_string(),
            provider: "deepseek".to_string(),
            model: "deepseek-chat".to_string(),
            reasoning_effort: None,
            context_length: Some(64_000),
            max_tokens: Some(4_096),
        };
        store.create_profile("my-test".to_string(), new_prof).expect("create profile");

        // 强力断言：全局默认预设绝对不受新建预设的任何影响！
        assert_eq!(store.document().default_profile, "deepseek-r1");
        assert!(store.document().profiles.contains_key("my-test"));

        // 重新从磁盘读取，验证持久化无误
        let reloaded = ProfilesStore::load_or_create(&path).expect("reload");
        assert_eq!(reloaded.document().default_profile, "deepseek-r1");
        assert!(reloaded.document().profiles.contains_key("my-test"));
    }

    #[test]
    fn test_cannot_delete_default_profile() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("profiles.toml");

        let mut store = ProfilesStore::load_or_create(&path).expect("create store");
        let err = store.delete_profile("deepseek-r1").expect_err("should reject deleting default");
        assert!(matches!(err, ProfileStoreError::CannotDeleteDefaultProfile(_)));
    }
}
