//! 工具错误模型（base spec §7）。
//!
//! - [`ToolError`]：可恢复错误（回给模型）。
//! - [`FatalToolError`]：内部 fatal（上抛 runtime，不上模型 wire）。
//! - [`ToolExecutionError`]：执行层统一返回类型（显式区分两者）。

use std::fmt;

/// 可恢复错误分类（封闭集合，base spec §7.1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolErrorKind {
    /// 参数非法（反序列化/校验失败）。
    InvalidArguments,
    /// 目标不存在。
    NotFound,
    /// 冲突（并发修改、hash 失配等）。
    Conflict,
    /// 权限拒绝。
    PermissionDenied,
    /// 未授权（凭证缺失/失效）。
    Unauthorized,
    /// 超时（base spec §6.4：可恢复终态）。
    Timeout,
    /// 取消（用户/系统；base spec §6.4：可恢复终态）。
    Cancelled,
    /// 网络失败。
    Network,
    /// 执行失败（工具内部逻辑）。
    Execution,
    /// 暂不可用（可重试）。
    Unavailable,
    /// 工具特有错误（必须携带命名空间 code）。
    Custom,
}

impl ToolErrorKind {
    /// 内置 code（`Custom` 无内置 code，必须携带命名空间 code）。
    pub fn builtin_code(self) -> Option<&'static str> {
        Some(match self {
            Self::InvalidArguments => "invalid_arguments",
            Self::NotFound => "not_found",
            Self::Conflict => "conflict",
            Self::PermissionDenied => "permission_denied",
            Self::Unauthorized => "unauthorized",
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
            Self::Network => "network",
            Self::Execution => "execution",
            Self::Unavailable => "unavailable",
            Self::Custom => return None,
        })
    }

    /// 默认可重试性（base spec §6.4：超时/网络/暂不可用可重试）。
    pub fn default_retryable(self) -> bool {
        matches!(self, Self::Timeout | Self::Network | Self::Unavailable)
    }
}

/// 稳定、机器可读的错误 code。
///
/// 形态：`^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)*$`。
/// [`ToolErrorKind::Custom`] 必须至少含一个命名空间段（`.` 分隔），
/// 例如 `mcp.rate_limited`、`edit.hash_mismatch`。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ToolErrorCode(String);

/// code 构造错误（不 panic）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolErrorCodeError {
    /// 具体原因（静态文案）。
    pub reason: &'static str,
}

impl ToolErrorCode {
    /// 解析并校验 code。
    pub fn parse(raw: &str) -> Result<Self, ToolErrorCodeError> {
        if raw.is_empty() {
            return Err(ToolErrorCodeError {
                reason: "code 不得为空",
            });
        }
        for segment in raw.split('.') {
            if segment.is_empty() {
                return Err(ToolErrorCodeError {
                    reason: "code 不允许空段（连续/首尾点）",
                });
            }
            let mut chars = segment.chars();
            match chars.next() {
                Some(first) if first.is_ascii_lowercase() => {}
                _ => {
                    return Err(ToolErrorCodeError {
                        reason: "code 段首字符必须为小写字母",
                    });
                }
            }
            for c in chars {
                if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
                    return Err(ToolErrorCodeError {
                        reason: "code 段仅允许 [a-z0-9_]",
                    });
                }
            }
        }
        Ok(Self(raw.to_owned()))
    }

    /// 由内置 kind 构造（code 由 kind 推导；`Custom` 返回 `None`）。
    pub fn builtin(kind: ToolErrorKind) -> Option<Self> {
        kind.builtin_code().map(|code| Self(code.to_owned()))
    }

    /// 桥接构造：legacy 错误码为 UPPER_SNAKE（如 `RESOURCE_MISMATCH`），
    /// 不做小写/命名空间校验。
    ///
    /// **仅限 legacy 适配层**（[`super::legacy`]）使用；新工具必须走
    /// [`Self::parse`] / [`Self::builtin`]。
    pub(crate) fn from_legacy(raw: &str) -> Self {
        Self(raw.to_owned())
    }

    /// 字符串视图。
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// 是否带命名空间（含 `.`）。
    pub fn is_namespaced(&self) -> bool {
        self.0.contains('.')
    }
}

impl fmt::Display for ToolErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 可恢复工具错误（base spec §7.1）。
///
/// `source` 仅开发诊断，不序列化、不发给模型。
#[derive(Debug)]
pub struct ToolError {
    /// 机器可读分类。
    pub kind: ToolErrorKind,
    /// 稳定 code（内置 kind 由 kind 推导；`Custom` 必须命名空间形态）。
    pub code: ToolErrorCode,
    /// 模型可见、具体、可操作的说明。
    pub detail: String,
    /// 是否允许模型或宿主重试。
    pub retryable: bool,
    /// 可选修正建议。
    pub hint: Option<String>,
    /// 字段级校验、冲突资源、候选位置等结构化数据。
    pub details: Option<serde_json::Value>,
    /// 仅开发诊断（不序列化）。
    source: Option<anyhow::Error>,
}

impl ToolError {
    /// 构造内置类错误（code 由 kind 推导、retryable 取 kind 默认）。
    ///
    /// `kind` 应为 [`ToolErrorKind::Custom`] 之外的分类；误传 `Custom` 时
    /// code 回退为 `custom.unspecified`（保持"Custom 必带命名空间"不变量）。
    pub fn new(kind: ToolErrorKind, detail: impl Into<String>) -> Self {
        let code = ToolErrorCode::builtin(kind)
            .unwrap_or_else(|| ToolErrorCode("custom.unspecified".to_owned()));
        Self {
            kind,
            code,
            detail: detail.into(),
            retryable: kind.default_retryable(),
            hint: None,
            details: None,
            source: None,
        }
    }

    /// 构造 `Custom` 错误（code 必须为命名空间形态，否则报构造错误）。
    pub fn custom(
        code: ToolErrorCode,
        detail: impl Into<String>,
    ) -> Result<Self, ToolErrorCodeError> {
        if !code.is_namespaced() {
            return Err(ToolErrorCodeError {
                reason: "Custom code 必须包含命名空间（如 mcp.rate_limited）",
            });
        }
        Ok(Self {
            kind: ToolErrorKind::Custom,
            code,
            detail: detail.into(),
            retryable: false,
            hint: None,
            details: None,
            source: None,
        })
    }

    /// 参数非法（P1-③）。
    pub fn invalid_arguments(detail: impl Into<String>) -> Self {
        Self::new(ToolErrorKind::InvalidArguments, detail)
    }

    /// 目标不存在（P1-③）。
    pub fn not_found(detail: impl Into<String>) -> Self {
        Self::new(ToolErrorKind::NotFound, detail)
    }

    /// 冲突（P1-③）。
    pub fn conflict(detail: impl Into<String>) -> Self {
        Self::new(ToolErrorKind::Conflict, detail)
    }

    /// 权限拒绝。
    pub fn permission_denied(detail: impl Into<String>) -> Self {
        Self::new(ToolErrorKind::PermissionDenied, detail)
    }

    /// 超时（retryable 默认 true）。
    pub fn timeout(detail: impl Into<String>) -> Self {
        Self::new(ToolErrorKind::Timeout, detail)
    }

    /// 取消（可恢复终态）。
    pub fn cancelled(detail: impl Into<String>) -> Self {
        Self::new(ToolErrorKind::Cancelled, detail)
    }

    /// 覆盖可重试性。
    pub fn with_retryable(mut self, retryable: bool) -> Self {
        self.retryable = retryable;
        self
    }

    /// 附加修正建议。
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    /// 附加结构化数据。
    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = Some(details);
        self
    }

    /// 附加诊断源（不序列化）。
    pub fn with_source(mut self, source: anyhow::Error) -> Self {
        self.source = Some(source);
        self
    }

    /// 诊断源只读视图。
    pub fn source(&self) -> Option<&anyhow::Error> {
        self.source.as_ref()
    }
}

/// 内部 fatal（base spec §7.2）：注册表损坏、凭证不一致、invariant 失败等。
/// **不得**作为普通 `ToolError` 回给模型。
#[derive(Debug)]
pub struct FatalToolError {
    /// fatal code（进 timeline failure.code）。
    pub code: String,
    /// 诊断消息（不直接进模型面）。
    pub message: String,
    /// 仅开发诊断。
    source: Option<anyhow::Error>,
}

impl FatalToolError {
    /// 构造 fatal。
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            source: None,
        }
    }

    /// 附加诊断源。
    pub fn with_source(mut self, source: anyhow::Error) -> Self {
        self.source = Some(source);
        self
    }

    /// 诊断源只读视图。
    pub fn source(&self) -> Option<&anyhow::Error> {
        self.source.as_ref()
    }
}

/// 执行层统一返回类型（base spec §7.3）。
#[derive(Debug)]
pub enum ToolExecutionError {
    /// 可恢复：写入 `ToolOutcome.error` 并反馈模型。
    Recoverable(ToolError),
    /// 内部 fatal：上抛 runtime（上抛前必须 seal 对应 timeline 块为 Failed）。
    Fatal(FatalToolError),
}

impl ToolExecutionError {
    /// 是否 fatal。
    pub fn is_fatal(&self) -> bool {
        matches!(self, Self::Fatal(_))
    }
}

impl From<ToolError> for ToolExecutionError {
    fn from(error: ToolError) -> Self {
        Self::Recoverable(error)
    }
}

impl From<FatalToolError> for ToolExecutionError {
    fn from(error: FatalToolError) -> Self {
        Self::Fatal(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_parse_accepts_builtin_and_namespaced_forms() {
        for raw in ["invalid_arguments", "mcp.rate_limited", "edit.hash_mismatch"] {
            assert!(ToolErrorCode::parse(raw).is_ok(), "{raw} 应合法");
        }
    }

    #[test]
    fn code_parse_rejects_malformed() {
        for raw in ["", "Invalid", "_x", "a..b", ".a", "a.", "a.b-c"] {
            assert!(ToolErrorCode::parse(raw).is_err(), "{raw} 应非法");
        }
    }

    #[test]
    fn builtin_codes_derive_from_kind() {
        let code = ToolErrorCode::builtin(ToolErrorKind::NotFound).expect("内置 code");
        assert_eq!(code.as_str(), "not_found");
        assert!(!code.is_namespaced());
        assert_eq!(ToolErrorCode::builtin(ToolErrorKind::Custom), None);
    }

    #[test]
    fn custom_requires_namespace() {
        let ok = ToolErrorCode::parse("mcp.rate_limited").expect("valid");
        assert!(ToolError::custom(ok, "限流").is_ok());

        let flat = ToolErrorCode::parse("rate_limited").expect("valid");
        assert!(
            ToolError::custom(flat, "限流").is_err(),
            "无命名空间 code 不得构造 Custom"
        );
    }

    #[test]
    fn new_derives_retryable_from_kind() {
        assert!(ToolError::timeout("t").retryable, "超时默认可重试");
        assert!(!ToolError::invalid_arguments("a").retryable);
        assert!(!ToolError::cancelled("c").retryable);
    }

    #[test]
    fn custom_kind_via_new_keeps_namespaced_fallback_code() {
        let error = ToolError::new(ToolErrorKind::Custom, "x");
        assert!(error.code.is_namespaced(), "回退 code 仍保持命名空间形态");
    }

    #[test]
    fn execution_error_distinguishes_fatal() {
        let recoverable: ToolExecutionError = ToolError::not_found("f").into();
        assert!(!recoverable.is_fatal());
        let fatal: ToolExecutionError = FatalToolError::new("registry_corrupt", "x").into();
        assert!(fatal.is_fatal());
    }
}
