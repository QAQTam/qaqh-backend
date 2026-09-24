//! axum_impl::content — see parent module docs.

use super::*;
use axum::body::Body;
use axum::http::HeaderValue;

/// `GET /ringing/v2/content/{content_id}`。
///
/// **不带 seed 参数**：先按 id 取条目，再用条目自己的 `seed` 校验调用方归属
/// （客户端可能同时 attach 多个 seed，所以不能反推）。未命中一律 404——不泄漏
/// 「存在但不属于你」。
pub(crate) async fn handle_content_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(content_id): Path<String>,
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
    // canonical `ContentRef` 用 `sha256:<hex>`（schema 校验强制），content store 的
    // 条目 id 是裸 hex（`sha256_hex`）；两种形态都接受，统一归一后查 store。
    let store_id = content_id
        .strip_prefix("sha256:")
        .unwrap_or(content_id.as_str());
    let Some(entry) = state.hub.get_content_any(store_id) else {
        return (StatusCode::NOT_FOUND, "content not found or expired").into_response();
    };
    let owns = state
        .leases
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .owns_seed(&session_id, &entry.seed);
    if !owns {
        return (
            StatusCode::FORBIDDEN,
            [(header::CONTENT_TYPE, "application/json")],
            br#"{"code":"content_forbidden","message":"content is not owned by this session"}"#
                .to_vec(),
        )
            .into_response();
    }
    // BUG-2026-09-13-03 双保险：历史上可能已入库非法 media_type（注入
    // 面修复前），直接拼响应头会让 axum TryInto<HeaderValue> 失败 →
    // panic（存储型 DoS）。出站前校验，非法回退 octet-stream。
    let content_type = if is_valid_media_type(&entry.media_type) {
        HeaderValue::from_str(&entry.media_type)
            .unwrap_or(HeaderValue::from_static("application/octet-stream"))
    } else {
        HeaderValue::from_static("application/octet-stream")
    };
    let total = entry.bytes.len();
    let range = match parse_byte_range(headers.get(header::RANGE), total) {
        Ok(range) => range,
        Err(()) => return range_not_satisfiable(total),
    };
    let (status, bytes, content_range) = match range {
        Some((start, end)) => (
            StatusCode::PARTIAL_CONTENT,
            entry.bytes[start..=end].to_vec(),
            Some(format!("bytes {start}-{end}/{total}")),
        ),
        None => (StatusCode::OK, entry.bytes, None),
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
