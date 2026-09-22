//! Browser-origin, cookie, and CSRF checks for the loopback gateway.

use axum::http::{HeaderMap, header};
use subtle::ConstantTimeEq;

pub const SESSION_COOKIE: &str = "qaqh_webui_session";
pub const CSRF_HEADER: &str = "x-qaqh-csrf";

pub fn host_allowed(headers: &HeaderMap, allowed_hosts: &[String]) -> bool {
    let Some(host) = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
    else {
        return false;
    };
    allowed_hosts.iter().any(|allowed| host == allowed)
}

pub fn origin_allowed(headers: &HeaderMap, allowed_origins: &[String]) -> bool {
    let Some(origin) = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
    else {
        return false;
    };
    allowed_origins.iter().any(|allowed| origin == allowed)
}

pub fn sec_fetch_site_allowed(headers: &HeaderMap) -> bool {
    match headers
        .get("sec-fetch-site")
        .and_then(|value| value.to_str().ok())
    {
        None => true,
        Some(site) => site.eq_ignore_ascii_case("same-origin") || site.eq_ignore_ascii_case("none"),
    }
}

pub fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let cookies = headers.get(header::COOKIE)?.to_str().ok()?;
    cookies.split(';').find_map(|cookie| {
        let (candidate, value) = cookie.trim().split_once('=')?;
        (candidate == name).then(|| value.to_string())
    })
}

pub fn csrf_matches(headers: &HeaderMap, expected: &str) -> bool {
    let Some(provided) = headers
        .get(CSRF_HEADER)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    bool::from(provided.as_bytes().ct_eq(expected.as_bytes()))
}

pub fn session_cookie(session_id: &str, max_age_secs: u64) -> String {
    format!(
        "{SESSION_COOKIE}={session_id}; HttpOnly; SameSite=Strict; Path=/__gateway; Max-Age={max_age_secs}"
    )
}

pub fn clear_session_cookie() -> String {
    format!("{SESSION_COOKIE}=; HttpOnly; SameSite=Strict; Path=/__gateway; Max-Age=0")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderName, HeaderValue};

    #[test]
    fn cookie_origin_and_csrf_checks_are_exact() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("127.0.0.1:41234"));
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("http://127.0.0.1:41234"),
        );
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("other=1; qaqh_webui_session=abc"),
        );
        headers.insert(
            HeaderName::from_static(CSRF_HEADER),
            HeaderValue::from_static("csrf-value"),
        );

        assert!(host_allowed(&headers, &["127.0.0.1:41234".to_string()]));
        assert!(origin_allowed(
            &headers,
            &["http://127.0.0.1:41234".to_string()]
        ));
        assert_eq!(
            cookie_value(&headers, SESSION_COOKIE).as_deref(),
            Some("abc")
        );
        assert!(csrf_matches(&headers, "csrf-value"));
        assert!(!csrf_matches(&headers, "csrf-valu"));
        assert!(!host_allowed(&headers, &["localhost:41234".to_string()]));
    }
}
