//! 错误面：**机器可读 code + 可重试位 + 是否需重登**，对齐
//! `developers.openai.com/siwc/token-sharing-open-source/errors-and-recovery`。
//!
//! M0 的三条硬约束（都落在类型上，而不是纪律上）：
//!
//! 1. **「端点不可用」≠「凭据无效」**。JWKS/token 端点 5xx、网络中断、限流
//!    都**不得**清凭据（[`SiwcCode::retryable`]）。
//! 2. **`subscription_sharing_*` / `chatpass_v2_*` 是产品状态**，不是传输错误：
//!    限流要让 UI 去 `chatgpt.com/settings/usage`，`unsupported_capability`
//!    基本等价于「我方发错了字段」（见 `responses_sdk` 的 plan 兼容位）。
//! 3. **`invalid_grant` 家族才清凭据**（[`SiwcCode::requires_reauth`]）。

use serde_json::{Value, json};

/// 机器可读错误码。字符串形态与文档措辞一致，便于日志与跨端对照。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
#[serde(rename_all = "snake_case")]
pub enum SiwcCode {
    // ── 协议前 / 本地 ──
    /// discovery 拉取失败或校验不通过（issuer/origin 不符）。
    DiscoveryUnavailable,
    /// JWKS/密钥服务不可用：**身份未判定**，必须保连接。
    IdentityVerificationUnavailable,
    /// ID token 本身不可信（签名/iss/aud/nonce/exp）。
    InvalidIdToken,
    /// 新注册回调没带回下发的 `client_id`（或带回了 `dynamic_agent_client`）。
    RegistrationIncomplete,
    /// 回访返回的账户与选中 profile 的 `sub` 不一致。
    AccountMismatch,
    /// token 响应缺字段/形状不符。
    InvalidTokenResponse,
    /// 换码或刷新被拒：凭据不可用，需用已下发 client_id 重走 OAuth。
    InvalidGrant,
    /// client 配置错（client_id 失效等）。
    InvalidClient,
    /// 回环端口不可用。
    CallbackPortUnavailable,
    /// 回环监听器异常终止。
    CallbackFailed,
    /// 用户取消 / 超时。
    SignInCancelled,
    /// 未登录或无可用的 access token。
    SignInRequired,
    /// 距上次刷新太近（`earliest_refresh_at` 未到）或刚恢复，稍后重试即可。
    RefreshNotReady,
    /// 已登录但未授权套餐用量（缺 `chatgpt.tokens.use.direct`）。
    SharingNotEnabled,
    /// 已有一次登录/切档在进行中。
    ConnectionBusy,
    /// 指定 profile 不存在。
    ProfileNotFound,
    /// 凭据落盘失败。
    StoreFailure,
    /// 入参非法（label、端口、profileId 组合等）。
    InvalidConfig,
    /// 传输层失败（网络、5xx）：保凭据、退避重试。
    #[serde(rename = "transport_error")]
    Transport,

    // ── 上游产品状态（`error.code`）──
    /// `subscription_sharing_usage_limit_exceeded`（429）。
    #[serde(rename = "subscription_sharing_usage_limit_exceeded")]
    UsageLimitExceeded,
    /// `subscription_sharing_user_not_eligible`（403）。
    #[serde(rename = "subscription_sharing_user_not_eligible")]
    UserNotEligible,
    /// `subscription_sharing_usage_unavailable` / `subscription_sharing_user_unavailable`（503）。
    #[serde(rename = "subscription_sharing_usage_unavailable")]
    UsageUnavailable,
    /// `subscription_sharing_unsupported_capability`（400）：我方发错字段/工具。
    #[serde(rename = "subscription_sharing_unsupported_capability")]
    UnsupportedCapability,
    /// `subscription_sharing_route_not_supported`（403）：方法/端点不对。
    #[serde(rename = "subscription_sharing_route_not_supported")]
    RouteNotSupported,
    /// `subscription_sharing_invalid_user`（401）。
    #[serde(rename = "subscription_sharing_invalid_user")]
    InvalidUser,
    /// `chatpass_v2_scope_not_authorized` / `chatpass_v2_invalid_authorization_context`（403）。
    #[serde(rename = "chatpass_v2_scope_not_authorized")]
    ScopeNotAuthorized,
    /// 其它 Responses 层错误（保留原始 code 于 message/request_id）。
    ProviderError,
}

impl SiwcCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DiscoveryUnavailable => "discovery_unavailable",
            Self::IdentityVerificationUnavailable => "identity_verification_unavailable",
            Self::InvalidIdToken => "invalid_id_token",
            Self::RegistrationIncomplete => "registration_incomplete",
            Self::AccountMismatch => "account_mismatch",
            Self::InvalidTokenResponse => "invalid_token_response",
            Self::InvalidGrant => "invalid_grant",
            Self::InvalidClient => "invalid_client",
            Self::CallbackPortUnavailable => "callback_port_unavailable",
            Self::CallbackFailed => "callback_failed",
            Self::SignInCancelled => "sign_in_cancelled",
            Self::SignInRequired => "sign_in_required",
            Self::RefreshNotReady => "refresh_not_ready",
            Self::SharingNotEnabled => "sharing_not_enabled",
            Self::ConnectionBusy => "connection_busy",
            Self::ProfileNotFound => "profile_not_found",
            Self::StoreFailure => "store_failure",
            Self::InvalidConfig => "invalid_config",
            Self::Transport => "transport_error",
            Self::UsageLimitExceeded => "subscription_sharing_usage_limit_exceeded",
            Self::UserNotEligible => "subscription_sharing_user_not_eligible",
            Self::UsageUnavailable => "subscription_sharing_usage_unavailable",
            Self::UnsupportedCapability => "subscription_sharing_unsupported_capability",
            Self::RouteNotSupported => "subscription_sharing_route_not_supported",
            Self::InvalidUser => "subscription_sharing_invalid_user",
            Self::ScopeNotAuthorized => "chatpass_v2_scope_not_authorized",
            Self::ProviderError => "provider_error",
        }
    }

    /// 可重试：**保留凭据**，退避后重试。传输失败、上游暂不可用、刷新窗口未到。
    pub fn retryable(self) -> bool {
        matches!(
            self,
            Self::Transport
                | Self::UsageUnavailable
                | Self::RefreshNotReady
                | Self::CallbackPortUnavailable
        )
    }

    /// 该错误意味着「本 profile 的凭据已不可用」→ 清 token、保留注册、要求重登。
    ///
    /// 刻意**不含** [`Self::UsageUnavailable`] / [`Self::Transport`]：那是暂时故障。
    pub fn requires_reauth(self) -> bool {
        matches!(
            self,
            Self::InvalidGrant | Self::InvalidClient | Self::InvalidUser | Self::AccountMismatch
        )
    }

    /// 需要用户去 ChatGPT 设置处理（限流 / 不适用 / 授权上下文）。
    pub fn needs_user_action(self) -> bool {
        matches!(
            self,
            Self::UsageLimitExceeded | Self::UserNotEligible | Self::ScopeNotAuthorized
        )
    }
}

impl std::fmt::Display for SiwcCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 带 code 与 request id 的错误（request id 供支持排障，文档点名要保留）。
#[derive(Debug, Clone)]
pub struct SiwcError {
    code: SiwcCode,
    message: String,
    request_id: Option<String>,
}

impl SiwcError {
    pub fn new(code: SiwcCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            request_id: None,
        }
    }

    pub fn with_request_id(mut self, request_id: Option<String>) -> Self {
        self.request_id = request_id;
        self
    }

    pub fn code(&self) -> SiwcCode {
        self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn request_id(&self) -> Option<&str> {
        self.request_id.as_deref()
    }

    /// 服务面响应形状。**不含任何 token 字段**（结构上就无从泄漏）。
    pub fn to_json(&self) -> Value {
        let mut value = json!({
            "code": self.code.as_str(),
            "message": self.message,
            "retryable": self.code.retryable(),
            "requiresReauth": self.code.requires_reauth(),
        });
        if let Some(request_id) = &self.request_id
            && let Some(map) = value.as_object_mut()
        {
            map.insert("requestId".into(), json!(request_id));
        }
        value
    }
}

impl std::fmt::Display for SiwcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for SiwcError {}

/// 服务面 `dispatch` 的错误通道是 `String`：只出 `code: message`，不出结构体。
impl From<SiwcError> for String {
    fn from(error: SiwcError) -> Self {
        format!("{}: {}", error.code.as_str(), error.message)
    }
}

/// 服务面 / 上游错误 → 语义档位。
///
/// `status` 为 HTTP 状态；`code` 为 `error.code`（若有）。**上下文不可推断时
/// 一律给可重试档**——文档明确「网络或基础设施的临时故障不得删除凭据」。
pub fn classify_provider_error(status: u16, code: Option<&str>) -> SiwcCode {
    match code {
        Some("subscription_sharing_usage_limit_exceeded") => SiwcCode::UsageLimitExceeded,
        Some("subscription_sharing_user_not_eligible") => SiwcCode::UserNotEligible,
        Some("subscription_sharing_usage_unavailable") => SiwcCode::UsageUnavailable,
        Some("subscription_sharing_user_unavailable") => SiwcCode::UsageUnavailable,
        Some("subscription_sharing_unsupported_capability") => SiwcCode::UnsupportedCapability,
        Some("subscription_sharing_route_not_supported") => SiwcCode::RouteNotSupported,
        Some("subscription_sharing_invalid_user") => SiwcCode::InvalidUser,
        Some("chatpass_v2_scope_not_authorized") => SiwcCode::ScopeNotAuthorized,
        Some("chatpass_v2_invalid_authorization_context") => SiwcCode::ScopeNotAuthorized,
        Some("invalid_grant") => SiwcCode::InvalidGrant,
        Some("invalid_refresh_token") | Some("token_expired") | Some("refresh_token_expired") => {
            SiwcCode::InvalidGrant
        }
        Some("refresh_token_invalidated") | Some("refresh_token_reused") => SiwcCode::InvalidGrant,
        Some("invalid_client") => SiwcCode::InvalidClient,
        _ => match status {
            401 => SiwcCode::InvalidUser,
            403 => SiwcCode::UserNotEligible,
            429 => SiwcCode::UsageLimitExceeded,
            500..=599 => SiwcCode::Transport,
            _ => SiwcCode::ProviderError,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_token_family_requires_reauth_but_5xx_does_not() {
        for code in [
            "invalid_grant",
            "invalid_refresh_token",
            "token_expired",
            "refresh_token_expired",
            "refresh_token_invalidated",
            "refresh_token_reused",
        ] {
            let classified = classify_provider_error(400, Some(code));
            assert_eq!(classified, SiwcCode::InvalidGrant, "{code}");
            assert!(classified.requires_reauth(), "{code} 必须要求重登");
            assert!(!classified.retryable(), "{code} 不该无限重试");
        }
        let outage = classify_provider_error(503, None);
        assert_eq!(outage, SiwcCode::Transport);
        assert!(outage.retryable());
        assert!(!outage.requires_reauth(), "5xx 绝不清凭据");
    }

    #[test]
    fn plan_usage_codes_map_to_user_facing_states() {
        assert_eq!(
            classify_provider_error(429, Some("subscription_sharing_usage_limit_exceeded")),
            SiwcCode::UsageLimitExceeded
        );
        assert!(
            classify_provider_error(429, Some("subscription_sharing_usage_limit_exceeded"))
                .needs_user_action()
        );
        assert_eq!(
            classify_provider_error(400, Some("subscription_sharing_unsupported_capability")),
            SiwcCode::UnsupportedCapability
        );
        assert!(
            !classify_provider_error(400, Some("subscription_sharing_unsupported_capability"))
                .retryable(),
            "发错字段重试同体没有意义"
        );
        assert_eq!(
            classify_provider_error(403, Some("chatpass_v2_invalid_authorization_context")),
            SiwcCode::ScopeNotAuthorized
        );
    }

    #[test]
    fn error_json_never_carries_token_fields() {
        let value = SiwcError::new(SiwcCode::UsageLimitExceeded, "limit reached")
            .with_request_id(Some("req_123".to_string()))
            .to_json();
        let text = value.to_string();
        for banned in ["access_token", "refresh_token", "id_token", "token\":"] {
            assert!(!text.contains(banned), "错误载荷出现 {banned}: {text}");
        }
        assert_eq!(value["code"], "subscription_sharing_usage_limit_exceeded");
        assert_eq!(value["requestId"], "req_123");
    }
}
