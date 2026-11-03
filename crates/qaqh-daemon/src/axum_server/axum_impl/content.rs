//! axum_impl::content — see parent module docs.

use super::v2::api_error_response;
use super::*;
use axum::body::Body;
use axum::http::HeaderValue;

/// `GET /ringing/v2/content/{content_id}`。
///
/// **不带 seed 参数**：按内容 ID 查询关联会话，检查调用方是否持有其中一个会话的 lease。
/// 没有关联内容返回 404；内容存在但调用方无权访问时返回 403。
pub(crate) async fn handle_content_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(content_id): Path<String>,
) -> Response {
    let Some(session_id) = get_session_id(&headers) else {
        return lease_required_json();
    };
    if content_id.is_empty() {
        return (StatusCode::BAD_REQUEST, "missing content_id").into_response();
    }
    // canonical `ContentRef` 用 `sha256:<hex>`；上传响应使用裸 hex。
    let store_id = content_id
        .strip_prefix("sha256:")
        .unwrap_or(content_id.as_str());
    let owners = match state.hub.content_sessions(store_id) {
        Ok(owners) => owners,
        Err(error) => {
            return api_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "content_unavailable",
                &format!("content lookup failed: {error}"),
            );
        }
    };
    if owners.is_empty() {
        return (StatusCode::NOT_FOUND, "content not found").into_response();
    }
    let owner_session = {
        let leases = state.leases.lock().unwrap_or_else(|e| e.into_inner());
        owners
            .iter()
            .find(|owner| leases.owns_session(&session_id, owner))
            .cloned()
    };
    let Some(owner_session) = owner_session else {
        return (
            StatusCode::FORBIDDEN,
            [(header::CONTENT_TYPE, "application/json")],
            br#"{"code":"content_forbidden","message":"content is not owned by this session"}"#
                .to_vec(),
        )
            .into_response();
    };
    let entry = match state.hub.get_content(&owner_session, store_id) {
        Ok(Some(entry)) => entry,
        Ok(None) => return (StatusCode::NOT_FOUND, "content not found").into_response(),
        Err(error) => {
            return api_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "content_unavailable",
                &format!("content read failed: {error}"),
            );
        }
    };
    serve_content_bytes(&headers, &entry.media_type, entry.bytes)
}

/// 统一的正文出站面：media_type 出站校验 + RFC 9110 单区间 Range。
fn serve_content_bytes(headers: &HeaderMap, media_type: &str, bytes: Vec<u8>) -> Response {
    // BUG-2026-09-13-03 双保险：历史上可能已入库非法 media_type（注入
    // 面修复前），直接拼响应头会让 axum TryInto<HeaderValue> 失败 →
    // panic（存储型 DoS）。出站前校验，非法回退 octet-stream。
    let content_type = if is_valid_media_type(media_type) {
        HeaderValue::from_str(media_type)
            .unwrap_or(HeaderValue::from_static("application/octet-stream"))
    } else {
        HeaderValue::from_static("application/octet-stream")
    };
    let total = bytes.len();
    let range = match parse_byte_range(headers.get(header::RANGE), total) {
        Ok(range) => range,
        Err(()) => return range_not_satisfiable(total),
    };
    let (status, bytes, content_range) = match range {
        Some((start, end)) => (
            StatusCode::PARTIAL_CONTENT,
            bytes[start..=end].to_vec(),
            Some(format!("bytes {start}-{end}/{total}")),
        ),
        None => (StatusCode::OK, bytes, None),
    };
    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, bytes.len().to_string());
    if let Some(content_range) = content_range {
        builder = builder.header(header::CONTENT_RANGE, content_range);
    }
    builder
        .body(Body::from(bytes))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// Parse one RFC 9110 byte range.
///
/// Only a single `bytes=` range is accepted. `bytes=start-`, `bytes=start-end`
/// and `bytes=-suffix` are supported; multi-range responses are deliberately
/// rejected because the content endpoint returns the stored object unchanged.
fn parse_byte_range(
    value: Option<&HeaderValue>,
    total: usize,
) -> Result<Option<(usize, usize)>, ()> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.to_str().map_err(|_| ())?;
    let spec = value.strip_prefix("bytes=").ok_or(())?;
    if spec.is_empty() || spec.contains(',') || total == 0 {
        return Err(());
    }
    let (start, end) = spec.split_once('-').ok_or(())?;
    if start.is_empty() {
        let suffix = end.parse::<usize>().map_err(|_| ())?;
        if suffix == 0 {
            return Err(());
        }
        return Ok(Some((total.saturating_sub(suffix), total - 1)));
    }
    let start = start.parse::<usize>().map_err(|_| ())?;
    if start >= total {
        return Err(());
    }
    let end = if end.is_empty() {
        total - 1
    } else {
        end.parse::<usize>().map_err(|_| ())?.min(total - 1)
    };
    if start > end {
        return Err(());
    }
    Ok(Some((start, end)))
}

fn range_not_satisfiable(total: usize) -> Response {
    Response::builder()
        .status(StatusCode::RANGE_NOT_SATISFIABLE)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_RANGE, format!("bytes */{total}"))
        .body(Body::empty())
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
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
    Extension(identity): Extension<Identity>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(response) = require_scope(&identity, Scope::Interact) {
        return response;
    }
    let Some(client_session_id) = get_session_id(&headers) else {
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
    let mut session_id_part: Option<String> = None;
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
            "session_id" => session_id_part = String::from_utf8(value.to_vec()).ok(),
            "media_type" => media_type = String::from_utf8(value.to_vec()).ok(),
            "content" => content = Some(value.to_vec()),
            _ => {}
        }
    }
    let Some(session_id) = session_id_part.filter(|s| !s.is_empty()) else {
        return (StatusCode::BAD_REQUEST, "missing session_id").into_response();
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
        .owns_session(&client_session_id, &session_id);
    if !owns {
        return (
            StatusCode::FORBIDDEN,
            [(header::CONTENT_TYPE, "application/json")],
            br#"{"code":"content_forbidden"}"#.to_vec(),
        )
            .into_response();
    }
    let content_size = content.len() as u64;
    let content_id = match state
        .hub
        .put_content(&session_id, &media_type, content, false)
    {
        Ok(content_id) => content_id,
        Err(error) => {
            return api_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "content_unavailable",
                &format!("content write failed: {error}"),
            );
        }
    };
    // `content_id` 即内容摘要；响应形状由 `RingingV2ContentUploadResponse` 锁定。
    let response = qaqh_ringing::RingingV2ContentUploadResponse {
        content_id: content_id.clone(),
        media_type,
        sha256: content_id,
        size: content_size,
        truncated: false,
    };
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_vec(&response).unwrap_or_default(),
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

    #[test]
    fn byte_ranges_are_single_and_inclusive() {
        let value = |raw: &str| HeaderValue::from_str(raw).expect("header");
        assert_eq!(parse_byte_range(None, 10), Ok(None));
        assert_eq!(
            parse_byte_range(Some(&value("bytes=2-5")), 10),
            Ok(Some((2, 5)))
        );
        assert_eq!(
            parse_byte_range(Some(&value("bytes=7-")), 10),
            Ok(Some((7, 9)))
        );
        assert_eq!(
            parse_byte_range(Some(&value("bytes=-3")), 10),
            Ok(Some((7, 9)))
        );
        assert_eq!(parse_byte_range(Some(&value("bytes=20-30")), 10), Err(()));
        assert_eq!(parse_byte_range(Some(&value("bytes=0-1,4-5")), 10), Err(()));
        assert_eq!(parse_byte_range(Some(&value("bytes=5-2")), 10), Err(()));
        assert_eq!(parse_byte_range(Some(&value("items=0-1")), 10), Err(()));
        assert_eq!(parse_byte_range(Some(&value("bytes=0-0")), 0), Err(()));
    }
}
