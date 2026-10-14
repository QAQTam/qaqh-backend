//! 协议常量与 OpenID discovery。
//!
//! **所有外部坐标集中在本文件**：该流程仍处 preview（DevKit 把 `sendHostId`
//! 标注为「仅兼容 deployment 开启」、`force_reconsent` 待 OpenAI 确认），
//! 端点/scope/参数一旦变动，改动面必须只在这里。
//!
//! issuer 是**参数而非全局常量**：生产默认 [`ISSUER`]，但 discovery 校验、
//! authorize/token/jwks 的坐标都由调用方注入——否则回环端到端测试无法自建
//! 一个可控的 OIDC 提供方（也会逼出「测试里改环境变量」这类脏钩子）。
//!
//! 来源：`developers.openai.com/siwc/token-sharing-open-source/*`（核对于 2026-10-14）。

use serde_json::Value;

use crate::error::{SiwcCode, SiwcError};
use crate::http::Http;

/// 生产 issuer。生产路径下 discovery 与 ID token 的 `iss` 都必须等于它。
pub const ISSUER: &str = "https://auth.openai.com";
/// discovery 文档路径（相对 issuer）。
pub const DISCOVERY_PATH: &str = "/.well-known/openid-configuration";
/// OAuth `resource` 参数：套餐用量只落在这个 resource 上。
pub const RESOURCE: &str = "https://api.openai.com/v1";
/// 推理与模型目录的 API 前缀（`/v1/responses`、`/v1/models` 挂在其后）。
pub const API_BASE: &str = "https://api.openai.com/v1";
/// 请求的完整 scope 集合。套餐用量必须**整套**一起请求（缺一即无 token）。
pub const SCOPES: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
/// 判定「套餐用量已授权」的唯一 scope。
pub const SHARING_SCOPE: &str = "chatgpt.tokens.use.direct";
/// 首次注册的入口 client_id。**绝不落盘、绝不用它换码**。
pub const DYNAMIC_CLIENT_ID: &str = "dynamic_agent_client";
/// 回环回调路径。文档固化：`/callback` 不等于 `/auth/callback`，且禁用 `localhost`。
pub const CALLBACK_PATH: &str = "/auth/callback";
/// 默认回环端口（端口可变，scheme/host/path 不可变）。
pub const DEFAULT_CALLBACK_PORT: u16 = 1455;
/// 用量设置深链：UI 的 Manage usage 唯一真源。
pub const USAGE_URL: &str = "https://chatgpt.com/settings/usage";
/// 单次授权尝试的有效期（文档示例 10 分钟）。
pub const SIGN_IN_TTL_MS: u64 = 10 * 60 * 1000;

/// `GET {issuer}/.well-known/openid-configuration` 的投影。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovery {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub jwks_uri: String,
    pub revocation_endpoint: Option<String>,
}

impl Discovery {
    pub fn document_url(issuer: &str) -> String {
        format!("{issuer}{DISCOVERY_PATH}")
    }

    /// 拉取并校验 discovery（网络失败即为**可用性**故障，不是身份问题）。
    pub fn load(http: &Http, issuer: &str) -> Result<Self, SiwcError> {
        let response = http.get(&Self::document_url(issuer), None)?;
        if !response.is_success() {
            return Err(SiwcError::new(
                SiwcCode::DiscoveryUnavailable,
                format!("discovery 返回 HTTP {}", response.status),
            )
            .with_request_id(response.request_id.clone()));
        }
        Self::parse(&response.body, issuer)
    }

    /// 严格解析：`issuer` 必须精确等于期望值，且**每个 endpoint 的 origin 必须
    /// 同 issuer**——否则一份被污染的 discovery 就能把授权码与凭据送往他处。
    pub fn parse(raw: &str, expected_issuer: &str) -> Result<Self, SiwcError> {
        let value: Value = serde_json::from_str(raw).map_err(|e| {
            SiwcError::new(
                SiwcCode::DiscoveryUnavailable,
                format!("discovery JSON 非法: {e}"),
            )
        })?;
        Self::from_json(&value, expected_issuer)
    }

    pub fn from_json(value: &Value, expected_issuer: &str) -> Result<Self, SiwcError> {
        let issuer = value
            .get("issuer")
            .and_then(Value::as_str)
            .ok_or_else(|| SiwcError::new(SiwcCode::DiscoveryUnavailable, "discovery 缺 issuer"))?;
        if issuer != expected_issuer {
            return Err(SiwcError::new(
                SiwcCode::DiscoveryUnavailable,
                format!("discovery issuer 非预期: {issuer}（期望 {expected_issuer}）"),
            ));
        }
        let required = |key: &str| -> Result<String, SiwcError> {
            let raw = value.get(key).and_then(Value::as_str).ok_or_else(|| {
                SiwcError::new(SiwcCode::DiscoveryUnavailable, format!("discovery 缺 {key}"))
            })?;
            if !same_origin(raw, expected_issuer) {
                return Err(SiwcError::new(
                    SiwcCode::DiscoveryUnavailable,
                    format!("discovery {key} 越出 issuer origin: {raw}"),
                ));
            }
            Ok(raw.to_owned())
        };
        let revocation_endpoint = match value.get("revocation_endpoint") {
            Some(Value::String(raw)) => {
                if !same_origin(raw, expected_issuer) {
                    return Err(SiwcError::new(
                        SiwcCode::DiscoveryUnavailable,
                        format!("discovery revocation_endpoint 越出 issuer origin: {raw}"),
                    ));
                }
                Some(raw.clone())
            }
            _ => None,
        };
        Ok(Self {
            issuer: issuer.to_owned(),
            authorization_endpoint: required("authorization_endpoint")?,
            token_endpoint: required("token_endpoint")?,
            jwks_uri: required("jwks_uri")?,
            revocation_endpoint,
        })
    }
}

/// 粗粒度 origin 比较：scheme + host + 端口。
fn same_origin(candidate: &str, reference: &str) -> bool {
    fn origin(raw: &str) -> Option<(String, String, Option<u16>)> {
        let (scheme, rest) = raw.split_once("://")?;
        let scheme = scheme.to_ascii_lowercase();
        let authority = rest.split(['/', '?', '#']).next()?;
        if authority.is_empty() || authority.contains('@') {
            return None;
        }
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => (host, port.parse::<u16>().ok()),
            None => (authority, None),
        };
        Some((scheme, host.to_ascii_lowercase(), port))
    }
    match (origin(candidate), origin(reference)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"{
        "issuer": "https://auth.openai.com",
        "authorization_endpoint": "https://auth.openai.com/api/accounts/authorize",
        "token_endpoint": "https://auth.openai.com/api/accounts/oauth/token",
        "jwks_uri": "https://auth.openai.com/.well-known/jwks.json",
        "revocation_endpoint": "https://auth.openai.com/api/accounts/oauth/revoke"
    }"#;

    #[test]
    fn parses_production_discovery() {
        let discovery = Discovery::parse(GOOD, ISSUER).expect("good discovery");
        assert_eq!(discovery.issuer, ISSUER);
        assert_eq!(
            discovery.token_endpoint,
            "https://auth.openai.com/api/accounts/oauth/token"
        );
        assert!(discovery.revocation_endpoint.is_some());
        assert_eq!(
            Discovery::document_url(ISSUER),
            "https://auth.openai.com/.well-known/openid-configuration"
        );
    }

    #[test]
    fn rejects_foreign_issuer_or_endpoint() {
        let foreign_issuer = GOOD.replace(
            "\"issuer\": \"https://auth.openai.com\"",
            "\"issuer\": \"https://evil.example\"",
        );
        assert_eq!(
            Discovery::parse(&foreign_issuer, ISSUER)
                .expect_err("must reject")
                .code(),
            SiwcCode::DiscoveryUnavailable
        );

        // issuer 正确但 token 端点被指向他处：必须拒绝（防凭据外送）。
        let hijacked = GOOD.replace(
            "https://auth.openai.com/api/accounts/oauth/token",
            "https://evil.example/oauth/token",
        );
        assert_eq!(
            Discovery::parse(&hijacked, ISSUER)
                .expect_err("must reject")
                .code(),
            SiwcCode::DiscoveryUnavailable
        );
    }

    #[test]
    fn issuer_is_injectable_for_loopback_providers() {
        let mock = r#"{
            "issuer": "http://127.0.0.1:9/",
            "authorization_endpoint": "http://127.0.0.1:9/authorize",
            "token_endpoint": "http://127.0.0.1:9/token",
            "jwks_uri": "http://127.0.0.1:9/jwks"
        }"#;
        let discovery = Discovery::parse(mock, "http://127.0.0.1:9/").expect("mock issuer");
        assert_eq!(discovery.token_endpoint, "http://127.0.0.1:9/token");
        // 同一个文档在生产 issuer 下必须被拒。
        assert!(Discovery::parse(mock, ISSUER).is_err());
    }

    #[test]
    fn revocation_endpoint_is_optional() {
        let json = serde_json::json!({
            "issuer": ISSUER,
            "authorization_endpoint": "https://auth.openai.com/authorize",
            "token_endpoint": "https://auth.openai.com/token",
            "jwks_uri": "https://auth.openai.com/jwks"
        });
        let discovery = Discovery::from_json(&json, ISSUER).expect("optional revocation");
        assert!(discovery.revocation_endpoint.is_none());
    }
}
