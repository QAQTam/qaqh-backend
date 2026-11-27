//! Config 与 Provider 目录管理域强类型 RPC。

use qaqh_core_vocab::{CredentialSource, JsonText, ModelSelection, Wire};
use serde::{Deserialize, Serialize};

/// 配置分区。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConfigSection {
    /// 模型与端点。
    Providers,
    /// 工具与权限。
    Tools,
    /// 沙箱。
    Sandbox,
    /// 界面偏好。
    Ui,
    /// 钩子。
    Hooks,
}

/// 内置服务商预设视图。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderPresetView {
    /// 预设 id。
    pub id: String,
    /// 展示名称。
    pub name: String,
    /// 默认 API 根地址。
    pub base_url: String,
    /// 线缆协议类别。
    pub wire: Wire,
    /// 凭据来源类别。
    pub credential: CredentialSource,
}

/// 配置域请求。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConfigRequest {
    /// 读一个分区（原始 JSON 文本搬运）。
    Get {
        /// 分区。
        section: ConfigSection,
    },
    /// 写一个分区。
    Set {
        /// 分区。
        section: ConfigSection,
        /// 原始 JSON 文本。
        value: JsonText,
    },
    /// 重新加载磁盘配置。
    Reload,
    /// 读取全量只读配置快照（`ConfigDto` 原始文本）。
    LoadFull,
    /// 应用全量配置补丁（`ConfigPatch` 原始文本）。
    PatchFull {
        /// 补丁负载。
        patch: JsonText,
    },
    /// 查询服务商与模型目录。
    ListProviders,
    /// 查询官方内置服务商预设列表。
    ListProviderPresets,
    /// 新增或更新服务商条目。
    UpsertProvider {
        /// 服务商载体（已做秘密脱敏/表单提交）。
        provider: JsonText,
        /// 乐观锁配置修订号。
        expected_revision: u64,
    },
    /// 删除服务商。
    DeleteProvider {
        /// 服务商 id。
        provider_id: String,
        /// 乐观锁配置修订号。
        expected_revision: u64,
    },
    /// 新增或更新服务商下的模型。
    UpsertProviderModel {
        /// 服务商 id。
        provider_id: String,
        /// 模型配置。
        model: JsonText,
        /// 乐观锁配置修订号。
        expected_revision: u64,
    },
    /// 删除服务商下的模型。
    DeleteProviderModel {
        /// 服务商 id。
        provider_id: String,
        /// 模型 id。
        model_id: String,
        /// 乐观锁配置修订号。
        expected_revision: u64,
    },
    /// 设置全局默认模型选择。
    SetDefaultModel {
        /// 默认模型选择。
        selection: ModelSelection,
        /// 乐观锁配置修订号。
        expected_revision: u64,
    },
    /// 重载全部会话代理配置与 MCP。
    ReloadAgents,
}

/// 配置域回执。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConfigResponse {
    /// 分区取值。
    Value {
        /// 分区。
        section: ConfigSection,
        /// 原始 JSON 文本。
        value: JsonText,
    },
    /// 全量配置文档。
    FullDocument {
        /// 原始文档。
        raw: JsonText,
    },
    /// 服务商目录快照。
    Providers {
        /// 权威服务商文档。
        raw: JsonText,
        /// 当前修订号。
        revision: u64,
    },
    /// 官方内置服务商预设表。
    ProviderPresets(Vec<ProviderPresetView>),
    /// 已写入。
    Stored,
    /// 已重载。
    Reloaded,
    /// 变更受理。
    Ack,
}
