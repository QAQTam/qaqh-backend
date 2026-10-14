//! OAuth 面：授权 URL 构造、换码、刷新、撤销。
//!
//! 三个「必须逐字一致」的位置由类型固定下来：authorize 的 `redirect_uri`、
//! token 交换的 `redirect_uri`、以及 PKCE `code_verifier` 的归属尝试。
//!
//! 刷新是**旋转式**的：每次成功刷新返回新的 refresh token（30 天重置）。
//! 因此这里的 `refresh` 只负责「发请求 + 校验响应」，**落盘检查点与单飞串行化
//! 在 [`crate::manager`]**——否则一次取消就可能丢弃唯一的后继 token。

use serde_json::Value;

use crate::error::{SiwcCode, SiwcError};
use crate::http::Http;
use crate::util::base64url_encode;

/// `code_challenge = base64url(SHA-256(verifier))`（RFC 7636 §4.2，S256）。
pub fn pkce_challenge(verifier: &str) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, verifier.as_bytes());
    base64url_encode(digest.as_ref())
}

/// authorize 请求参数。
#[derive(Debug, Clone)]
pub struct AuthorizeParams<'a> {
    /// 首次注册用 [`crate::protocol::DYNAMIC_CLIENT_ID`]；回访用**下发的** client_id。
    pub client_id: &'a str,
    pub redirect_uri: &'a str,
    pub state: &'a str,
    pub nonce: &'a str,
    pub code_challenge: &'a str,
    /// host 标识（必带）。
    pub host_id: &'a str,
    /// 应用真名（**仅首次注册带**）。
    pub agent_name_hint: Option<&'a str>,
    /// 回访的可选 hint（上次成功的 id_token）。
    pub id_token_hint: Option<&'a str>,
    /// 回访的可选邮箱 hint。
    pub login_hint: Option<&'a str>,
    /// 仅当用户主动去设置里开启套餐用量时置真（`prompt=consent`）。
    pub reconsent: bool,
    pub scope: &'a str,
    pub resource: &'a str,
}

/// 构造授权 URL（system browser 打开；**不**在内嵌 webview 里跑）。
pub fn authorize_url(
    authorization_endpoint: &str,
    params: &AuthorizeParams<'_>,
) -> Result<String, SiwcError> {
    let mut url = url::Url::parse(authorization_endpoint).map_err(|e| {
        SiwcError::new(
            SiwcCode::DiscoveryUnavailable,
            format!("authorization_endpoint 非法: {e}"),
        )
    })?;
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("client_id", params.client_id)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", params.redirect_uri)
            .append_pair("scope", params.scope)
            .append_pair("resource", params.resource)
            .append_pair("state", params.state)
            .append_pair("nonce", params.nonce)
            .append_pair("code_challenge_method", "S256")
            .append_pair("code_challenge", params.code_challenge)
            .append_pair("ext_agent_host_id", params.host_id);
        if let Some(hint) = params.agent_name_hint {
            query.append_pair("agent_name_hint", hint);
        }
        if let Some(hint) = params.id_token_hint {
            query.append_pair("id_token_hint", hint);
        }
        if let Some(hint) = params.login_hint {
            query.append_pair("login_hint", hint);
        }
        if params.reconsent {
            query.append_pair("prompt", "consent");
        }
    }
    Ok(url.into())
}

/// token 端点响应（换码与刷新同形状）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenResponse {
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    pub id_token: Option<String>,
    pub expires_in_secs: Option<u64>,
    pub earliest_refresh_at_ms: Option<u64>,
    /// **实际授予**的 scope（不是我们请求的）。
    pub scopes: Vec<String>,
}

impl TokenResponse {
    /// 严格解析：`scope` 必须存在；有 access_token 就必须伴 `token_type=bearer`
    /// 与正的 `expires_in`；请求了 `offline_access` 却没有 refresh_token 视为不完整。
    /// （与 DevKit `tokenFields` 的判定同形。）
    pub fn from_json(value: &Value) -> Result<Self, SiwcError> {
        let invalid = |message: String| SiwcError::new(SiwcCode::InvalidTokenResponse, message);
        let scope = value
            .get("scope")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("token 响应缺 scope：无法确认实际授予的权限".into()))?;
        let scopes: Vec<String> = scope
            .split_whitespace()
            .map(str::to_owned)
            .collect();

        let access_token = value
            .get("access_token")
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
            .map(str::to_owned);
        let refresh_token = value
            .get("refresh_token")
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
            .map(str::to_owned);
        let id_token = value
            .get("id_token")
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
            .map(str::to_owned);
        let expires_in_secs = value.get("expires_in").and_then(Value::as_u64);
        let earliest_refresh_at_ms = value.get("earliest_refresh_at").and_then(|raw| match raw {
            Value::Number(number) => number.as_u64().map(|secs| secs.saturating_mul(1000)),
            Value::String(text) => parse_rfc3339_ms(text),
            _ => None,
        });

        let expects_refresh = scopes.iter().any(|s| s == "offline_access");
        match &access_token {
            Some(_) => {
                let token_type_ok = value
                    .get("token_type")
                    .and_then(Value::as_str)
                    .map(|t| t.eq_ignore_ascii_case("bearer"))
                    .unwrap_or(false);
                if !token_type_ok {
                    return Err(invalid("token 响应 token_type 非 bearer".into()));
                }
                match expires_in_secs {
                    Some(secs) if secs > 0 => {}
                    _ => return Err(invalid("token 响应缺有效的 expires_in".into())),
                }
                if expects_refresh && refresh_token.is_none() {
                    return Err(invalid("授予了 offline_access 但未返回 refresh_token".into()));
                }
            }
            None => {
                if expects_refresh || scopes.iter().any(|s| s == crate::protocol::SHARING_SCOPE) {
                    return Err(invalid(
                        "token 响应未返回所请求的凭据（缺 access_token）".into(),
                    ));
                }
            }
        }

        Ok(Self {
            access_token,
            refresh_token,
            id_token,
            expires_in_secs,
            earliest_refresh_at_ms,
            scopes,
        })
    }
}

/// `2026-10-14T00:00:00Z` → epoch ms（不引时间库；只解析文档字段形态）。
fn parse_rfc3339_ms(text: &str) -> Option<u64> {
    // 期望 `YYYY-MM-DDTHH:MM:SS(.fff)?Z`
    let (date, rest) = text.split_once('T')?;
    let mut date_parts = date.split('-');
    let year: i64 = date_parts.next()?.parse().ok()?;
    let month: i64 = date_parts.next()?.parse().ok()?;
    let day: i64 = date_parts.next()?.parse().ok()?;
    let time = rest.trim_end_matches('Z');
    let time = time.split('.').next()?;
    let mut time_parts = time.split(':');
    let hour: i64 = time_parts.next()?.parse().ok()?;
    let minute: i64 = time_parts.next()?.parse().ok()?;
    let second: i64 = time_parts.next()?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let secs = days as u64 * 86_400 + (hour.max(0) as u64) * 3_600 + (minute.max(0) as u64) * 60 + second.max(0) as u64;
    Some(secs.saturating_mul(1000))
}

/// Howard Hinnant 的 civil→days 算法（UTC，1970-01-01 为 0）。
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `POST token_endpoint`，`grant_type=authorization_code`。
/// **无 client secret**（公开客户端）。
pub fn exchange_code(
    http: &Http,
    token_endpoint: &str,
    client_id: &str,
    code: &str,
    code_verifier: &str,
    redirect_uri: &str,
    resource: &str,
) -> Result<TokenResponse, SiwcError> {
    let response = http.post_form(
        token_endpoint,
        &[
            ("grant_type", "authorization_code"),
            ("client_id", client_id),
            ("code", code),
            ("code_verifier", code_verifier),
            ("redirect_uri", redirect_uri),
            ("resource", resource),
        ],
    )?;
    token_response_from(response)
}

/// `POST token_endpoint`，`grant_type=refresh_token`。
/// 省略 `scope` 以保留原授权。
pub fn refresh(
    http: &Http,
    token_endpoint: &str,
    client_id: &str,
    refresh_token: &str,
    resource: &str,
) -> Result<TokenResponse, SiwcError> {
    let response = http.post_form(
        token_endpoint,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", client_id),
            ("refresh_token", refresh_token),
            ("resource", resource),
        ],
    )?;
    token_response_from(response)
}

fn token_response_from(response: crate::http::HttpResponse) -> Result<TokenResponse, SiwcError> {
    if !response.is_success() {
        let code = crate::error::classify_provider_error(response.status, response.error_code().as_deref());
        return Err(SiwcError::new(
            code,
            format!("token 端点 HTTP {}: {}", response.status, response.error_message()),
        )
        .with_request_id(response.request_id.clone()));
    }
    let value = response.json()?;
    TokenResponse::from_json(&value).map_err(|e| e.with_request_id(response.request_id.clone()))
}

/// `POST revocation_endpoint`：撤销可再生会话（登出时先撤销再清本地）。
///
/// 空 200 即成功（含已失效 token）；5xx 是可重试的传输故障。
pub fn revoke(
    http: &Http,
    revocation_endpoint: &str,
    client_id: &str,
    refresh_token: &str,
) -> Result<(), SiwcError> {
    let response = http.post_form(
        revocation_endpoint,
        &[
            ("token", refresh_token),
            ("token_type_hint", "refresh_token"),
            ("client_id", client_id),
        ],
    )?;
    if response.is_success() {
        return Ok(());
    }
    let code = crate::error::classify_provider_error(response.status, response.error_code().as_deref());
    Err(
        SiwcError::new(code, format!("撤销会话 HTTP {}", response.status))
            .with_request_id(response.request_id.clone()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_matches_rfc7636_appendix_b() {
        // RFC 7636 Appendix B 的官方示例向量。
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(pkce_challenge(verifier), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn authorize_url_carries_every_documented_parameter() {
        let params = AuthorizeParams {
            client_id: crate::protocol::DYNAMIC_CLIENT_ID,
            redirect_uri: "http://127.0.0.1:1455/auth/callback",
            state: "st",
            nonce: "no",
            code_challenge: "cc",
            host_id: "urn:uuid:0191f0a0-1111-4222-8333-444455556666",
            agent_name_hint: Some("QAQ-Harness"),
            id_token_hint: None,
            login_hint: None,
            reconsent: false,
            scope: crate::protocol::SCOPES,
            resource: crate::protocol::RESOURCE,
        };
        let url = authorize_url("https://auth.openai.com/api/accounts/authorize", &params)
            .expect("authorize url");
        let parsed = url::Url::parse(&url).expect("parse");
        let pairs: std::collections::HashMap<String, String> = parsed
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert_eq!(pairs["client_id"], "dynamic_agent_client");
        assert_eq!(pairs["response_type"], "code");
        assert_eq!(pairs["code_challenge_method"], "S256");
        assert_eq!(pairs["redirect_uri"], "http://127.0.0.1:1455/auth/callback");
        assert_eq!(pairs["resource"], "https://api.openai.com/v1");
        assert_eq!(pairs["agent_name_hint"], "QAQ-Harness");
        assert_eq!(pairs["ext_agent_host_id"], "urn:uuid:0191f0a0-1111-4222-8333-444455556666");
        assert!(pairs["scope"].contains("chatgpt.tokens.use.direct"));
        assert!(!pairs.contains_key("prompt"), "普通登录不得强制同意");
    }

    #[test]
    fn reauthorization_omits_agent_name_hint_and_may_pass_hints() {
        let params = AuthorizeParams {
            client_id: "oaiapp_saved",
            redirect_uri: "http://127.0.0.1:1455/auth/callback",
            state: "st",
            nonce: "no",
            code_challenge: "cc",
            host_id: "urn:uuid:x",
            agent_name_hint: None,
            id_token_hint: Some("hint.jwt.value"),
            login_hint: Some("user@example.com"),
            reconsent: true,
            scope: crate::protocol::SCOPES,
            resource: crate::protocol::RESOURCE,
        };
        let url = authorize_url("https://auth.openai.com/api/accounts/authorize", &params)
            .expect("authorize url");
        assert!(url.contains("id_token_hint=hint.jwt.value"));
        assert!(url.contains("login_hint=user%40example.com"), "{url}");
        assert!(url.contains("prompt=consent"));
        assert!(!url.contains("agent_name_hint"));
    }

    #[test]
    fn token_response_validation_matches_documented_shape() {
        let good = serde_json::json!({
            "access_token": "at",
            "refresh_token": "rt",
            "id_token": "it",
            "token_type": "Bearer",
            "expires_in": 3600,
            "scope": "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct",
            "earliest_refresh_at": "2026-10-14T00:00:00Z"
        });
        let parsed = TokenResponse::from_json(&good).expect("good");
        assert!(
            parsed
                .scopes
                .iter()
                .any(|s| s == crate::protocol::SHARING_SCOPE)
        );
        assert_eq!(parsed.expires_in_secs, Some(3600));
        assert!(parsed.earliest_refresh_at_ms.is_some());

        // 缺 scope：无法确认授权面 → 不完整。
        let mut no_scope = good.clone();
        no_scope.as_object_mut().expect("obj").remove("scope");
        assert_eq!(
            TokenResponse::from_json(&no_scope).expect_err("no scope").code(),
            SiwcCode::InvalidTokenResponse
        );

        // offline_access 已授予却无 refresh_token → 不完整。
        let mut no_refresh = good.clone();
        no_refresh.as_object_mut().expect("obj").remove("refresh_token");
        assert_eq!(
            TokenResponse::from_json(&no_refresh).expect_err("no refresh").code(),
            SiwcCode::InvalidTokenResponse
        );

        // token_type 非 bearer → 不完整。
        let mut bad_type = good.clone();
        bad_type["token_type"] = serde_json::json!("mac");
        assert!(TokenResponse::from_json(&bad_type).is_err());

        // 有 offline_access 却完全没给凭据 → 不完整。
        let empty = serde_json::json!({
            "token_type": "Bearer",
            "expires_in": 3600,
            "scope": "openid offline_access"
        });
        assert!(TokenResponse::from_json(&empty).is_err());
    }

    #[test]
    fn rfc3339_parser_handles_documented_forms() {
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339_ms("1970-01-02T00:00:00Z"), Some(86_400_000));
        assert_eq!(
            parse_rfc3339_ms("2026-10-14T00:00:00.000Z"),
            parse_rfc3339_ms("2026-10-14T00:00:00Z")
        );
        assert!(parse_rfc3339_ms("not-a-time").is_none());
    }
}
