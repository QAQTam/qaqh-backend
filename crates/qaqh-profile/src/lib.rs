//! QAQ-Harness 模块化 Profile / Provider / Model 领域体系。
//!
//! 对齐成熟业界规范（参考 ZCode），彻底根除“全局配置污染”：
//! 1. 结构正交：Provider 维护端点与多 Models，Profile 组装可复用预设；
//! 2. 存储隔离：写入单独的 `profiles.toml`，创建预设零全局副作用；
//! 3. 权威解析：会话可自由绑定 Profile，未指定时纯净回退到全局默认预设。


#![cfg_attr(not(test), forbid(unsafe_code))]
pub mod model;
pub mod resolver;
pub mod store;

pub use model::{ModelDef, ModelDefaults, ModelSelection, ProfileDef, ProfilesDocument, ProviderDef};
pub use resolver::{resolve_effective, resolve_model_selection, EffectiveExecutionProfile, EffectiveModelConfig, ModelResolveError, ProfileResolveError};
pub use store::{ProfilesStore, ProfileStoreError};

use std::path::PathBuf;

/// 获取默认 profiles.toml 路径（与 config.toml / secrets.toml 同目录）。
///
/// 旧 profile 名体系的落盘文件；新 BYOK 目录用 [`default_providers_path`]。
/// 新 BYOK 目录（[`ProfilesDocument`]）的权威落盘路径。
///
/// `Config::load` 从这里装载 `profiles_catalog`，provider/model 的全部读写
/// 必须落在这一个文件；旧 `profiles.toml` 属于已废除的 profile 名体系。
pub fn default_providers_path() -> PathBuf {
    qaqh_types::platform::config_path().with_file_name("providers.toml")
}
