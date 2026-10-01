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
    /// 工具特有错误（code 建议带工具前缀，且不得与内置码重名）。
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

    /// 全部内置 code（供 `Custom` 构造查重）。
    pub fn iter_builtin_codes() -> impl Iterator<Item = &'static str> {
        [
            Self::InvalidArguments,
            Self::NotFound,
            Self::Conflict,
            Self::PermissionDenied,
            Self::Unauthorized,
            Self::Timeout,
            Self::Cancelled,
            Self::Network,
            Self::Execution,
            Self::Unavailable,
        ]
        .into_iter()
        .filter_map(Self::builtin_code)
    }

    /// 默认可重试性（base spec §6.4：超时/网络/暂不可用可重试）。
    pub fn default_retryable(self) -> bool {
        matches!(self, Self::Timeout | Self::Network | Self::Unavailable)
    }
}

/// 稳定、机器可读的错误 code。
///
/// 形态：`^[a-z][a-z0-9_]*$`（与 canonical fact v2 的 `error.code` 校验完全一致）。
/// 工具命名空间用下划线前缀约定表达，例如 `mcp_rate_limited`、`edit_hash_mismatch`。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ToolErrorCode(String);

/// code 构造错误（不 panic）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolErrorCodeError {
    /// 具体原因（静态文案）。
    pub reason: &'static str,
}

impl ToolErrorCode {
    /// 解析并校验 code（`^[a-z][a-z0-9_]*$`）。
    pub fn parse(raw: &str) -> Result<Self, ToolErrorCodeError> {
        if raw.is_empty() {
            return Err(ToolErrorCodeError {
                reason: "code 不得为空",
            });
        }
        let mut chars = raw.chars();
        match chars.next() {
            Some(first) if first.is_ascii_lowercase() => {}
            _ => {
                return Err(ToolErrorCodeError {
                    reason: "code 首字符必须为小写字母",
                });
            }
        }
        for c in chars {
            if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
                return Err(ToolErrorCodeError {
                    reason: "code 仅允许 [a-z0-9_]",
                });
            }
        }
        Ok(Self(raw.to_owned()))
    }

    /// 由内置 kind 构造（code 由 kind 推导；`Custom` 返回 `None`）。
    pub fn builtin(kind: ToolErrorKind) -> Option<Self> {
        kind.builtin_code().map(|code| Self(code.to_owned()))
    }

    /// 保留具体错误码：`raw` 合法则原样采用，否则回退 kind 内置码。
    ///
    /// 错误码的唯一规范形态就是 [`Self::parse`] 接受的 snake_case——调用方
    /// 传入的码可能来自旧持久化数据或动态拼接，非法时不得透传（会破坏
    /// canonical fact 追加），静默落到 kind 内置码即可。
    pub fn parse_or_builtin(raw: &str, fallback_kind: ToolErrorKind) -> Self {
        Self::parse(raw).unwrap_or_else(|_| {
            Self::builtin(fallback_kind).unwrap_or_else(|| Self("tool_error".to_owned()))
        })
    }

    /// 字符串视图。
    pub fn as_str(&self) -> &str {
        &self.0
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
    /// 稳定 code（内置 kind 由 kind 推导；`Custom` 建议带工具前缀）。
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
    /// code 回退为 `custom_unspecified`（保持"Custom 不得冒用内置码"不变量）。
    pub fn new(kind: ToolErrorKind, detail: impl Into<String>) -> Self {
        let code = ToolErrorCode::builtin(kind)
            .unwrap_or_else(|| ToolErrorCode("custom_unspecified".to_owned()));
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

    /// 构造 `Custom` 错误（code 不得与内置码重名，建议带工具前缀，如
    /// `mcp_rate_limited`）。
    pub fn custom(
        code: ToolErrorCode,
        detail: impl Into<String>,
    ) -> Result<Self, ToolErrorCodeError> {
        if ToolErrorKind::iter_builtin_codes().any(|builtin| builtin == code.0) {
            return Err(ToolErrorCodeError {
                reason: "Custom code 不得与内置码重名（建议带工具前缀，如 mcp_rate_limited）",
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
    fn code_parse_accepts_snake_case() {
        for raw in [
            "invalid_arguments",
            "mcp_rate_limited",
            "edit_hash_mismatch",
        ] {
            assert!(ToolErrorCode::parse(raw).is_ok(), "{raw} 应合法");
        }
    }

    #[test]
    fn code_parse_rejects_malformed() {
        for raw in [
            "",
            "Invalid",
            "TOOL_ERROR",
            "_x",
            "mcp.rate_limited",
            "a b",
            "全角",
        ] {
            assert!(ToolErrorCode::parse(raw).is_err(), "{raw} 应非法");
        }
    }

    #[test]
    fn builtin_codes_derive_from_kind() {
        let code = ToolErrorCode::builtin(ToolErrorKind::NotFound).expect("内置 code");
        assert_eq!(code.as_str(), "not_found");
        assert_eq!(ToolErrorCode::builtin(ToolErrorKind::Custom), None);
    }

    #[test]
    fn custom_rejects_builtin_collision() {
        let ok = ToolErrorCode::parse("mcp_rate_limited").expect("valid");
        assert!(ToolError::custom(ok, "限流").is_ok());

        let builtin = ToolErrorCode::parse("not_found").expect("valid");
        assert!(
            ToolError::custom(builtin, "x").is_err(),
            "Custom code 不得与内置码重名"
        );
    }

    #[test]
    fn parse_or_builtin_keeps_conforming_and_falls_back() {
        let kept = ToolErrorCode::parse_or_builtin("edit_hash_mismatch", ToolErrorKind::Execution);
        assert_eq!(kept.as_str(), "edit_hash_mismatch");
        let legacy = ToolErrorCode::parse_or_builtin("TOOL_ERROR", ToolErrorKind::Execution);
        assert_eq!(legacy.as_str(), "execution", "非法码回退 kind 内置码");
        let custom_fallback = ToolErrorCode::parse_or_builtin("", ToolErrorKind::Custom);
        assert_eq!(
            custom_fallback.as_str(),
            "tool_error",
            "Custom 无内置码时兜底"
        );
    }

    #[test]
    fn new_derives_retryable_from_kind() {
        assert!(ToolError::timeout("t").retryable, "超时默认可重试");
        assert!(!ToolError::invalid_arguments("a").retryable);
        assert!(!ToolError::cancelled("c").retryable);
    }

    #[test]
    fn custom_kind_via_new_keeps_conforming_fallback_code() {
        let error = ToolError::new(ToolErrorKind::Custom, "x");
        assert_eq!(
            error.code.as_str(),
            "custom_unspecified",
            "回退 code 仍为合法形态"
        );
    }

    #[test]
    fn execution_error_distinguishes_fatal() {
        let recoverable: ToolExecutionError = ToolError::not_found("f").into();
        assert!(!recoverable.is_fatal());
        let fatal: ToolExecutionError = FatalToolError::new("registry_corrupt", "x").into();
        assert!(fatal.is_fatal());
    }
}
