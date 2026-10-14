//! ID token 校验：RS256 + JWKS（`ring`，不引入 JWT 依赖）。
//!
//! 文档要求逐条落地：
//! - 签名对 OpenAI 公布的 JWKS 验证；`iss` 精确匹配、`aud` 对**下发的 client_id**；
//! - `exp`/`iat` 必存在，允许小 skew；`nonce` 必须等于本次尝试的 nonce；
//! - 多 `aud` 或存在 `azp` 时 `azp` 必须等于 client_id；
//! - **`sub` 才是账户身份**，`email` 只是展示；`sub`/`email` 都不是 workspace 标识。
//!
//! 错误分类是安全属性：**「JWKS/密钥服务不可用」与「token 不可信」必须分开**。
//! 前者是可用性故障（保连接、稍后重试），后者是完整性失败。这里只产后者；
//! 前者由调用方在拉取 JWKS 失败时给出（见 `manager`）。

use serde_json::Value;

use crate::error::{SiwcCode, SiwcError};
use crate::util::base64url_decode;

/// 允许的时钟偏移（文档示例 5 秒）。
pub const CLOCK_SKEW_MS: u64 = 5_000;

/// 已校验的账户身份。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub subject: String,
    pub email: Option<String>,
    pub name: Option<String>,
}

/// JWKS 里的一个 RSA 公钥（`n`/`e` 已解码为大端字节）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Jwk {
    pub kid: String,
    pub n: Vec<u8>,
    pub e: Vec<u8>,
}

/// 解析 JWKS（只保留 `kty == RSA` 且带 `kid` 的项）。
pub fn parse_jwks(raw: &str) -> Result<Vec<Jwk>, SiwcError> {
    let value: Value = serde_json::from_str(raw).map_err(|e| {
        SiwcError::new(
            SiwcCode::IdentityVerificationUnavailable,
            format!("JWKS 非法: {e}"),
        )
    })?;
    parse_jwks_value(&value)
}

pub fn parse_jwks_value(value: &Value) -> Result<Vec<Jwk>, SiwcError> {
    let keys = value
        .get("keys")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            SiwcError::new(
                SiwcCode::IdentityVerificationUnavailable,
                "JWKS 缺 keys 数组",
            )
        })?;
    let mut parsed = Vec::new();
    for key in keys {
        if key.get("kty").and_then(Value::as_str) != Some("RSA") {
            continue;
        }
        let (Some(kid), Some(n), Some(e)) = (
            key.get("kid").and_then(Value::as_str),
            key.get("n").and_then(Value::as_str),
            key.get("e").and_then(Value::as_str),
        ) else {
            continue;
        };
        let (Ok(n), Ok(e)) = (base64url_decode(n), base64url_decode(e)) else {
            continue;
        };
        if n.is_empty() || e.is_empty() {
            continue;
        }
        parsed.push(Jwk {
            kid: kid.to_owned(),
            n,
            e,
        });
    }
    if parsed.is_empty() {
        return Err(SiwcError::new(
            SiwcCode::IdentityVerificationUnavailable,
            "JWKS 没有可用的 RSA 公钥",
        ));
    }
    Ok(parsed)
}

/// JWT 的三段投影。
#[derive(Debug, Clone)]
pub struct IdTokenParts {
    pub kid: Option<String>,
    /// `header.payload` 原文（验签输入，不做任何重编码）。
    pub signing_input: Vec<u8>,
    pub signature: Vec<u8>,
    pub claims: Value,
}

/// 拆解并做**结构性**校验（段数、alg、JSON 形状）。
pub fn split_id_token(token: &str) -> Result<IdTokenParts, SiwcError> {
    let invalid = |message: &str| SiwcError::new(SiwcCode::InvalidIdToken, message.to_string());
    let segments: Vec<&str> = token.split('.').collect();
    if segments.len() != 3 {
        return Err(invalid("ID token 不是三段式 JWT"));
    }
    let (header_raw, payload_raw, signature_raw) = match (segments.first(), segments.get(1), segments.get(2)) {
        (Some(h), Some(p), Some(s)) => (*h, *p, *s),
        _ => return Err(invalid("ID token 段缺失")),
    };
    if signature_raw.is_empty() {
        return Err(invalid("ID token 无签名"));
    }
    let header_bytes = base64url_decode(header_raw).map_err(|e| invalid(&format!("header 解码失败: {e}")))?;
    let header: Value = serde_json::from_slice(&header_bytes)
        .map_err(|e| invalid(&format!("header 非法: {e}")))?;
    if header.get("alg").and_then(Value::as_str) != Some("RS256") {
        return Err(invalid("ID token alg 不是 RS256"));
    }
    let payload_bytes = base64url_decode(payload_raw).map_err(|e| invalid(&format!("payload 解码失败: {e}")))?;
    let claims: Value = serde_json::from_slice(&payload_bytes)
        .map_err(|e| invalid(&format!("claims 非法: {e}")))?;
    let signature = base64url_decode(signature_raw).map_err(|e| invalid(&format!("签名解码失败: {e}")))?;
    Ok(IdTokenParts {
        kid: header.get("kid").and_then(Value::as_str).map(str::to_owned),
        signing_input: format!("{header_raw}.{payload_raw}").into_bytes(),
        signature,
        claims,
    })
}

/// 按 `kid` 选键验签；无 `kid` 时对全部已知键尝试（单键轮换的常见形态）。
///
/// 找不到对应键 = **完整性失败**（不是可用性）：已公布的 JWKS 里没有任何键
/// 能为这段签名背书。调用方在重取 JWKS 后再判一次。
pub fn verify_signature(parts: &IdTokenParts, keys: &[Jwk]) -> Result<(), SiwcError> {
    let invalid = |message: &str| SiwcError::new(SiwcCode::InvalidIdToken, message.to_string());
    let candidates: Vec<&Jwk> = match &parts.kid {
        Some(kid) => keys.iter().filter(|key| &key.kid == kid).collect(),
        None => keys.iter().collect(),
    };
    if candidates.is_empty() {
        return Err(invalid("JWKS 中没有匹配 kid 的公钥"));
    }
    for key in candidates {
        let components = ring::signature::RsaPublicKeyComponents {
            n: &key.n,
            e: &key.e,
        };
        if components
            .verify(
                &ring::signature::RSA_PKCS1_2048_8192_SHA256,
                &parts.signing_input,
                &parts.signature,
            )
            .is_ok()
        {
            return Ok(());
        }
    }
    Err(invalid("ID token 签名校验失败"))
}

/// claims 校验 → 身份。`expected_nonce` 为本次授权尝试的 nonce。
pub fn validate_claims(
    claims: &Value,
    expected_issuer: &str,
    client_id: &str,
    expected_nonce: Option<&str>,
    now_ms: u64,
) -> Result<Identity, SiwcError> {
    let invalid = |message: String| SiwcError::new(SiwcCode::InvalidIdToken, message);
    let issuer = claims
        .get("iss")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("ID token 缺 iss".into()))?;
    if issuer != expected_issuer {
        return Err(invalid(format!("ID token iss 非预期: {issuer}")));
    }

    let audience_ok = match claims.get("aud") {
        Some(Value::String(aud)) => aud == client_id,
        Some(Value::Array(items)) => items.iter().any(|item| item.as_str() == Some(client_id)),
        _ => false,
    };
    if !audience_ok {
        return Err(invalid("ID token aud 不含本次 client_id".into()));
    }
    let multiple_audiences = claims
        .get("aud")
        .and_then(Value::as_array)
        .map(|items| items.len() > 1)
        .unwrap_or(false);
    if let Some(azp) = claims.get("azp").and_then(Value::as_str) {
        if azp != client_id {
            return Err(invalid("ID token azp 与 client_id 不符".into()));
        }
    } else if multiple_audiences {
        return Err(invalid("ID token 多 aud 但缺 azp".into()));
    }

    let exp_seconds = claims
        .get("exp")
        .and_then(Value::as_u64)
        .ok_or_else(|| invalid("ID token 缺 exp".into()))?;
    if exp_seconds.saturating_mul(1000).saturating_add(CLOCK_SKEW_MS) <= now_ms {
        return Err(invalid("ID token 已过期".into()));
    }
    if claims.get("iat").and_then(Value::as_u64).is_none() {
        return Err(invalid("ID token 缺 iat".into()));
    }

    let subject = claims
        .get("sub")
        .and_then(Value::as_str)
        .filter(|sub| !sub.is_empty())
        .ok_or_else(|| invalid("ID token 缺 sub".into()))?
        .to_owned();

    if let Some(expected) = expected_nonce {
        let actual = claims.get("nonce").and_then(Value::as_str);
        if actual != Some(expected) {
            return Err(invalid("ID token nonce 不匹配".into()));
        }
    }

    Ok(Identity {
        subject,
        email: claims
            .get("email")
            .and_then(Value::as_str)
            .map(str::to_owned),
        name: claims
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const JWK_N: &str = "r5PETJTVMLeXDfDVkdL3YDL6LRas91j_HkGX5AaLzHFqAbhu3dl9eZDjIx6dUvG5m_R2E5rNeP6HfRsv9zNHggDywfkkdGJcvbJ8OTLJeZM1twZv_NNIj9P_SKak7gy-K5_-Ugl7ptsnPllU-5Jhgj9zmyTpK2hE35JY_2uflelB3SDg-rHR3Hp67ThMzDfZ3jQsUyAnyApD9FbWVlCwYF96tyN8lebQ_pPDRRUXi7snBcqZJ2UlS9r1Q0bMIucIZx_i2g8SB2L2V8tT8KVloaqRLBj3143dqFxwivIRu9R9eLHFQhoIouOpZRQDpJMNDkeySFXCx01lwMGFE8-p9Q";

    fn jwks() -> Vec<Jwk> {
        parse_jwks(&format!(
            r#"{{"keys":[{{"kty":"RSA","kid":"{}","alg":"RS256","n":"{JWK_N}","e":"AQAB"}}]}}"#,
            crate::fixture::TEST_KID
        ))
        .expect("jwks")
    }

    fn claims() -> Value {
        serde_json::json!({
            "iss": "https://auth.openai.com",
            "aud": "oaiapp_test",
            "sub": "sub-abc",
            "email": "user@example.com",
            "name": "User",
            "nonce": "n-1",
            "iat": 1_700_000_000u64,
            "exp": 4_100_000_000u64
        })
    }

    #[test]
    fn valid_claims_yield_identity() {
        let identity = validate_claims(
            &claims(),
            "https://auth.openai.com",
            "oaiapp_test",
            Some("n-1"),
            1_700_000_000_000,
        )
        .expect("identity");
        assert_eq!(identity.subject, "sub-abc");
        assert_eq!(identity.email.as_deref(), Some("user@example.com"));
    }

    #[test]
    fn audience_and_nonce_are_enforced() {
        let wrong_aud = claims();
        assert!(
            validate_claims(&wrong_aud, "https://auth.openai.com", "oaiapp_other", Some("n-1"), 1_700_000_000_000)
                .is_err()
        );
        assert!(
            validate_claims(&claims(), "https://auth.openai.com", "oaiapp_test", Some("n-2"), 1_700_000_000_000)
                .is_err(),
            "nonce 不匹配必须拒"
        );
        // 多 aud 且 azp 缺失 → 拒。
        let mut multi = claims();
        multi["aud"] = serde_json::json!(["oaiapp_test", "oaiapp_other"]);
        assert!(validate_claims(&multi, "https://auth.openai.com", "oaiapp_test", Some("n-1"), 1_700_000_000_000).is_err());
        multi["azp"] = serde_json::json!("oaiapp_test");
        assert!(validate_claims(&multi, "https://auth.openai.com", "oaiapp_test", Some("n-1"), 1_700_000_000_000).is_ok());
        multi["azp"] = serde_json::json!("oaiapp_other");
        assert!(validate_claims(&multi, "https://auth.openai.com", "oaiapp_test", Some("n-1"), 1_700_000_000_000).is_err());
    }

    #[test]
    fn expiry_and_subject_are_enforced() {
        let mut expired = claims();
        expired["exp"] = serde_json::json!(1_600_000_000u64);
        assert!(validate_claims(&expired, "https://auth.openai.com", "oaiapp_test", Some("n-1"), 1_700_000_000_000).is_err());

        let mut no_sub = claims();
        no_sub["sub"] = serde_json::json!("");
        assert!(validate_claims(&no_sub, "https://auth.openai.com", "oaiapp_test", Some("n-1"), 1_700_000_000_000).is_err());
    }

    #[test]
    fn jwks_filters_non_rsa_and_requires_keys() {
        let mixed = parse_jwks(
            r#"{"keys":[{"kty":"EC","kid":"ec1","n":"AA","e":"AQAB"},
                        {"kty":"RSA","kid":"k1","n":"AQAB","e":"AQAB"}]}"#,
        )
        .expect("rsa survives");
        assert_eq!(mixed.len(), 1);
        assert_eq!(mixed[0].kid, "k1");

        assert_eq!(
            parse_jwks(r#"{"keys":[]}"#).expect_err("empty").code(),
            SiwcCode::IdentityVerificationUnavailable
        );
    }

    #[test]
    fn split_rejects_structural_violations() {
        assert_eq!(
            split_id_token("only.two").expect_err("two segments").code(),
            SiwcCode::InvalidIdToken
        );
        // header 里 alg=none 必须拒。
        let header = crate::util::base64url_encode(br#"{"alg":"none","kid":"k1"}"#);
        let payload = crate::util::base64url_encode(br#"{"sub":"s"}"#);
        let token = format!("{header}.{payload}.{}", crate::util::base64url_encode(b"sig"));
        assert_eq!(
            split_id_token(&token).expect_err("alg none").code(),
            SiwcCode::InvalidIdToken
        );
    }

    /// 真实密钥对：用固定测试私钥签一段 JWT，走完整校验链
    /// （拆解 → 按 kid 验签 → claims）。
    #[test]
    fn signature_round_trip_with_fixture_key() {
        fn verify(token: &str) -> Result<Identity, SiwcError> {
            let parts = split_id_token(token)?;
            verify_signature(&parts, &jwks())?;
            validate_claims(
                &parts.claims,
                "https://auth.openai.com",
                "oaiapp_test",
                Some("n-1"),
                1_700_000_000_000,
            )
        }

        let token = crate::fixture::sign_id_token(&crate::fixture::claims_json(
            "https://auth.openai.com",
            "oaiapp_test",
            "n-1",
        ));
        let identity = verify(&token).expect("verified");
        assert_eq!(identity.subject, "sub-fixture");

        // 篡改**签名字节**（payload 是 base64url，明文串不出现在 token 里，
        // 直接 replace 明文是测不出签名校验的）。
        let mut tampered = token.clone();
        let separator = tampered.rfind('.').expect("signature separator");
        let signature = tampered
            .get(separator + 1..)
            .expect("signature segment")
            .to_string();
        let flipped = match signature.get(1..) {
            Some(rest) => format!("{}{rest}", if signature.starts_with('A') { "B" } else { "A" }),
            None => signature,
        };
        tampered.replace_range(separator + 1.., &flipped);
        assert_eq!(
            verify(&tampered).expect_err("tampered").code(),
            SiwcCode::InvalidIdToken
        );

        // 未知 kid（密钥轮换但 JWKS 尚未更新）：已公布密钥里无人能背书这段签名。
        let unseen = parse_jwks(&format!(
            r#"{{"keys":[{{"kty":"RSA","kid":"unseen-kid","alg":"RS256","n":"{JWK_N}","e":"AQAB"}}]}}"#
        ))
        .expect("jwks");
        let parts = split_id_token(&token).expect("split");
        assert_eq!(
            verify_signature(&parts, &unseen)
                .expect_err("unknown kid")
                .code(),
            SiwcCode::InvalidIdToken
        );
    }
}
