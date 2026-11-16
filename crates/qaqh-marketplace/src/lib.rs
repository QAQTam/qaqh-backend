//! QAQ-Harness 插件市场：codex / claudecode 双宿主兼容（ADR-0022）。
//!
//! 正典清单 = `.agents/plugins/marketplace.json`（中立命名空间；Codex 清单
//! 查找序把它排在第一优先，occ 侧配套补丁读同一路径）。wire 形状取 Claude
//! marketplace spec 与 Codex `RawMarketplaceManifestPluginSourceObject` 的
//! **真交集**，不在交集的形态解析即拒——不写转换层、不做形态兼容（宪法 4/9）。
//!
//! 分层：
//! - [`manifest`]：清单模型 + fail-fast 校验
//! - [`registry`]：已知市场注册表 + git clone/pull 缓存（`~/.qaqh/marketplaces/`）
//! - [`installer`]：市场插件 → 宿主投影（skills 拷贝 + qaqh MCP 导入）

#![cfg_attr(not(test), deny(unsafe_code))]

pub mod installer;
pub mod manifest;
pub mod registry;

pub use manifest::{MarketplaceManifest, PluginEntry, PluginSource};
pub use registry::{RegisteredMarketplace, Registry};
