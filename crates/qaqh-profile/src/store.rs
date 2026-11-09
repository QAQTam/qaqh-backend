//! providers.toml 持久化存储与安全原子修改。
//!
//! 核心安全策略：
//! 1. `delete_provider` / `delete_model` 前校验引用完整性（default_model_selection），杜绝悬挂引用；
//! 2. 采用 tmp-write + rename 原子写入，防止断电导致配置损坏。

use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;
use crate::model::{ModelDef, ModelSelection, ProfilesDocument, ProviderDef};

#[derive(Debug, Error)]
pub enum ProfileStoreError {
    #[error("配置文件 I/O 错误: {0}")]
    Io(#[from] std::io::Error),
    #[error("配置文件 TOML 格式错误: {0}")]
    TomlDe(#[from] toml::de::Error),
    #[error("配置文件序列化错误: {0}")]
    TomlSer(#[from] toml::ser::Error),
    #[error("引用的 Provider「{0}」不存在")]
    ProviderNotFound(String),
    #[error("Provider「{0}」仍被 Profile「{1}」引用，无法删除")]
    ProviderInUse(String, String),
    #[error("模型 {0}/{1} 正被选择引用")]
    ModelInUse(String, String),
    #[error("模型 {0}/{1} 未注册")]
    ModelNotFound(String, String),
    #[error("目录修订冲突：expected {expected}, actual {actual}")]
    RevisionConflict { expected: u64, actual: u64 },
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
            let doc = ProfilesDocument::default();
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

    pub fn check_revision(&self, expected: u64) -> Result<(), ProfileStoreError> {
        if self.doc.revision != expected {
            return Err(ProfileStoreError::RevisionConflict { expected, actual: self.doc.revision });
        }
        Ok(())
    }

    fn touch(&mut self) { self.doc.revision = self.doc.revision.saturating_add(1); }

    pub fn upsert_model(&mut self, provider_id: &str, model_id: &str, model: ModelDef) -> Result<(), ProfileStoreError> {
        let provider = self.doc.providers.get_mut(provider_id)
            .ok_or_else(|| ProfileStoreError::ProviderNotFound(provider_id.to_string()))?;
        provider.models.insert(model_id.to_string(), model);
        self.touch();
        self.save()
    }

    pub fn delete_model(&mut self, provider_id: &str, model_id: &str) -> Result<(), ProfileStoreError> {
        if self.doc.model_is_referenced(provider_id, model_id) {
            return Err(ProfileStoreError::ModelInUse(provider_id.to_string(), model_id.to_string()));
        }
        let provider = self.doc.providers.get_mut(provider_id)
            .ok_or_else(|| ProfileStoreError::ProviderNotFound(provider_id.to_string()))?;
        if provider.models.remove(model_id).is_none() {
            return Err(ProfileStoreError::ModelNotFound(provider_id.to_string(), model_id.to_string()));
        }
        self.touch();
        self.save()
    }

    pub fn set_default_model_selection(&mut self, selection: ModelSelection) -> Result<(), ProfileStoreError> {
        self.doc.default_model_selection = Some(selection);
        self.touch();
        self.save()
    }

    /// 原子保存至磁盘
    pub fn save(&self) -> Result<(), ProfileStoreError> {
        let serialized = toml::to_string_pretty(&self.doc)?;
        let tmp_path = self.path.with_extension("toml.tmp");
        fs::write(&tmp_path, serialized)?;
        fs::rename(&tmp_path, &self.path)?;
        Ok(())
    }

    /// 新增或更新一个 Provider
    pub fn upsert_provider(&mut self, id: String, def: ProviderDef) -> Result<(), ProfileStoreError> {
        if let Some(selection) = &self.doc.default_model_selection
            && selection.provider_id == id
            && !def.models.contains_key(&selection.model_id)
        {
            return Err(ProfileStoreError::ModelInUse(id, selection.model_id.clone()));
        }
        self.doc.providers.insert(id, def);
        self.touch();
        self.save()?;
        Ok(())
    }

    /// 删除一个 Provider（先检查 default_model_selection 是否在引用）
    pub fn delete_provider(&mut self, id: &str) -> Result<(), ProfileStoreError> {
        if self.doc.default_model_selection.as_ref().is_some_and(|selection| selection.provider_id == id) {
            return Err(ProfileStoreError::ProviderInUse(id.to_string(), "default_model_selection".into()));
        }
        if !self.doc.providers.contains_key(id) {
            return Err(ProfileStoreError::ProviderNotFound(id.to_string()));
        }
        self.doc.providers.remove(id);
        self.touch();
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
        let path = dir.path().join("providers.toml");

        let mut store = ProfilesStore::load_or_create(&path).expect("create store");
        assert!(store.document().default_model_selection.is_none());
        assert!(store.document().providers.is_empty());

        store.upsert_provider("acme".into(), ProviderDef {
            name: "Acme".into(), base_url: "https://example.invalid/v1".into(), wire: qaqh_types::Wire::Chat,
            credential: qaqh_types::CredentialSource::ApiKey, api_key: None, headers: Default::default(), compat: None,
            models: Default::default(),
        }).expect("provider");
        store.upsert_model("acme", "m", ModelDef::default()).expect("model");
        store.set_default_model_selection(ModelSelection { provider_id: "acme".into(), model_id: "m".into(), options: Default::default() }).expect("selection");
        assert_eq!(store.document().default_model_selection.as_ref().unwrap().model_id, "m");

        // 重新从磁盘读取，验证持久化无误
        let reloaded = ProfilesStore::load_or_create(&path).expect("reload");
        assert_eq!(reloaded.document().default_model_selection.as_ref().unwrap().provider_id, "acme");
    }

    #[test]
    fn test_load_or_create_starts_empty() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("providers.toml");

        let store = ProfilesStore::load_or_create(&path).expect("create store");
        assert!(store.document().default_model_selection.is_none());
    }
}
