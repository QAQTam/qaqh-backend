//! 平台信封：`{ code, msg, data: { biz_code, biz_msg, biz_data } }`。
//!
//! 实测（2026-10-14）的三段形状都在下面的回归里：成功、顶层业务失败、以及
//! `data` 缺失的失败响应。平台把「HTTP 层」与「业务层」分成两条码，**两层都要看**：
//! `code != 0` 是平台级失败，`biz_code != 0` 是业务级失败，两者都可能是 `40003`
//! （鉴权失效，见 [`crate::error::classify_biz_code`]）。

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::error::{DsCode, DsError, classify_biz_code};

#[derive(Debug, Deserialize)]
struct RawEnvelope {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    msg: String,
    #[serde(default)]
    data: Option<RawData>,
}

#[derive(Debug, Default, Deserialize)]
struct RawData {
    #[serde(default)]
    biz_code: i64,
    #[serde(default)]
    biz_msg: String,
    #[serde(default)]
    biz_data: Option<Value>,
}

/// 解出 `biz_data` 并投影成 `T`。
///
/// 调用方只需描述**业务载荷**的形状（例如 [`crate::account::Summary`]），信封的
/// 两层码在这里统一处理——否则每个端点都要自己记得看一眼 `biz_code`。
pub fn parse<T: DeserializeOwned>(body: &str) -> Result<T, DsError> {
    let value = parse_value(body)?;
    serde_json::from_value(value)
        .map_err(|error| DsError::new(DsCode::Protocol, format!("业务载荷形状不符: {error}")))
}

/// 只取 `biz_data` 原值（形状由调用方自己判，例如需要看平台多给了什么字段）。
pub fn parse_value(body: &str) -> Result<Value, DsError> {
    let raw: RawEnvelope = serde_json::from_str(body)
        .map_err(|error| DsError::new(DsCode::Protocol, format!("信封解析失败: {error}")))?;
    if raw.code != 0 {
        return Err(DsError::new(classify_biz_code(raw.code), raw.msg));
    }
    let data = raw.data.unwrap_or_default();
    if data.biz_code != 0 {
        return Err(DsError::new(classify_biz_code(data.biz_code), data.biz_msg));
    }
    Ok(data.biz_data.unwrap_or(Value::Null))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::DsCode;

    /// 实测返回（`auth_init`）原文。
    const AUTH_INIT_OK: &str = r#"{"code":0,"msg":"","data":{"biz_code":0,"biz_msg":"","biz_data":{"authorize_url":"https://platform.deepseek.com/dsh/authorize?authorize_id=qJPrcQ5oFxpKSOg7nTa3nrO01lPFsI3BV7AHeSyDQ6U","authorize_id":"qJPrcQ5oFxpKSOg7nTa3nrO01lPFsI3BV7AHeSyDQ6U","expires_in":600}}}"#;

    #[derive(Debug, Deserialize, PartialEq)]
    struct AuthInit {
        authorize_url: String,
        authorize_id: String,
        expires_in: u64,
    }

    #[test]
    fn success_unwraps_biz_data() {
        let init: AuthInit = parse(AUTH_INIT_OK).expect("init 必须能解出");
        assert_eq!(init.expires_in, 600);
        assert_eq!(init.authorize_id.len(), 43);
        assert_eq!(
            init.authorize_url,
            "https://platform.deepseek.com/dsh/authorize?authorize_id=qJPrcQ5oFxpKSOg7nTa3nrO01lPFsI3BV7AHeSyDQ6U"
        );
        // 实测：authorize_url 里**只有 authorize_id**，不带 state——state 由服务端
        // 记住，回调时原样带回（我们的回调校验就依赖这一点）。
    }

    #[test]
    fn top_level_business_code_wins_and_40003_means_reauth() {
        let body =
            r#"{"code":0,"msg":"","data":{"biz_code":40003,"biz_msg":"invalid authorization"}}"#;
        let error = parse_value(body).expect_err("40003 必须报错");
        assert_eq!(error.code(), DsCode::Unauthorized);
        assert!(error.requires_reauth());
        assert_eq!(error.message(), "invalid authorization");

        let other = r#"{"code":0,"msg":"","data":{"biz_code":10001,"biz_msg":"订单不存在"}}"#;
        let error = parse_value(other).expect_err("非零 biz_code 必须报错");
        assert_eq!(error.code(), DsCode::Business(10001));
        assert!(!error.requires_reauth());
    }

    #[test]
    fn platform_level_failure_without_data_is_reported_with_its_message() {
        let body = r#"{"code":50000,"msg":"internal error"}"#;
        let error = parse_value(body).expect_err("非零 code 必须报错");
        assert_eq!(error.code(), DsCode::Business(50_000));
        assert_eq!(error.message(), "internal error");
    }

    #[test]
    fn malformed_body_is_a_protocol_failure_not_a_reauth() {
        let error = parse_value("<html>502 Bad Gateway</html>").expect_err("非 JSON 必须报错");
        assert_eq!(error.code(), DsCode::Protocol);
        assert!(!error.requires_reauth(), "网关页不等于凭据失效");
    }

    #[test]
    fn null_biz_data_is_accepted_for_acknowledgement_shaped_calls() {
        // `ack_bonus_notified` 这类只回状态的调用：biz_data 为 null，不该当成错误。
        let body = r#"{"code":0,"msg":"","data":{"biz_code":0,"biz_msg":"","biz_data":null}}"#;
        assert_eq!(parse_value(body).expect("null 载荷合法"), Value::Null);
        let unit: () = parse(body).expect("投影成 () 也必须成立");
        assert_eq!(unit, ());
    }

    #[test]
    fn the_response_of_a_revoked_token_is_a_reauth_not_a_business_error() {
        // 实测原文（注销后立刻打 users/current）：平台把失效码放在**顶层 `code`**、
        // `data` 为 null、文案是英文。只盯 `data.biz_code` 的实现会把它当业务失败，
        // 于是凭据永远清不掉。
        let body = r#"{"code":40003,"msg":"Authorization Failed (invalid token)","data":null}"#;
        let error = parse_value(body).expect_err("失效必须报错");
        assert_eq!(error.code(), DsCode::Unauthorized);
        assert!(error.requires_reauth(), "顶层 code 里的 40003 也要被认出来");
        assert_eq!(error.message(), "Authorization Failed (invalid token)");
    }
}
