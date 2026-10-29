//! QAQ-Harness 模块化 Profile / Provider / Model 领域体系。
//!
//! 对齐成熟业界规范（参考 ZCode），彻底根除“全局配置污染”：
//! 1. 结构正交：Provider 维护端点与多 Models，Profile 组装可复用预设；
//! 2. 存储隔离：写入单独的 `profiles.toml`，创建预设零全局副作用；
//! 3. 权威解析：会话可自由绑定 Profile，未指定时纯净回退到全局默认预设。

pub mod model;
pub mod resolver;
pub mod store;

pub use model::{ModelDef, ProfileDef, ProfilesDocument, ProviderDef};
pub use resolver::{resolve_effective, EffectiveExecutionProfile, ProfileResolveError};
pub use store::{ProfilesStore, ProfileStoreError};

use std::path::PathBuf;

/// 获取默认 profiles.toml 路径（与 config.toml / secrets.toml 同目录）
pub fn default_profiles_path() -> PathBuf {
    qaqh_types::platform::config_path().with_file_name("profiles.toml")
}
