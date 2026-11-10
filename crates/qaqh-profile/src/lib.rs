//! QAQ-Harness 模块化 Provider / Model 领域体系。
//!
//! 对齐成熟业界规范（参考 ZCode），彻底根除“全局配置污染”：
//! 1. 结构正交：Provider 维护端点与多 Models，全局默认是一枚
//!    `default_model_selection` 指针（provider_id + model_id）；
//! 2. 存储隔离：唯一落盘文件是 `providers.toml`（providers + active）；
//! 3. 权威解析：会话/子代理按 ModelSelection 从目录 fail-fast 解析端点与凭据；
//! 4. 内置预设是一张代码静态表（[`presets`]），选择预设即复制默认值进目录。


#![cfg_attr(not(test), forbid(unsafe_code))]
pub mod model;
pub mod presets;
pub mod resolver;
pub mod store;

pub use model::{ModelDef, ModelDefaults, ModelSelection, ProfilesDocument, ProviderDef};
pub use presets::{PROVIDER_PRESETS, ProviderPreset, preset_compat, provider_preset};
pub use resolver::{resolve_model_selection, EffectiveModelConfig, ModelResolveError};
pub use store::{ProfilesStore, ProfileStoreError};

use std::path::PathBuf;

/// 获取 providers.toml 权威落盘路径（与 config.toml / secrets.toml 同目录）。
///
/// 新 BYOK 目录（[`ProfilesDocument`]）的唯一存储文件：`Config::load` 从这里
/// 装载 `profiles_catalog`，provider/model 的全部读写必须落在这一个文件；
/// 旧 `profiles.toml` 属于已废除的 profile 名体系。
pub fn default_providers_path() -> PathBuf {
    qaqh_types::platform::providers_path()
}
