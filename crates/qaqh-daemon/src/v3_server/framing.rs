//! 异步帧读写（**只做 I/O，不做编码**）。
//!
//! 线缆形状与超限守卫全部来自 [`qaqh_ringing_v3`]（`u32_le(len) ++ postcard`、
//! 超限在分配前拒绝、一帧恰好一载荷）。这里唯一的职责是把那份**同步**编解码
//! 接到 `tokio::io` 上——所以本文件里既没有第二个帧格式，也没有第二套上限。
//!
//! 本文件与 `crates/qaqh-client/src/v3/framing.rs` 是同一段 20 行逻辑的两份实现：
//! 契约 crate 冻结为"零 `tokio`"，而服务端与客户端分处两个 crate，中间没有第三处
//! 可以放它的地方。**两份实现的互操作由六大不变量守卫持续证明**（真服务端 + 真客户端
//! + 真多路复用），任何一侧漂移都会当场全红，而不是"线上第一天才发现"。

use qaqh_ringing_v3::{decode_frame, encode_frame, peek_frame_len};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::error::FrameIoError;

/// 写一帧并 flush（**不 flush 就等于没发**：多路复用的对端看不到半帧）。
pub(crate) async fn write_frame<W, T>(writer: &mut W, payload: &T) -> Result<(), FrameIoError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let framed = encode_frame(payload)?;
    writer.write_all(&framed).await?;
    writer.flush().await?;
    Ok(())
}

/// 读一帧（长度前缀先到、载荷后到；超限在分配前拒绝）。
pub(crate) async fn read_frame<R, T>(reader: &mut R) -> Result<T, FrameIoError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut prefix = [0u8; 4];
    if let Err(error) = reader.read_exact(&mut prefix).await {
        return Err(if error.kind() == std::io::ErrorKind::UnexpectedEof {
            FrameIoError::Eof
        } else {
            FrameIoError::Io(error)
        });
    }
    let declared = peek_frame_len(&prefix)?;
    let mut framed = Vec::with_capacity(4 + declared);
    framed.extend_from_slice(&prefix);
    framed.resize(4 + declared, 0);
    if let Err(error) = reader.read_exact(&mut framed[4..]).await {
        return Err(if error.kind() == std::io::ErrorKind::UnexpectedEof {
            FrameIoError::Eof
        } else {
            FrameIoError::Io(error)
        });
    }
    decode_frame::<T>(&framed).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_ringing_v3::{DeliveryClass, FrameMeta, SessionEvent, SessionEventFrame};

    fn sample() -> SessionEventFrame {
        SessionEventFrame {
            meta: FrameMeta {
                seq: 7,
                server_ts_ms: 1_700_000_000_000,
                delivery: DeliveryClass::Reliable,
            },
            event: SessionEvent::CatalogChanged {
                session_id: qaqh_core_vocab::SessionId::new("session-a").expect("id"),
            },
        }
    }

    #[tokio::test]
    async fn frames_survive_a_real_duplex_roundtrip() {
        let (mut client, mut server) = tokio::io::duplex(4096);
        let frame = sample();
        write_frame(&mut client, &frame).await.expect("write");
        let decoded: SessionEventFrame = read_frame(&mut server).await.expect("read");
        assert_eq!(decoded, frame);
    }

    #[tokio::test]
    async fn closed_peer_reports_eof_not_a_decode_error() {
        let (client, mut server) = tokio::io::duplex(64);
        drop(client);
        let error = read_frame::<_, SessionEventFrame>(&mut server)
            .await
            .expect_err("必须报错");
        assert!(error.is_disconnect(), "断联必须被认成断联，实际：{error}");
    }
}
