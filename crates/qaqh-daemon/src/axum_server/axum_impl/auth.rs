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

pub(crate) fn parse_channel(s: &str) -> Option<RingingChannel> {
    match s {
        "control" => Some(RingingChannel::Control),
        "conversation" => Some(RingingChannel::Conversation),
        "tool" => Some(RingingChannel::Tool),
        _ => None,
    }
}

pub(crate) fn session_close_seed(close_seed: &str, envelope_seed: &Option<String>) -> String {
    if !close_seed.is_empty() {
        close_seed.to_string()
    } else {
        envelope_seed.clone().unwrap_or_default()
    }
}

pub(crate) fn publish_session_created(hub: &RingingHub, seed: &str, command_id: &str) {
    let _ = hub.publish_with_causation(
        seed,
        qaqh_domain::DomainEvent::Control(qaqh_domain::ControlEvent::SessionStateChanged {
            session_id: seed.to_string(),
            state: qaqh_domain::SessionState::Created,
        }),
        Some(command_id),
    );
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
