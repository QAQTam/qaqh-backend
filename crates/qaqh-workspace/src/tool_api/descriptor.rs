//! 工具描述与注册契约（base spec §5；09-19 补充稿 §2/§3）。
//!
//! - [`ToolName`]：唯一查找键 + wire 标识，构造即校验；[`ToolName::namespace`]
//!   提供只读的结构化视图（不参与查找）。
//! - [`ToolDescriptor`]：模型面 / 权限 / 编排 / 预算的单一描述源。
//! - [`ToolDescriptor::validate`]：注册前校验（P1-② 验收点）。

use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::capabilities::ToolCapabilities;
use crate::ToolRisk;
use crate::permission::ToolCategory;

/// 工具规范名（`^[a-z][a-z0-9_]*$`，长度 ≤ [`ToolName::MAX_LEN`]）。
///
/// wire 形态为扁平字符串（`serde(transparent)`）；动态工具沿用
/// `mcp__{server}__{tool}` 前缀约定（base spec §5.1），命名空间经
/// [`ToolName::namespace`] 以只读视图解析。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ToolName(String);

impl ToolName {
    /// 名称长度上限（base spec §5.1）。
    pub const MAX_LEN: usize = 64;

    /// 构造并校验。非法名称返回 [`DescriptorError::InvalidName`]（不 panic）。
    pub fn new(raw: &str) -> Result<Self, DescriptorError> {
        validate_name(raw)?;
        Ok(Self(raw.to_owned()))
    }

    /// 扁平字符串视图。
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// 结构化命名空间视图（只读；不符合约定返回 `None`，不影响查找）。
    pub fn namespace(&self) -> Option<Namespace> {
        Namespace::parse(&self.0)
    }
}

impl fmt::Display for ToolName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn validate_name(raw: &str) -> Result<(), DescriptorError> {
    if raw.is_empty() {
        return Err(DescriptorError::InvalidName {
            reason: "名称为空"
        });
    }
    if raw.len() > ToolName::MAX_LEN {
        return Err(DescriptorError::InvalidName {
            reason: "名称超过 64 字符",
        });
    }
    let mut chars = raw.chars();
    match chars.next() {
        Some(first) if first.is_ascii_lowercase() => {}
        _ => {
            return Err(DescriptorError::InvalidName {
                reason: "首字符必须为小写字母",
            });
        }
    }
    for c in chars {
        if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
            return Err(DescriptorError::InvalidName {
                reason: "仅允许 [a-z0-9_]",
            });
        }
    }
    Ok(())
}

/// 命名空间：由 `__` 前缀约定识别的结构化视图（09-19 补充稿 §2.1）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Namespace {
    /// `mcp__{server}__{tool}`。
    Mcp { server: String },
    /// `lsp__{tool}`。
    Lsp,
    /// `extension__{id}__{tool}`（预留，供插件类来源使用）。
    Extension { id: String },
}

impl Namespace {
    /// 解析命名空间视图；不符合约定返回 `None`（调用方回退为"无命名空间"）。
    pub fn parse(name: &str) -> Option<Self> {
        if let Some(rest) = name.strip_prefix("mcp__") {
            let (server, tool) = rest.split_once("__")?;
            if server.is_empty() || tool.is_empty() {
                return None;
            }
            return Some(Namespace::Mcp {
                server: server.to_owned(),
            });
        }
        if let Some(rest) = name.strip_prefix("lsp__") {
            if rest.is_empty() {
                return None;
            }
            return Some(Namespace::Lsp);
        }
        if let Some(rest) = name.strip_prefix("extension__") {
            let (id, tool) = rest.split_once("__")?;
            if id.is_empty() || tool.is_empty() {
                return None;
            }
            return Some(Namespace::Extension { id: id.to_owned() });
        }
        None
    }
}

/// 工具暴露面（base spec §5.2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolExposure {
    /// 进入模型首轮工具清单。
    #[default]
    Direct,
    /// 注册但不在首轮清单（预留：工具搜索）。
    Deferred,
    /// 可由宿主调用，不进入模型面。
    Hidden,
    /// 仅供运行时/内部编排，不进入 client 工具清单。
    Internal,
}

/// 工具来源（base spec §5.1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolSource {
    /// 内置工具。
    Builtin,
    /// MCP 动态工具。
    Mcp,
    /// LSP 动态工具。
    Lsp,
    /// 扩展/插件来源。
    Extension,
}

/// 输出预算（base spec §5.1：由工具声明，禁止按名称猜）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OutputBudget {
    /// 模型可见文本字符上限；`None` = 不截断（折叠策略可覆盖）。
    pub model_chars: Option<usize>,
    /// exec 类工具内部 token 截断上限；`None` = 不截断。
    pub exec_max_output_tokens: Option<u32>,
}

/// 工具描述符：模型面 / 权限 / 编排 / 预算的单一描述源
/// （base spec §5.1 + 09-19 补充稿 §3）。
#[derive(Debug, Clone)]
pub struct ToolDescriptor {
    /// 规范名（唯一查找键）。
    pub name: ToolName,
    /// 上游原名（动态工具规范化后保留展示用；不参与查找）。
    pub display_name: Option<String>,
    /// 模型可见描述（不得为空）。
    pub description: String,
    /// 输入 JSON Schema（object；typed 工具由类型生成）。
    pub input_schema: serde_json::Value,
    /// 输出 JSON Schema（object；动态工具用上游 raw schema）。
    pub output_schema: serde_json::Value,
    /// 权限类别（权限决策单一事实源）。
    pub category: ToolCategory,
    /// 安全档位（路径范围 fail-closed 判定输入）。
    pub risk: ToolRisk,
    /// 默认超时（调用方显式 timeout 可覆盖）。
    pub default_timeout: Duration,
    /// 暴露面。
    pub exposure: ToolExposure,
    /// 来源。
    pub source: ToolSource,
    /// 输出预算。
    pub output_budget: OutputBudget,
    /// 编排能力声明（09-19 补充稿 §3）。
    pub capabilities: ToolCapabilities,
}

/// 工具元数据（SDK v2）：typed 工具作者声明的部分——schema 不在其中，
/// 由 [`TypedTool`](super::typed::TypedTool) 的 `Args`/`Output` 类型经
/// [`super::schema::schema_of`] 生成。
#[derive(Debug, Clone)]
pub struct ToolMeta {
    /// 规范名（唯一查找键）。
    pub name: ToolName,
    /// 上游原名（动态工具规范化后保留展示用；不参与查找）。
    pub display_name: Option<String>,
    /// 模型可见描述（不得为空）。
    pub description: String,
    /// 权限类别（权限决策单一事实源）。
    pub category: ToolCategory,
    /// 安全档位（路径范围 fail-closed 判定输入）。
    pub risk: ToolRisk,
    /// 默认超时（调用方显式 timeout 可覆盖）。
    pub default_timeout: Duration,
    /// 暴露面。
    pub exposure: ToolExposure,
    /// 来源。
    pub source: ToolSource,
    /// 输出预算。
    pub output_budget: OutputBudget,
    /// 编排能力声明（09-19 补充稿 §3）。
    pub capabilities: ToolCapabilities,
}

impl ToolMeta {
    /// 内置工具的最短构造：名称/描述/权限/档位/超时必填，其余取默认。
    pub fn new(
        name: &str,
        description: impl Into<String>,
        category: ToolCategory,
        risk: ToolRisk,
        default_timeout: Duration,
    ) -> Self {
        Self {
            name: ToolName::new(name).expect("内置工具名必须合法"),
            display_name: None,
            description: description.into(),
            category,
            risk,
            default_timeout,
            exposure: ToolExposure::Direct,
            source: ToolSource::Builtin,
            output_budget: OutputBudget::default(),
            capabilities: ToolCapabilities::default(),
        }
    }

    /// 覆写暴露面。
    #[must_use]
    pub fn with_exposure(mut self, exposure: ToolExposure) -> Self {
        self.exposure = exposure;
        self
    }

    /// 覆写编排能力声明。
    #[must_use]
    pub fn with_capabilities(mut self, capabilities: ToolCapabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// 覆写输出预算。
    #[must_use]
    pub fn with_output_budget(mut self, output_budget: OutputBudget) -> Self {
        self.output_budget = output_budget;
        self
    }
}

impl From<ToolMeta> for ToolDescriptor {
    fn from(meta: ToolMeta) -> Self {
        ToolDescriptor {
            name: meta.name,
            display_name: meta.display_name,
            description: meta.description,
            input_schema: serde_json::Value::Null,
            output_schema: serde_json::Value::Null,
            category: meta.category,
            risk: meta.risk,
            default_timeout: meta.default_timeout,
            exposure: meta.exposure,
            source: meta.source,
            output_budget: meta.output_budget,
            capabilities: meta.capabilities,
        }
    }
}

impl ToolDescriptor {
    /// 注册前校验（P1-②）：
    /// - 描述非空；
    /// - input/output schema 为 object；
    /// - 非交互工具默认超时非零（交互工具允许 `default_timeout = 0`）。
    ///
    /// 名称合法性在 [`ToolName`] 构造期已保证，无需重复校验。
    pub fn validate(&self) -> Result<(), DescriptorError> {
        if self.description.trim().is_empty() {
            return Err(DescriptorError::EmptyDescription);
        }
        if !self.input_schema.is_object() {
            return Err(DescriptorError::SchemaNotObject {
                field: "input_schema",
            });
        }
        if !self.output_schema.is_object() {
            return Err(DescriptorError::SchemaNotObject {
                field: "output_schema",
            });
        }
        if self.default_timeout.is_zero() && !self.capabilities.interactive {
            return Err(DescriptorError::ZeroTimeout);
        }
        Ok(())
    }
}

/// 描述符构造/校验错误（不 panic）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DescriptorError {
    /// 名称非法（构造期）。
    InvalidName {
        /// 具体原因（静态文案）。
        reason: &'static str,
    },
    /// 描述为空。
    EmptyDescription,
    /// schema 不是 object。
    SchemaNotObject {
        /// 字段名（`input_schema` / `output_schema`）。
        field: &'static str,
    },
    /// 默认超时为零。
    ZeroTimeout,
}

impl fmt::Display for DescriptorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName { reason } => write!(f, "非法工具名: {reason}"),
            Self::EmptyDescription => f.write_str("工具描述不得为空"),
            Self::SchemaNotObject { field } => write!(f, "{field} 必须是 JSON Schema object"),
            Self::ZeroTimeout => f.write_str("default_timeout 不得为零"),
        }
    }
}

impl std::error::Error for DescriptorError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_names() {
        for raw in ["read", "web_fetch", "mcp__fs__read_file", "a1_b2", "x"] {
            assert!(ToolName::new(raw).is_ok(), "{raw} 应合法");
        }
    }

    #[test]
    fn rejects_invalid_names() {
        for raw in ["", "Read", "_x", "1x", "a-b", "a b", "工具"] {
            assert!(ToolName::new(raw).is_err(), "{raw} 应非法");
        }
        let long = "a".repeat(ToolName::MAX_LEN + 1);
        assert!(ToolName::new(&long).is_err(), "超长名应非法");
        let edge = "a".repeat(ToolName::MAX_LEN);
        assert!(ToolName::new(&edge).is_ok(), "边界长度应合法");
    }

    #[test]
    fn parses_namespaces_without_changing_lookup_key() {
        let mcp = ToolName::new("mcp__fs__read_file").expect("valid");
        assert_eq!(
            mcp.namespace(),
            Some(Namespace::Mcp {
                server: "fs".to_owned()
            })
        );
        assert_eq!(mcp.as_str(), "mcp__fs__read_file", "查找键不变");

        let lsp = ToolName::new("lsp__definition").expect("valid");
        assert_eq!(lsp.namespace(), Some(Namespace::Lsp));

        let ext = ToolName::new("extension__demo__run").expect("valid");
        assert_eq!(
            ext.namespace(),
            Some(Namespace::Extension {
                id: "demo".to_owned()
            })
        );

        let builtin = ToolName::new("read").expect("valid");
        assert_eq!(builtin.namespace(), None);

        // 约定不完整时不给出视图（回退为无命名空间），不报错。
        let malformed = ToolName::new("mcp__only_server").expect("valid");
        assert_eq!(malformed.namespace(), None);
        let malformed_lsp = ToolName::new("lsp__").expect("valid");
        assert_eq!(malformed_lsp.namespace(), None);
    }

    fn descriptor() -> ToolDescriptor {
        ToolDescriptor {
            name: ToolName::new("read").expect("valid"),
            display_name: None,
            description: "读取文件".to_owned(),
            input_schema: serde_json::json!({"type": "object"}),
            output_schema: serde_json::json!({"type": "object"}),
            category: ToolCategory::Read,
            risk: ToolRisk::ReadOnly,
            default_timeout: Duration::from_secs(30),
            exposure: ToolExposure::Direct,
            source: ToolSource::Builtin,
            output_budget: OutputBudget::default(),
            capabilities: ToolCapabilities::default(),
        }
    }

    #[test]
    fn validate_accepts_well_formed_descriptor() {
        assert_eq!(descriptor().validate(), Ok(()));
    }

    #[test]
    fn validate_rejects_empty_description_schema_and_zero_timeout() {
        let mut d = descriptor();
        d.description = "  ".to_owned();
        assert_eq!(d.validate(), Err(DescriptorError::EmptyDescription));

        let mut d = descriptor();
        d.input_schema = serde_json::json!("not-an-object");
        assert_eq!(
            d.validate(),
            Err(DescriptorError::SchemaNotObject {
                field: "input_schema"
            })
        );

        let mut d = descriptor();
        d.output_schema = serde_json::json!([]);
        assert_eq!(
            d.validate(),
            Err(DescriptorError::SchemaNotObject {
                field: "output_schema"
            })
        );

        let mut d = descriptor();
        d.default_timeout = Duration::ZERO;
        assert_eq!(d.validate(), Err(DescriptorError::ZeroTimeout));
    }
}
