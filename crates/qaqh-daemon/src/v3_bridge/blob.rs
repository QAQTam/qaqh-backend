use bytes::Bytes;
use qaqh_core_vocab::{ContentHash, SessionId};
use qaqh_ringing_v3::{BlobChunkFrame, DeliveryClass, ErrorCode, FrameMeta, RpcError};
use qaqh_v3_transport::{read_frame, write_frame};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite};

use super::RealBackend;
use crate::v3_server::identity::ConnectionIdentity;
use crate::v3_server::now_ms;

pub(crate) async fn can_fetch(
    backend: &RealBackend,
    identity: &ConnectionIdentity,
    hash: &ContentHash,
) -> bool {
    let owners = match backend.hub.content_sessions_async(&hash.to_hex()).await {
        Ok(owners) => owners,
        Err(error) => {
            log::error!("v3 blob owner lookup failed: {error}");
            return false;
        }
    };
    for owner in owners {
        match SessionId::new(&owner) {
            Ok(id) => {
                if backend
                    .ensure_session_runtime(&id)
                    .await
                    .ok()
                    .flatten()
                    .is_some_and(|session| session.can_access(&identity.device))
                {
                    return true;
                }
            }
            Err(error) => log::error!("invalid persisted blob owner `{owner}`: {error}"),
        }
    }
    false
}

pub(crate) async fn push<W: AsyncWrite + Unpin>(
    backend: &RealBackend,
    writer: &mut W,
    hash: ContentHash,
    expected_len: u64,
) -> Result<u64, RpcError> {
    let owners = backend
        .hub
        .content_sessions_async(&hash.to_hex())
        .await
        .map_err(|error| super::internal(format!("resolve blob owners: {error}")))?;
    let mut stored = None;
    for owner in owners {
        if let Some(entry) = backend
            .hub
            .get_content_async(&owner, &hash.to_hex())
            .await
            .map_err(|error| super::internal(format!("read blob: {error}")))?
        {
            stored = Some(entry);
            break;
        }
    }
    let stored = stored.ok_or_else(|| RpcError {
        code: ErrorCode::NotFound,
        message: format!("blob `{}` 不在库里", hash.to_hex()),
        retryable: false,
    })?;
    if stored.bytes.len() as u64 != expected_len {
        return Err(super::internal(format!(
            "blob `{}` 长度不符：引用 {expected_len}，实际 {}",
            hash.to_hex(),
            stored.bytes.len()
        )));
    }
    if stored.bytes.is_empty() {
        write_frame(
            writer,
            &BlobChunkFrame {
                meta: FrameMeta {
                    seq: 1,
                    server_ts_ms: now_ms(),
                    delivery: DeliveryClass::Reliable,
                },
                hash,
                offset: 0,
                bytes: Bytes::new(),
                last: true,
            },
        )
        .await
        .map_err(|error| super::internal(format!("write empty blob: {error}")))?;
        return Ok(0);
    }
    let chunk_bytes = backend.state.config.blob_chunk_bytes.max(1);
    let mut offset = 0usize;
    let mut seq = 0u64;
    while offset < stored.bytes.len() {
        let end = (offset + chunk_bytes).min(stored.bytes.len());
        seq += 1;
        write_frame(
            writer,
            &BlobChunkFrame {
                meta: FrameMeta {
                    seq,
                    server_ts_ms: now_ms(),
                    delivery: DeliveryClass::Reliable,
                },
                hash,
                offset: offset as u64,
                bytes: Bytes::copy_from_slice(&stored.bytes[offset..end]),
                last: end == stored.bytes.len(),
            },
        )
        .await
        .map_err(|error| super::internal(format!("write blob chunk: {error}")))?;
        offset = end;
        if seq.is_multiple_of(8) {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        } else {
            tokio::task::yield_now().await;
        }
    }
    Ok(offset as u64)
}

pub(crate) async fn receive_upload<R: AsyncRead + Unpin>(
    backend: &RealBackend,
    stream: &mut R,
    session_id: SessionId,
    expected_hash: ContentHash,
    expected_len: u64,
) -> bool {
    let mut bytes = Vec::with_capacity(expected_len as usize);
    loop {
        let Ok(frame) = read_frame::<_, BlobChunkFrame>(stream).await else {
            return false;
        };
        if frame.hash != expected_hash || frame.offset != bytes.len() as u64 {
            return false;
        }
        let next_len = (bytes.len() as u64).saturating_add(frame.bytes.len() as u64);
        if next_len > expected_len {
            return false;
        }
        bytes.extend_from_slice(&frame.bytes);
        if frame.last {
            break;
        }
    }
    if bytes.len() as u64 != expected_len {
        return false;
    }
    let actual: [u8; 32] = Sha256::digest(&bytes).into();
    if actual != *expected_hash.as_bytes() {
        return false;
    }
    match backend
        .hub
        .put_content_async(
            session_id.as_str(),
            "application/octet-stream",
            bytes,
            false,
        )
        .await
    {
        Ok(_) => true,
        Err(error) => {
            log::error!("v3 blob upload persistence failed: {error}");
            false
        }
    }
}
