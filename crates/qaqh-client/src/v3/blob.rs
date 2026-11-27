//! Blob 接收端：服务端推来的 `BlobTransfer` 流在这里被组装并校验完整性。
//!
//! ## 组装完就定址校验（Fail-Closed）
//!
//! 每个分片都带 `hash` 与 `offset`。组装时：
//! - 限制单条 blob 内存上限（[`MAX_ASSEMBLED_BLOB_BYTES`]），防恶意超大流 OOM；
//! - 逐片校验 `offset` 连续且 `hash` 一致；
//! - 收完后核对 `sha256(&bytes) == hash`，任何字节篡改或错位当场丢弃，绝不投递。

use std::collections::HashMap;

use qaqh_core_vocab::ContentHash;
use qaqh_ringing_v3::{BlobChunkFrame, BlobTransferRole, StreamKind};
use qaqh_v3_transport::{MuxError, MuxTransport, read_frame};
use sha2::{Digest, Sha256};
use tokio::io::AsyncRead;
use tokio::sync::{mpsc, watch};

/// 接收端单条 blob 上限（128 MiB，防恶意服务端或损坏帧拖垮客户端内存）。
pub const MAX_ASSEMBLED_BLOB_BYTES: usize = 128 * 1024 * 1024;

/// 有界收件箱队列容量（提供背压）。
const BLOB_INBOX_CAPACITY: usize = 32;

/// 一条组装完成且通过哈希校验的 blob。
#[derive(Debug, Clone)]
pub struct AssembledBlob {
    /// 内容哈希（与 `ContentRef::Blob{hash}` 同源）。
    pub hash: ContentHash,
    /// 完整字节。
    pub bytes: Vec<u8>,
    /// 分片数。
    pub chunks: u32,
}

/// blob 收件箱。
pub struct BlobInbox {
    rx: mpsc::Receiver<AssembledBlob>,
    received: HashMap<String, usize>,
}

impl BlobInbox {
    /// 建一条有界收件通道（发送端给接受循环，收件箱留给调用方）。
    pub(crate) fn channel() -> (mpsc::Sender<AssembledBlob>, Self) {
        let (tx, rx) = mpsc::channel(BLOB_INBOX_CAPACITY);
        (
            tx,
            Self {
                rx,
                received: HashMap::new(),
            },
        )
    }

    /// 等下一坨已校验的 blob。
    pub async fn next(&mut self) -> Option<AssembledBlob> {
        let blob = self.rx.recv().await?;
        self.received.insert(blob.hash.to_hex(), blob.bytes.len());
        Some(blob)
    }

    /// 已收到的 blob 字节数（按 hex 哈希索引；守卫对数用）。
    pub fn received_bytes(&self) -> usize {
        self.received.values().sum()
    }
}

/// 常驻接受循环：服务端推一条 `BlobTransfer` 流，就组装并校验一条。
pub(crate) async fn accept_pushes<B: MuxTransport>(
    transport: B,
    sink: mpsc::Sender<AssembledBlob>,
    mut cancel: watch::Receiver<bool>,
) {
    loop {
        let accepted = tokio::select! {
            biased;
            _ = cancel.wait_for(|&flag| flag) => return,
            accepted = transport.accept_bi() => accepted,
        };
        let mut stream = match accepted {
            Ok(stream) => stream,
            // 连接关了：接受循环下班。
            Err(MuxError::Closed) => return,
            Err(error) => {
                log::warn!("v3-client: 接受 blob 流失败：{error}");
                return;
            }
        };
        let sink = sink.clone();
        tokio::spawn(async move {
            let Some(blob) = receive_pushed_blob(&mut stream).await else {
                return;
            };
            let _ = sink.send(blob).await;
        });
    }
}

/// 从单条流读出、组装并校验 `sha256`（独立函数便于单测）。
async fn receive_pushed_blob<R: AsyncRead + Unpin>(stream: &mut R) -> Option<AssembledBlob> {
    let Ok(StreamKind::BlobTransfer {
        role: BlobTransferRole::ServerPush,
    }) = read_frame::<_, StreamKind>(stream).await
    else {
        log::warn!("v3-client: 服务端推来了一条非 ServerPush BlobTransfer 流，已忽略");
        return None;
    };
    let mut bytes: Vec<u8> = Vec::new();
    let mut hash: Option<ContentHash> = None;
    let mut chunks = 0u32;
    loop {
        let frame: BlobChunkFrame = match read_frame(stream).await {
            Ok(frame) => frame,
            Err(error) => {
                log::warn!("v3-client: blob 流中断（{error}），丢弃半截内容");
                return None;
            }
        };
        if frame.offset != bytes.len() as u64 {
            log::warn!(
                "v3-client: blob 分片偏移断裂（期望 {}，收到 {}），丢弃该 blob",
                bytes.len(),
                frame.offset
            );
            return None;
        }
        if bytes.len().saturating_add(frame.bytes.len()) > MAX_ASSEMBLED_BLOB_BYTES {
            log::warn!("v3-client: blob 超过单条接收上限 {MAX_ASSEMBLED_BLOB_BYTES}，丢弃");
            return None;
        }
        if let Some(expected) = hash {
            if expected != frame.hash {
                log::warn!("v3-client: 同一条 blob 流里出现两个 hash，丢弃");
                return None;
            }
        } else {
            hash = Some(frame.hash);
        }
        bytes.extend_from_slice(&frame.bytes);
        chunks += 1;
        if frame.last {
            break;
        }
    }
    let hash = hash?;
    let actual_digest: [u8; 32] = Sha256::digest(&bytes).into();
    if actual_digest != *hash.as_bytes() {
        log::warn!(
            "v3-client: blob `{}` sha256 校验失败（内容篡改或损坏），丢弃",
            hash.to_hex()
        );
        return None;
    }
    Some(AssembledBlob {
        hash,
        bytes,
        chunks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use qaqh_ringing_v3::{DeliveryClass, FrameMeta};
    use qaqh_v3_transport::write_frame;

    #[tokio::test]
    async fn tampered_blob_payload_is_rejected_fail_closed() {
        let payload = b"hello verified blob".to_vec();
        let digest: [u8; 32] = Sha256::digest(&payload).into();
        let hash = ContentHash::from_bytes(digest);

        // 合法载荷 + 正确 sha256 → 接收成功。
        let (mut client, mut server) = tokio::io::duplex(4096);
        write_frame(
            &mut server,
            &StreamKind::BlobTransfer {
                role: BlobTransferRole::ServerPush,
            },
        )
        .await
        .expect("header");
        write_frame(
            &mut server,
            &BlobChunkFrame {
                meta: FrameMeta {
                    seq: 1,
                    server_ts_ms: 1,
                    delivery: DeliveryClass::Reliable,
                },
                hash,
                offset: 0,
                bytes: Bytes::from(payload.clone()),
                last: true,
            },
        )
        .await
        .expect("chunk");
        drop(server);
        let assembled = receive_pushed_blob(&mut client)
            .await
            .expect("合法 blob 必须通过校验");
        assert_eq!(assembled.bytes, payload);

        // 篡改一个字节（保留原 hash）→ 必须 Fail-Closed 丢弃。
        let (mut client, mut server) = tokio::io::duplex(4096);
        let mut bad = payload;
        bad[0] ^= 0xFF;
        write_frame(
            &mut server,
            &StreamKind::BlobTransfer {
                role: BlobTransferRole::ServerPush,
            },
        )
        .await
        .expect("header");
        write_frame(
            &mut server,
            &BlobChunkFrame {
                meta: FrameMeta {
                    seq: 1,
                    server_ts_ms: 1,
                    delivery: DeliveryClass::Reliable,
                },
                hash,
                offset: 0,
                bytes: Bytes::from(bad),
                last: true,
            },
        )
        .await
        .expect("chunk");
        drop(server);
        assert!(
            receive_pushed_blob(&mut client).await.is_none(),
            "sha256 不匹配的 blob 必须被拒绝"
        );
    }
}
