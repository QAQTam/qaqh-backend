//! Blob 接收端：服务端推来的 `BlobTransfer` 流在这里被组装成完整字节。
//!
//! ## 为什么客户端要自己开一个接受循环
//!
//! `BlobTransfer` 是**服务端开流**（服务端知道有什么要推）。客户端的多路复用
//! 接受侧因此必须常驻一个循环：握手完成后就等着服务端推流，收到一条就起一个
//! 组装任务。不常驻的话，服务端推第一条流时就会因为"没人接"而卡在开流上。
//!
//! ## 组装完就定址校验
//!
//! 每个分片都带 `hash` 与 `offset`。组装时**逐片校验 offset 连续**，收完再把
//! 结果交给内容哈希的下游使用者（守卫会拿它和 `ContentRef::Blob.hash` 对账）。
//! offset 断了就是断了：不拼接、不跳洞，直接丢弃这一条并记一笔。

use std::collections::HashMap;

use qaqh_core_vocab::ContentHash;
use qaqh_ringing_v3::{BlobChunkFrame, StreamKind};
use qaqh_v3_transport::{MuxTransport, MuxError};
use tokio::sync::{mpsc, watch};

use super::framing::read_frame;

/// 一条组装完成的 blob。
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
    rx: mpsc::UnboundedReceiver<AssembledBlob>,
    received: HashMap<String, usize>,
}

impl BlobInbox {
    /// 建一条收件通道（发送端给接受循环，收件箱留给调用方）。
    pub(crate) fn channel() -> (mpsc::UnboundedSender<AssembledBlob>, Self) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            tx,
            Self {
                rx,
                received: HashMap::new(),
            },
        )
    }

    /// 等下一坨 blob。
    pub async fn next(&mut self) -> Option<AssembledBlob> {
        let blob = self.rx.recv().await?;
        self.received
            .insert(blob.hash.to_hex(), blob.bytes.len());
        Some(blob)
    }

    /// 已收到的 blob 字节数（按 hex 哈希索引；守卫对数用）。
    pub fn received_bytes(&self) -> usize {
        self.received.values().sum()
    }
}

/// 常驻接受循环：服务端推一条 `BlobTransfer` 流，就组装一条。
pub(crate) async fn accept_pushes<B: MuxTransport>(
    transport: B,
    sink: mpsc::UnboundedSender<AssembledBlob>,
    mut cancel: watch::Receiver<bool>,
) {
    loop {
        let accepted = tokio::select! {
            biased;
            _ = cancel.changed() => return,
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
            let Ok(StreamKind::BlobTransfer) = read_frame::<_, StreamKind>(&mut stream).await else {
                log::warn!("v3-client: 服务端推来了一条非 BlobTransfer 流，已忽略");
                return;
            };
            let mut bytes: Vec<u8> = Vec::new();
            let mut hash = None;
            let mut chunks = 0u32;
            loop {
                let frame: BlobChunkFrame = match read_frame(&mut stream).await {
                    Ok(frame) => frame,
                    Err(error) => {
                        log::warn!("v3-client: blob 流中断（{error}），丢弃半截内容");
                        return;
                    }
                };
                if frame.offset != bytes.len() as u64 {
                    log::warn!(
                        "v3-client: blob 分片偏移断裂（期望 {}，收到 {}），丢弃该 blob",
                        bytes.len(),
                        frame.offset
                    );
                    return;
                }
                if let Some(expected) = hash {
                    if expected != frame.hash {
                        log::warn!("v3-client: 同一条 blob 流里出现两个 hash，丢弃");
                        return;
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
            let Some(hash) = hash else {
                return;
            };
            let _ = sink.send(AssembledBlob {
                hash,
                bytes,
                chunks,
            });
        });
    }
}
