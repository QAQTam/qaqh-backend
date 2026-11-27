//! Blob 门面：大输出/图片的**外置端**（`ADR-0042` 铁律 6："大输出一律外置"）。
//!
//! ## 分块必须让出
//!
//! 一次 8 MiB 的推送如果"一口气写完"，同一条多路复用连接上的 PTY 回显与
//! 控制面 RTT 会被自己的队列顶到几十毫秒。每块之后 `yield_now()` 让出去，
//! 让 driver 有机会把其它流的帧插进来。

use std::sync::Arc;

use bytes::Bytes;
use qaqh_core_vocab::ContentHash;
#[cfg(test)]
use qaqh_core_vocab::ContentRef;
use qaqh_ringing_v3::{BlobChunkFrame, DeliveryClass, ErrorCode, FrameMeta, RpcError};
use qaqh_v3_transport::write_frame;
use tokio::io::AsyncWrite;

use super::super::error::rpc;
use super::super::state::ServerState;
use super::super::util::now_ms;

/// Blob 门面骨架实现。
pub(crate) struct SkeletonBlob {
    state: Arc<ServerState>,
}

impl SkeletonBlob {
    /// 构造。
    pub(crate) fn new(state: Arc<ServerState>) -> Self {
        Self { state }
    }

    /// 把一段字节外置为 blob，返回引用。
    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn spill(&self, bytes: Vec<u8>) -> ContentRef {
        let len = bytes.len() as u64;
        let hash = self.state.blobs.put(bytes);
        ContentRef::Blob { hash, len }
    }

    /// 按窗口分块推送一个 blob（含 0 字节显式尾帧），返回实际推送字节数。
    pub(crate) async fn push<W>(
        &self,
        writer: &mut W,
        hash: ContentHash,
        expected_len: u64,
    ) -> Result<u64, RpcError>
    where
        W: AsyncWrite + Unpin,
    {
        push_blob(&self.state, writer, hash, expected_len).await
    }
}

pub(crate) async fn push_blob<W>(
    state: &ServerState,
    writer: &mut W,
    hash: ContentHash,
    expected_len: u64,
) -> Result<u64, RpcError>
where
    W: AsyncWrite + Unpin,
{
    let blob = state.blobs.get(&hash).ok_or_else(|| {
        rpc(
            ErrorCode::NotFound,
            format!("blob `{}` 不在库里", hash.to_hex()),
        )
    })?;
    if blob.len() as u64 != expected_len {
        return Err(rpc(
            ErrorCode::Internal,
            format!(
                "blob `{}` 长度不符：引用 {expected_len}，实际 {}",
                hash.to_hex(),
                blob.len()
            ),
        ));
    }
    if blob.is_empty() {
        let frame = BlobChunkFrame {
            meta: FrameMeta {
                seq: 1,
                server_ts_ms: now_ms(),
                delivery: DeliveryClass::Reliable,
            },
            hash,
            offset: 0,
            bytes: Bytes::new(),
            last: true,
        };
        write_frame(writer, &frame).await.map_err(|error| {
            rpc(
                ErrorCode::Unavailable,
                format!("BlobTransfer 流写失败：{error}"),
            )
        })?;
        return Ok(0);
    }
    let chunk_bytes = state.config.blob_chunk_bytes.max(1);
    let mut offset = 0usize;
    let mut seq = 0u64;
    while offset < blob.len() {
        let end = (offset + chunk_bytes).min(blob.len());
        seq += 1;
        let last = end == blob.len();
        let frame = BlobChunkFrame {
            meta: FrameMeta {
                seq,
                server_ts_ms: now_ms(),
                delivery: DeliveryClass::Reliable,
            },
            hash,
            offset: offset as u64,
            bytes: Bytes::copy_from_slice(&blob[offset..end]),
            last,
        };
        write_frame(writer, &frame).await.map_err(|error| {
            rpc(
                ErrorCode::Unavailable,
                format!("BlobTransfer 流写失败：{error}"),
            )
        })?;
        offset = end;
        // 每块让出、每 256 KiB 喘息 1ms：Blob 是"能等的"，避免占满发送窗口把控制面/PTY 挤在后面。
        if seq.is_multiple_of(4) {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        } else {
            tokio::task::yield_now().await;
        }
    }
    Ok(offset as u64)
}

#[cfg(test)]
mod smoke {
    use super::*;
    use crate::v3_server::facade::test_support;
    use qaqh_v3_transport::read_frame;

    #[tokio::test]
    async fn spill_then_push_reconstructs_the_exact_bytes() {
        let rig = test_support::rig();
        let payload: Vec<u8> = (0..200_000u32).map(|index| (index % 251) as u8).collect();
        let reference = rig.backend.blob.spill(payload.clone());
        let (hash, len) = match reference {
            ContentRef::Blob { hash, len } => (hash, len),
            other => panic!("大内容必须外置，实际 {other:?}"),
        };
        assert_eq!(len, payload.len() as u64);

        let (mut client, mut server) = tokio::io::duplex(64 * 1024);
        let pump = tokio::spawn(async move {
            rig.backend
                .blob
                .push(&mut server, hash, len)
                .await
                .expect("推送")
        });

        let mut received = Vec::new();
        let mut chunks = 0u32;
        loop {
            let frame: BlobChunkFrame = read_frame(&mut client).await.expect("读分片");
            assert_eq!(frame.hash, hash);
            assert_eq!(frame.offset, received.len() as u64);
            received.extend_from_slice(&frame.bytes);
            chunks += 1;
            if frame.last {
                break;
            }
        }
        assert_eq!(pump.await.expect("推送任务"), payload.len() as u64);
        assert_eq!(received, payload, "分片重组必须逐字节相等");
        assert!(chunks > 1, "200 KB 必须被切成多片，实际 {chunks} 片");
    }

    #[tokio::test]
    async fn empty_blob_emits_explicit_terminal_frame() {
        let rig = test_support::rig();
        let reference = rig.backend.blob.spill(Vec::new());
        let (hash, len) = match reference {
            ContentRef::Blob { hash, len } => (hash, len),
            other => panic!("意外引用：{other:?}"),
        };
        assert_eq!(len, 0);
        let (mut client, mut server) = tokio::io::duplex(4096);
        let pushed = rig
            .backend
            .blob
            .push(&mut server, hash, 0)
            .await
            .expect("0 字节推送");
        assert_eq!(pushed, 0);
        let frame: BlobChunkFrame = read_frame(&mut client).await.expect("读尾帧");
        assert!(frame.last);
        assert!(frame.bytes.is_empty());
    }
}
