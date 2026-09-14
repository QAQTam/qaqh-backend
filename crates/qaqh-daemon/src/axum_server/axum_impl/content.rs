//! axum_impl::content — see parent module docs.

use super::*;
use axum::http::HeaderValue;

pub(crate) async fn handle_content_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(content_id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if !is_authorized(&headers, &state.token) {
        return unauthorized();
    }
    let Some(session_id) = get_session_id(&headers) else {
        return lease_required_json();
    };
    if content_id.is_empty() {
        return (StatusCode::BAD_REQUEST, "missing content_id").into_response();
    }
    let Some(seed) = params.get("seed").cloned() else {
        return (StatusCode::BAD_REQUEST, "missing seed query param").into_response();
    };
    let owns = state
        .leases
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .owns_seed(&session_id, &seed);
    if !owns {
        return (
            StatusCode::FORBIDDEN,
            [(header::CONTENT_TYPE, "application/json")],
            br#"{"code":"content_forbidden","message":"content is not owned by this session"}"#
                .to_vec(),
        )
            .into_response();
    }
    match state.hub.get_content(&seed, &content_id) {
        Some(entry) => {
            // BUG-2026-09-13-03 双保险：历史上可能已入库非法 media_type（注入
            // 面修复前），直接拼响应头会让 axum TryInto<HeaderValue> 失败 →
            // panic（存储型 DoS）。出站前校验，非法回退 octet-stream。
            let content_type = if is_valid_media_type(&entry.media_type) {
                HeaderValue::from_str(&entry.media_type)
                    .unwrap_or(HeaderValue::from_static("application/octet-stream"))
            } else {
                HeaderValue::from_static("application/octet-stream")
            };
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, content_type)],
                entry.bytes,
            )
                .into_response()
        }
        None => (StatusCode::NOT_FOUND, "content not found or expired").into_response(),
    }
}

/// BUG-2026-09-13-03：media_type 会直接拼进 GET 响应头（content.rs:40），
/// 合法性必须在此收口：非空、≤255 字节、全部为含空格的可打印 ASCII
/// （0x20..=0x7E，空格覆盖参数形态如 `; charset=utf-8`）。CRLF / 控制
/// 字符 / 非 ASCII 一律拒绝——HeaderValue 构造必然失败（panic）的值不
/// 允许入库。
pub(crate) fn is_valid_media_type(value: &str) -> bool {
    !value.is_empty() && value.len() <= 255 && value.bytes().all(|b| (0x20..=0x7e).contains(&b))
}

pub(crate) async fn handle_content_upload(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !is_authorized(&headers, &state.token) {
        return unauthorized();
    }
    let Some(session_id) = get_session_id(&headers) else {
        return lease_required_json();
    };
    let Some(ct) = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
    else {
        return (StatusCode::BAD_REQUEST, "missing content type").into_response();
    };
    let Some(boundary) = ct
        .split(';')
        .find_map(|part| part.trim().strip_prefix("boundary="))
        .map(|v| v.trim_matches('"').as_bytes().to_vec())
    else {
        return (StatusCode::BAD_REQUEST, "multipart boundary required").into_response();
    };
    let delimiter = [b"--".as_slice(), boundary.as_slice()].concat();
    let mut seed: Option<String> = None;
    let mut media_type: Option<String> = None;
    let mut content: Option<Vec<u8>> = None;
    // Split on the exact boundary without interpreting arbitrary binary bytes.
    let mut parts: Vec<&[u8]> = Vec::new();
    let mut offset = 0;
    while let Some(relative) = body[offset..]
        .windows(delimiter.len())
        .position(|window| window == delimiter.as_slice())
    {
        parts.push(&body[offset..offset + relative]);
        offset += relative + delimiter.len();
    }
    parts.push(&body[offset..]);
    for part in parts {
        let part = part.strip_prefix(b"\r\n").unwrap_or(part);
        let part = part.strip_suffix(b"\r\n").unwrap_or(part);
        let Some(header_end) = part.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&part[..header_end]);
        let value = &part[header_end + 4..];
        let Some(name) = headers
            .split(';')
            .find_map(|piece| piece.trim().strip_prefix("name=\""))
            .and_then(|value| value.strip_suffix('"'))
        else {
            continue;
        };
        match name {
            "seed" => seed = String::from_utf8(value.to_vec()).ok(),
            "media_type" => media_type = String::from_utf8(value.to_vec()).ok(),
            "content" => content = Some(value.to_vec()),
            _ => {}
        }
    }
    let Some(seed) = seed.filter(|s| !s.is_empty()) else {
        return (StatusCode::BAD_REQUEST, "missing seed").into_response();
    };
    let Some(content) = content else {
        return (StatusCode::BAD_REQUEST, "missing file part").into_response();
    };
    let media_type = media_type.unwrap_or_else(|| "application/octet-stream".into());
    // BUG-2026-09-13-03：注入面必须在入库前拒绝——含 CRLF 的 media_type 一旦
    // 入库，此后每次 GET 该 content 都会让 axum handler panic（存储型 DoS）。
    if !is_valid_media_type(&media_type) {
        return (StatusCode::BAD_REQUEST, "invalid media_type").into_response();
    }
    let owns = state
        .leases
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .owns_seed(&session_id, &seed);
    if !owns {
        return (
            StatusCode::FORBIDDEN,
            [(header::CONTENT_TYPE, "application/json")],
            br#"{"code":"content_forbidden"}"#.to_vec(),
        )
            .into_response();
    }
    let content_id = state
        .hub
        .put_content(&seed, &media_type, content.clone(), false);
    let resp = serde_json::json!({
        "content_id": content_id.clone(),
        "media_type": media_type,
        "sha256": content_id.clone(),
        "size": content.len(),
        "truncated": false,
    });
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_vec(&resp).unwrap_or_default(),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── BUG-2026-09-13-03 回归 ──

    #[test]
    fn valid_media_types_pass() {
        assert!(is_valid_media_type("text/plain"));
        assert!(is_valid_media_type("image/png"));
        assert!(is_valid_media_type(
            "application/vnd.qaqh.attachment+v1; charset=utf-8"
        ));
    }

    #[test]
    fn crlf_and_control_chars_rejected() {
        assert!(!is_valid_media_type("text/plain\r\nX-Evil: 1"));
        assert!(!is_valid_media_type("text/plain\nX-Evil: 1"));
        assert!(!is_valid_media_type("text\u{0}/plain"));
        assert!(!is_valid_media_type("中文/类型"));
        assert!(!is_valid_media_type(""));
    }
}
