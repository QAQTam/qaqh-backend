//! id_token 的账号面 claim 解析（对标 codex `codex-rs/login/src/token_data.rs:71-99`）。
//!
//! OpenAI 的 id_token 是 JWT：payload 的 `https://api.openai.com/auth` claim
//! 携带套餐账号信息。多账号 / 工作区场景下，推理请求需要带
//! `ChatGPT-Account-ID` 头（值即 `chatgpt_account_id`）服务端才能路由到
//! id_token 指向的账号；FedRAMP 工作区另发 `X-OpenAI-Fedramp: true`。
//!
//! 本模块是**纯函数、无 IO**：解析失败一律返回 `None`，调用方把账号头当
//! 增强项处理（不下发），绝不让推理请求因此失败。

use serde_json::Value;

/// id_token 里对推理请求有用的账号面事实。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AccountInfo {
    /// `https://api.openai.com/auth`.`chatgpt_account_id`。
    pub account_id: Option<String>,
    /// `https://api.openai.com/auth`.`chatgpt_account_is_fedramp`。
    pub is_fedramp: bool,
    /// `https://api.openai.com/auth`.`chatgpt_plan_type`（free/plus/pro/team/…）。
    pub plan_type: Option<String>,
}

const AUTH_CLAIM: &str = "https://api.openai.com/auth";

/// 解析 id_token（JWT）的账号面 claim。
///
/// 非 JWT 形状、payload 非法、缺 auth claim、结构不符都返回 `None`。
pub fn account_info_from_id_token(id_token: &str) -> Option<AccountInfo> {
    // JWT 三段式 header.payload.signature；我们只要未验签的 payload——
    // id_token 在换码/刷新时已按 RS256 + JWKS 验签（identity 模块），
    // 这里读取的是同一份可信凭据，不做二次验签。
    let payload = id_token.split('.').nth(1)?;
    let json = crate::util::base64url_decode(payload).ok()?;
    let value: Value = serde_json::from_slice(&json).ok()?;
    let auth = value.get(AUTH_CLAIM)?.as_object()?;
    let claim_str = |key: &str| -> Option<String> {
        auth.get(key)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    Some(AccountInfo {
        account_id: claim_str("chatgpt_account_id"),
        is_fedramp: auth
            .get("chatgpt_account_is_fedramp")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        plan_type: claim_str("chatgpt_plan_type"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::base64url_encode;
    use serde_json::json;

    fn fake_id_token(auth: Value) -> String {
        let header = base64url_encode(br#"{"alg":"RS256","typ":"JWT"}"#);
        let mut claims = json!({"iss": "https://auth.openai.com", "sub": "user-1"});
        claims
            .as_object_mut()
            .unwrap()
            .insert(AUTH_CLAIM.into(), auth);
        let payload = base64url_encode(claims.to_string().as_bytes());
        format!("{header}.{payload}.c2ln")
    }

    #[test]
    fn parses_account_claims() {
        let info = account_info_from_id_token(&fake_id_token(json!({
            "chatgpt_account_id": "acct_123",
            "chatgpt_account_is_fedramp": true,
            "chatgpt_plan_type": "team",
        })))
        .expect("应解析出账号面");
        assert_eq!(info.account_id.as_deref(), Some("acct_123"));
        assert!(info.is_fedramp);
        assert_eq!(info.plan_type.as_deref(), Some("team"));
    }

    #[test]
    fn missing_or_empty_claims_are_none() {
        let info = account_info_from_id_token(&fake_id_token(json!({
            "chatgpt_plan_type": "plus",
        })))
        .expect("auth claim 存在即 Some");
        assert_eq!(info.account_id, None);
        assert!(!info.is_fedramp);

        let info = account_info_from_id_token(&fake_id_token(json!({
            "chatgpt_account_id": "  ",
        })))
        .expect("auth claim 存在即 Some");
        assert_eq!(info.account_id, None, "空白账号 id 等价缺失");
    }

    #[test]
    fn malformed_tokens_are_none() {
        assert_eq!(account_info_from_id_token("not-a-jwt"), None);
        assert_eq!(account_info_from_id_token("a.b.c"), None);
        // payload 不是 JSON。
        assert_eq!(
            account_info_from_id_token(&format!("h.{}.s", base64url_encode(b"zzz"))),
            None
        );
        // 没有 auth claim。
        let header = base64url_encode(br#"{"alg":"RS256"}"#);
        let payload = base64url_encode(br#"{"sub":"u"}"#);
        assert_eq!(
            account_info_from_id_token(&format!("{header}.{payload}.s")),
            None
        );
    }
}
