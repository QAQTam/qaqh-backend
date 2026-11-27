use base64::Engine as _;
use qaqh_core_vocab::{ContentHash, ContentRef, SessionId};
use qaqh_ringing_runtime::RingingHub;
use qaqh_ringing_v3::ImageRefView;

use crate::v3_server::facade::BlobPush;
use crate::v3_server::state::ServerState;

pub(crate) async fn json_value(
    hub: &RingingHub,
    state: &ServerState,
    session_id: &str,
    value: &serde_json::Value,
) -> Result<ContentRef, String> {
    let text = serde_json::to_string(value).map_err(|error| error.to_string())?;
    inline(hub, state, session_id, text).await
}

pub(crate) async fn image_from_b64(
    hub: &RingingHub,
    session_id: &str,
    b64: &str,
    mime_type: String,
) -> Result<ImageRefView, String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|error| format!("decode image base64: {error}"))?;
    let len = bytes.len() as u64;
    let hash = hub
        .put_content_async(session_id, &mime_type, bytes, false)
        .await
        .map_err(|error| error.to_string())?;
    let hash = ContentHash::from_hex(&hash).map_err(|error| error.to_string())?;
    Ok(ImageRefView {
        hash,
        len,
        mime_type,
    })
}

pub(crate) async fn content_ref(
    hub: &RingingHub,
    state: &ServerState,
    session_id: &str,
    value: qaqh_ringing_contract::fact::ContentValue,
) -> Result<ContentRef, String> {
    match value {
        qaqh_ringing_contract::fact::ContentValue::Inline { text } => {
            inline(hub, state, session_id, text).await
        }
        qaqh_ringing_contract::fact::ContentValue::Ref { content_ref } => {
            let hash_str = content_ref.hash().as_str();
            let bare = hash_str.strip_prefix("sha256:").unwrap_or(hash_str);
            let entry = hub
                .get_content_async(session_id, bare)
                .await
                .map_err(|error| error.to_string())?
                .ok_or_else(|| format!("content ref missing: {hash_str}"))?;
            let hash = ContentHash::from_hex(bare).map_err(|error| error.to_string())?;
            let len = entry.bytes.len() as u64;
            if !entry.truncated
                && entry.bytes.len() <= state.config.inline_budget_bytes
                && is_text_media_type(&entry.media_type)
                && let Ok(text) = String::from_utf8(entry.bytes)
            {
                return Ok(ContentRef::Inline { text });
            }
            Ok(ContentRef::Blob { hash, len })
        }
        qaqh_ringing_contract::fact::ContentValue::Unavailable(reason) => {
            Ok(ContentRef::Inline {
                text: format!("[content unavailable: {reason:?}]"),
            })
        }
    }
}

fn is_text_media_type(media_type: &str) -> bool {
    let base = media_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    base.is_empty()
        || base.starts_with("text/")
        || matches!(
            base.as_str(),
            "application/json" | "application/xml" | "application/javascript"
        )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use qaqh_ringing_contract::fact::{
        ContentHash as OldHash, ContentRef as OldRef, ContentUnavailable, ContentUnavailableReason,
        ContentValue,
    };

    use super::*;
    use crate::v3_server::state::ServerConfig;

    #[tokio::test]
    async fn content_ref_handles_binary_large_truncated_and_unavailable_values() {
        let temp = tempfile::tempdir().unwrap();
        let db = std::sync::Arc::new(qaqh_session::db::Db::open(temp.path().join("q.db")).unwrap());
        db.write(|conn| qaqh_session::db::store::ensure_session(conn, "s1", None, false))
            .await
            .unwrap();
        let hub = RingingHub::with_database("e", db);
        let state = ServerState::new(
            ServerConfig {
                inline_budget_bytes: 16,
                ..Default::default()
            },
            "user",
        );

        let png = vec![0x89, b'P', b'N', b'G', 0xff, 0x00];
        let png_hex = hub.put_content_async("s1", "image/png", png.clone(), false).await.unwrap();
        let png_ref = content_ref(
            &hub,
            &state,
            "s1",
            ContentValue::Ref {
                content_ref: OldRef::new(OldHash::new(format!("sha256:{png_hex}"))),
            },
        )
        .await
        .unwrap();
        assert!(matches!(png_ref, ContentRef::Blob { len, .. } if len == png.len() as u64));

        let big = b"0123456789abcdef0123456789".to_vec();
        let big_hex = hub.put_content_async("s1", "text/plain", big.clone(), false).await.unwrap();
        let mut pushes = state.push_bus.subscribe();
        let big_ref = content_ref(
            &hub,
            &state,
            "s1",
            ContentValue::Ref {
                content_ref: OldRef::new(OldHash::new(format!("sha256:{big_hex}"))),
            },
        )
        .await
        .unwrap();
        assert!(matches!(big_ref, ContentRef::Blob { len, .. } if len == big.len() as u64));
        assert!(pushes.try_recv().is_err(), "existing blob must not re-push");

        let trunc_hex = hub.put_content_async("s1", "text/plain", b"abc".to_vec(), true).await.unwrap();
        let trunc_ref = content_ref(
            &hub,
            &state,
            "s1",
            ContentValue::Ref {
                content_ref: OldRef::new(OldHash::new(format!("sha256:{trunc_hex}"))),
            },
        )
        .await
        .unwrap();
        assert!(matches!(trunc_ref, ContentRef::Blob { len: 3, .. }));

        let unavailable = content_ref(
            &hub,
            &state,
            "s1",
            ContentValue::Unavailable(ContentUnavailable {
                content_ref: OldRef::new(OldHash::new(format!("sha256:{trunc_hex}"))),
                reason: ContentUnavailableReason::GarbageCollected,
                observed_at_logical_ms: 1,
                source_fact_seq: None,
                gc_event_seq: None,
            }),
        )
        .await
        .unwrap();
        assert!(matches!(unavailable, ContentRef::Inline { .. }));
    }
}

async fn inline(
    hub: &RingingHub,
    state: &ServerState,
    session_id: &str,
    text: String,
) -> Result<ContentRef, String> {
    if text.len() <= state.config.inline_budget_bytes {
        return Ok(ContentRef::Inline { text });
    }
    let len = text.len() as u64;
    let hash = hub
        .put_content_async(
            session_id,
            "text/plain; charset=utf-8",
            text.into_bytes(),
            false,
        )
        .await
        .map_err(|error| error.to_string())?;
    let hash = ContentHash::from_hex(&hash).map_err(|error| error.to_string())?;
    let _ = state.push_bus.send(BlobPush {
        session_id: SessionId::new(session_id).map_err(|error| error.to_string())?,
        hash,
        len,
    });
    Ok(ContentRef::Blob { hash, len })
}
