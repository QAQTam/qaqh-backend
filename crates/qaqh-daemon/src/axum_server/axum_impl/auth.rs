//! axum_impl::auth — see parent module docs.

use super::*;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

pub(crate) fn is_authorized(headers: &HeaderMap, token: &str) -> bool {
    let Some(provided) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let expected = format!("Bearer {token}");
    let provided_digest = Sha256::digest(provided.as_bytes());
    let expected_digest = Sha256::digest(expected.as_bytes());
    bool::from(provided_digest.ct_eq(&expected_digest))
}

pub(crate) fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, "unauthorized").into_response()
}

pub(crate) fn lease_required_json() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::CONTENT_TYPE, "application/json")],
        br#"{"code":"lease_required","message":"open a Ringing v1 client session first"}"#
            .as_slice(),
    )
        .into_response()
}

pub(crate) fn get_session_id(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-qaqh-client-session-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
}

/// v1 HTTP 面的路径频道解析。v1 命令 handler 删除后仅剩单测消费；
/// 阶段 3 清理 v1 面时一并退役。
#[cfg(test)]
pub(crate) fn parse_channel(s: &str) -> Option<RingingChannel> {
    match s {
        "control" => Some(RingingChannel::Control),
        "conversation" => Some(RingingChannel::Conversation),
        "tool" => Some(RingingChannel::Tool),
        _ => None,
    }
}

pub(crate) fn session_close_session(
    close_session: &str,
    envelope_session: &Option<String>,
) -> String {
    if !close_session.is_empty() {
        close_session.to_string()
    } else {
        envelope_session.clone().unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorization_rejects_prefix_length_and_scheme_variants() {
        let token = "0123456789abcdef";
        let mut headers = HeaderMap::new();
        assert!(!is_authorized(&headers, token));

        for value in [
            format!("Bearer {token}x"),
            "Bearer 0123456789abcde".to_string(),
            format!("bearer {token}"),
            format!("Token {token}"),
            format!("Bearer  {token}"),
        ] {
            headers.insert(header::AUTHORIZATION, value.parse().unwrap());
            assert!(
                !is_authorized(&headers, token),
                "must reject malformed authorization header {value:?}"
            );
        }

        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );
        assert!(is_authorized(&headers, token));
    }
}
