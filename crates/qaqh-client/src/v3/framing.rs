//! 异步帧读写（**只做 I/O，不做编码**）。
//!
//! 与 `crates/qaqh-daemon/src/v3_server/framing.rs` 是同一段逻辑的两份实现：
//! 契约 crate 冻结为"零 `tokio`"，而两端分处两个 crate，中间没有第三处可以放它。
//! **互操作由六大不变量守卫持续证明**（真服务端 + 真客户端 + 真多路复用），
//! 任何一侧漂移都会当场全红。

use qaqh_ringing_v3::{decode_frame, encode_frame, peek_frame_len};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::ClientError;

/// 帧 I/O 错误。
#[derive(Debug, thiserror::Error)]
pub enum FrameIoError {
    /// 对端正常关闭。
    #[error("对端已关闭流")]
    Eof,
    /// 帧形状非法。
    #[error("帧形状非法：{0}")]
    Frame(#[from] qaqh_ringing_v3::FrameError),
    /// 底层 I/O 失败。
    #[error("底层 I/O 失败：{0}")]
    Io(#[from] std::io::Error),
}

/// 写一帧并 flush。
pub(super) async fn write_frame<W, T>(writer: &mut W, payload: &T) -> Result<(), ClientError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let framed = encode_frame(payload)?;
    writer.write_all(&framed).await?;
    writer.flush().await?;
    Ok(())
}

/// 读一帧。
pub(super) async fn read_frame<R, T>(reader: &mut R) -> Result<T, FrameIoError>
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

impl From<qaqh_ringing_v3::FrameError> for ClientError {
    fn from(error: qaqh_ringing_v3::FrameError) -> Self {
        ClientError::Frame(FrameIoError::Frame(error))
    }
}

impl From<std::io::Error> for ClientError {
    fn from(error: std::io::Error) -> Self {
        ClientError::Frame(FrameIoError::Io(error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_ringing_v3::{DeliveryClass, FrameMeta, SessionEvent, SessionEventFrame};

    #[tokio::test]
    async fn frames_round_trip_over_a_real_duplex() {
        let (mut client, mut server) = tokio::io::duplex(4096);
        let frame = SessionEventFrame {
            meta: FrameMeta {
                seq: 1,
                server_ts_ms: 1,
                delivery: DeliveryClass::Reliable,
            },
            event: SessionEvent::CatalogChanged {
                session_id: qaqh_core_vocab::SessionId::new("s").expect("id"),
            },
        };
        write_frame(&mut client, &frame).await.expect("写");
        let back: SessionEventFrame = read_frame(&mut server).await.expect("读");
        assert_eq!(back, frame);
    }
}
