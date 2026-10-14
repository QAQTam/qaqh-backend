//! 失败分类：**唯一**决定「要不要清凭据」的地方。
//!
//! 不变量 2（见 crate 文档）：端点不可用 ≠ 凭据无效。只有 HTTP 401 或平台业务码
//! `40003`（invalid authorization）算凭据失效；其余失败一律保留登录态。

use std::fmt;

/// 账号流程的失败类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DsCode {
    /// 协议/形状不符：响应解不出来、字段缺失、被重定向。
    Protocol,
    /// 可用性问题：超时、连不上、5xx 重试耗尽。**可重试**。
    Network,
    /// 授权尝试过期（`expires_in` 用完）。
    Expired,
    /// 本地取消（用户关掉等待页或显式取消）。
    Cancelled,
    /// 回环监听/回调处理失败（端口不可用、请求形状非法）。
    Callback,
    /// **凭据失效**：HTTP 401 或业务码 `40003`。唯一会清凭据的类别。
    Unauthorized,
    /// 本机还没有可用凭据（没登录）。
    ///
    /// 与 [`DsCode::Storage`] 的区别是「没登录」vs「登录过但坏了」；与
    /// [`DsCode::Unauthorized`] 的区别是**不得**把它当成「该清凭据」。
    /// 账号路由拿到它就报错，**绝不回退**到 API key。
    NotSignedIn,
    /// 其它平台业务失败（带业务码，文案由服务端给）。
    Business(i64),
    /// 本地存储失败（凭据密文读写）。
    Storage,
}

/// 平台「鉴权失效」业务码（参照实现 README：top-level response code 40003）。
pub const BIZ_INVALID_AUTHORIZATION: i64 = 40003;

/// 带类别的失败。`message` 是**已脱敏**的可展示文案（绝不含 token/verifier）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DsError {
    code: DsCode,
    message: String,
}

impl DsError {
    /// 构造。
    pub fn new(code: DsCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// 失败类别（调用方按它分支，不匹配文案）。
    pub fn code(&self) -> DsCode {
        self.code
    }

    /// 脱敏后的可展示文案。
    pub fn message(&self) -> &str {
        &self.message
    }

    /// 是否必须清掉本地凭据。
    ///
    /// **这是唯一的判据入口**——散在各处写 `matches!(code, Unauthorized)` 迟早会
    /// 漏一处，而漏掉的方向有两种（该清的没清 = 永远 401；不该清的清了 = 用户莫名
    /// 掉线），两种都难查。
    pub fn requires_reauth(&self) -> bool {
        matches!(self.code, DsCode::Unauthorized)
    }

    /// 是否值得原样重试（可用性问题）。
    pub fn is_retryable(&self) -> bool {
        matches!(self.code, DsCode::Network)
    }
}

impl fmt::Display for DsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.code {
            DsCode::Business(code) => write!(f, "平台业务失败 {code}: {}", self.message),
            other => write!(f, "{other:?}: {}", self.message),
        }
    }
}

impl std::error::Error for DsError {}

/// HTTP 状态码 → 失败类别。
///
/// 与参照实现的取舍不同（它把 401 之外的一切非 2xx 都当网络故障）：`4xx` 是
/// **请求形状被拒**（要改代码或改配置），`5xx` 是**可用性**（可重试），两者处置
/// 不同，混淆会让「改配置就能好」的问题表现成「网络抖动」。
pub fn classify_http_status(status: u16) -> DsCode {
    match status {
        401 => DsCode::Unauthorized,
        500..=599 => DsCode::Network,
        400..=499 => DsCode::Protocol,
        _ => DsCode::Protocol,
    }
}

/// 平台业务码 → 失败类别（`code` 与 `biz_code` 两个位置共用这一条判据）。
pub fn classify_biz_code(code: i64) -> DsCode {
    if code == BIZ_INVALID_AUTHORIZATION {
        DsCode::Unauthorized
    } else {
        DsCode::Business(code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_unauthorized_requires_clearing_the_credential() {
        for code in [
            DsCode::Protocol,
            DsCode::Network,
            DsCode::Expired,
            DsCode::Cancelled,
            DsCode::Callback,
            DsCode::NotSignedIn,
            DsCode::Business(10001),
            DsCode::Storage,
        ] {
            let error = DsError::new(code, "x");
            assert!(!error.requires_reauth(), "{code:?} 不该清凭据");
        }
        assert!(DsError::new(DsCode::Unauthorized, "x").requires_reauth());
    }

    #[test]
    fn http_status_splits_request_shape_from_availability() {
        assert_eq!(classify_http_status(401), DsCode::Unauthorized);
        assert_eq!(classify_http_status(403), DsCode::Protocol);
        assert_eq!(classify_http_status(429), DsCode::Protocol);
        assert_eq!(classify_http_status(500), DsCode::Network);
        assert_eq!(classify_http_status(503), DsCode::Network);
        // 3xx 不该出现在这里（调用方一律 `redirect: error`），出现即协议异常。
        assert_eq!(classify_http_status(302), DsCode::Protocol);
    }

    #[test]
    fn the_invalid_authorization_business_code_is_the_only_reauth_business_code() {
        assert_eq!(classify_biz_code(40003), DsCode::Unauthorized);
        assert!(DsError::new(classify_biz_code(40003), "invalid authorization").requires_reauth());
        assert_eq!(classify_biz_code(0), DsCode::Business(0));
        assert_eq!(classify_biz_code(10001), DsCode::Business(10001));
        assert!(!DsError::new(classify_biz_code(10001), "x").requires_reauth());
    }

    #[test]
    fn retryable_is_only_the_availability_class() {
        assert!(DsError::new(DsCode::Network, "timeout").is_retryable());
        assert!(!DsError::new(DsCode::Unauthorized, "401").is_retryable());
        assert!(!DsError::new(DsCode::Business(1), "biz").is_retryable());
    }
}
