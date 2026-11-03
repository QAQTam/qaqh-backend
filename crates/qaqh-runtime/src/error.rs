//! `qaqh-runtime` 的错误类型。
//!
//! 设计约定：**内部层用枚举，边界层才降级成文本**。此前 runtime 内部一律
//! `Result<_, String>`，调用方只能靠嗅探 message 前缀（例如
//! `starts_with("FORBIDDEN")`）判断语义——同一份知识散落在生产代码与测试里。
//! 这里把语义钉在变体上，`Display` 只负责给人看的文本（保留既有 wire 文案）。
//!
//! - [`QaqhError::UnknownMethod`] / [`QaqhError::InvalidParams`]：客户端问题（400）；
//! - [`QaqhError::Forbidden`]：越权 / 白名单拒绝（403，`Display` 保留 `FORBIDDEN: ` 前缀）；
//! - [`QaqhError::NotFound`]：目标不存在（404）；
//! - [`QaqhError::Invalid`]：请求本身合法但被业务规则拒绝（400）；
//! - [`QaqhError::Context`] / [`QaqhError::Unavailable`] / [`QaqhError::Serialize`]：本机状态问题（5xx）。
//!
//! 与 `qaqh-config` / `qaqh-session` 等下游 crate 的边界仍是它们的错误类型或
//! 字符串；跨边界时用 [`QaqhError::message`] / [`QaqhError::context`] 显式收口，
//! 不让 `String` 在 runtime 内部继续流动。

use serde_json::Value;

/// 服务面 / 运行时统一的错误分类。
#[derive(Debug, thiserror::Error)]
pub enum QaqhError {
    #[error("unknown method: {method}")]
    UnknownMethod { method: String },

    #[error("invalid params for {method}: {detail}")]
    InvalidParams { method: String, detail: String },

    /// 越权 / 白名单拒绝。`Display` 保留 `FORBIDDEN: ` 前缀，wire 文案不变。
    #[error("FORBIDDEN: {message}")]
    Forbidden { message: String },

    #[error("{message}")]
    NotFound { message: String },

    #[error("{message}")]
    Invalid { message: String },

    #[error("{message}")]
    Conflict { message: String },

    /// 带上下文的失败：`{context}: {message}`（例如 `session.create: allocate session failed: …`）。
    #[error("{context}: {message}")]
    Context {
        context: &'static str,
        message: String,
    },

    #[error("{what} unavailable")]
    Unavailable { what: &'static str },

    /// 文件系统操作失败（`Display` 保持旧文案 `{op} {path}: {source}`）。
    #[error("{op} {path}: {source}")]
    Io {
        op: &'static str,
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("serialize {what}: {source}")]
    Serialize {
        what: &'static str,
        #[source]
        source: serde_json::Error,
    },
}

impl QaqhError {
    /// 业务规则拒绝（参数合法、状态不允许）。
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid {
            message: message.into(),
        }
    }

    /// 目标不存在。
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::NotFound {
            message: message.into(),
        }
    }

    /// 承接下游 crate 的字符串错误：显式标注「这里是边界，丢失结构」。
    pub fn message(error: impl std::fmt::Display) -> Self {
        Self::Invalid {
            message: error.to_string(),
        }
    }

    /// 承接下游失败并加上本层的操作上下文。
    pub fn context(context: &'static str, error: impl std::fmt::Display) -> Self {
        Self::Context {
            context,
            message: error.to_string(),
        }
    }

    /// 序列化任意 DTO 失败（本机 bug / 数据异常，不是客户端问题）。
    pub fn serialize(what: &'static str, source: serde_json::Error) -> Self {
        Self::Serialize { what, source }
    }

    /// 把 DTO 序列化成 wire `Value`；失败归类为 [`QaqhError::Serialize`]。
    pub fn to_value<T: serde::Serialize>(what: &'static str, value: T) -> QaqhResult<Value> {
        serde_json::to_value(value).map_err(|source| Self::serialize(what, source))
    }

    pub fn is_forbidden(&self) -> bool {
        matches!(self, Self::Forbidden { .. })
    }
}

pub type QaqhResult<T> = Result<T, QaqhError>;

/// 边界转换：`String` → [`QaqhError::Invalid`]。
///
/// registry / agent 层仍有不少 `Result<_, String>` 的辅助函数，也仍要与
/// `qaqh-session` / `qaqh-ringing` 等下游的字符串错误打交道。有了这条转换，
/// `?` 与 `ok_or_else` 可以原样把它们抬进本类型，**文案一字不变**。
///
/// 这是**单向收口点**：新代码不要再构造 `String` 错误，只需在跨旧接口时
/// 让它自动收敛。
impl From<String> for QaqhError {
    fn from(message: String) -> Self {
        Self::Invalid { message }
    }
}

/// 边界转换：`&str` → [`QaqhError::Invalid`]（`"...".into()` 形态）。
impl From<&str> for QaqhError {
    fn from(message: &str) -> Self {
        Self::Invalid {
            message: message.to_string(),
        }
    }
}

/// 边界转换：agent catalog 的注册/校验错误 → [`QaqhError::Invalid`]。
///
/// 目录错误的变体（重复 id、父路径缺失…）目前只用于内部 diagnostics，服务面
/// 只需要原样带出文案，因此统一走 `Display`。
impl From<crate::agent_catalog::AgentCatalogError> for QaqhError {
    fn from(error: crate::agent_catalog::AgentCatalogError) -> Self {
        Self::message(error)
    }
}

/// 边界转换：[`QaqhError`] → `String`（HTTP / 工具结果等旧签名仍是字符串）。
///
/// 有了它，仍在 `Result<_, String>` 的老接口可以直接 `?` 掉一个 `QaqhError`，
/// 降级动作只发生在这一处，而不是每个调用点各写一次 `.to_string()`。
impl From<QaqhError> for String {
    fn from(error: QaqhError) -> Self {
        error.to_string()
    }
}

/// 测试专用相等：同变体且文案一致即视为相等。
///
/// `Io` / `Serialize` 携带的 source 不实现 `PartialEq`，因此不比较它们；
/// 生产代码不应按相等判断错误（用 [`QaqhError::is_forbidden`] 之类的谓词）。
#[cfg(test)]
impl PartialEq for QaqhError {
    fn eq(&self, other: &Self) -> bool {
        std::mem::discriminant(self) == std::mem::discriminant(other)
            && self.to_string() == other.to_string()
    }
}
