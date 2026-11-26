//! Blob 门面：大输出/图片的**外置端**（`ADR-0042` 铁律 6："大输出一律外置"）。
//!
//! ## 方向是"服务端推"，不是"客户端拉"
//!
//! 契约里没有 blob 拉取请求，只有 `StreamKind::BlobTransfer` 与 `BlobChunkFrame`。
//! 所以这条流的方向是**服务端 → 客户端**：客户端从 `ContentRef::Blob{hash, len}`
//! 知道"有这么一坨、多大"，随后在 `BlobTransfer` 流上收到它。
//! 需要按需读大文件时的正确入口是 `Fs::ReadRange`（范围读），不是发明一条拉取协议
//! ——那会把已冻结的 P3-3 契约再撬一遍。
//!
//! ## 分块必须让出
//!
//! 一次 8 MiB 的推送如果"一口气写完"，同一条多路复用连接上的 PTY 回显与
//! 控制面 RTT 会被自己的队列顶到几十毫秒。每块之后 `yield_now()` 让出去，
//! 让 driver 有机会把其它流的帧插进来——这就是守卫②量化的那条隔离性的实现前提。

use std::sync::Arc;

use bytes::Bytes;
use qaqh_core_vocab::{ContentHash, ContentRef};
use qaqh_ringing_v3::{BlobChunkFrame, DeliveryClass, ErrorCode, FrameMeta, RpcError};
use tokio::io::AsyncWrite;

use super::super::error::rpc;
use super::super::framing::write_frame;
use super::super::state::ServerState;
use super::super::util::now_ms;

/// Blob 门面。
pub(crate) struct BlobFacade {
    state: Arc<ServerState>,
}

impl BlobFacade {
    /// 构造。
    pub(crate) fn new(state: Arc<ServerState>) -> Self {
        Self { state }
    }

    /// 把一段字节外置为 blob，返回引用（**调用方负责把推送请求投给连接层**）。
    pub(crate) fn spill(&self, bytes: Vec<u8>) -> ContentRef {
        let len = bytes.len() as u64;
        let hash = self.state.blobs.put(bytes);
        ContentRef::Blob { hash, len }
    }

    /// 按窗口分块推送一个 blob，返回实际推送字节数。
    pub(crate) async fn push<W>(
        &self,
        writer: &mut W,
        hash: ContentHash,
        expected_len: u64,
    ) -> Result<u64, RpcError>
    where
        W: AsyncWrite + Unpin,
    {
        let blob = self.state.blobs.get(&hash).ok_or_else(|| {
            rpc(
                ErrorCode::NotFound,
                format!("blob `{}` 不在库里", hash.to_hex()),
            )
        })?;
        if blob.len() as u64 != expected_len {
            // 长度对不上＝引用与内容不一致，绝不"按实际长度改口径"糊过去。
            return Err(rpc(
                ErrorCode::Internal,
                format!(
                    "blob `{}` 长度不符：引用 {expected_len}，实际 {}",
                    hash.to_hex(),
                    blob.len()
                ),
            ));
        }
        let chunk_bytes = self.state.config.blob_chunk_bytes.max(1);
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
            // 每块让出：Blob 是"能等的"，别把同连接的其它流挤在后面。
            tokio::task::yield_now().await;
        }
        Ok(offset as u64)
    }
}

#[cfg(test)]
mod smoke {
    use super::*;
    use crate::v3_server::facade::test_support;
    use crate::v3_server::framing::read_frame;

    #[tokio::test]
    async fn spill_then_push_reconstructs_the_exact_bytes() {
        let rig = test_support::rig();
        let payload: Vec<u8> = (0..200_000u32).map(|index| (index % 251) as u8).collect();
        let reference = rig.facades.blob.spill(payload.clone());
        let (hash, len) = match reference {
            ContentRef::Blob { hash, len } => (hash, len),
            other => panic!("大内容必须外置，实际 {other:?}"),
        };
        assert_eq!(len, payload.len() as u64);

        let (mut client, mut server) = tokio::io::duplex(64 * 1024);
        let pump = tokio::spawn(async move {
            rig.facades
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
        assert_eq!(pushed(pump).await, payload.len() as u64);
        assert_eq!(received, payload, "分片重组必须逐字节相等");
        assert!(chunks > 1, "200 KB 必须被切成多片，实际 {chunks} 片");
    }

    async fn pushed(handle: tokio::task::JoinHandle<u64>) -> u64 {
        handle.await.expect("推送任务")
    }
}
