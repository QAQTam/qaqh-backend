//! 异步帧读写单源（`v3_server` 与 `client/v3` 共用，消除重复实现）。
//!
//! 线缆形状与超限守卫全部来自 [`qaqh_ringing_v3`]（`u32_le(len) ++ postcard`、
//! 声明超限在分配前拒绝、一帧恰好一载荷）。
//!
//! ## 半帧截断 vs 正常 EOF
//!
//! - 连接在**两帧之间**（已读 0 字节前缀时）关闭 → [`FrameIoError::Eof`]；
//! - 连接在 **4 字节长度前缀中间**或**载荷中间**关闭 →
//!   [`FrameIoError::Frame(FrameError::TruncatedPrefix / TruncatedPayload)`]，
//!   绝不把截断伪装成正常收摊。

use qaqh_ringing_v3::{FrameError, decode_frame, encode_frame, peek_frame_len};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// 帧 I/O 错误。
#[derive(Debug, thiserror::Error)]
pub enum FrameIoError {
    /// 对端在帧边界正常关闭。
    #[error("对端已关闭流")]
    Eof,
    /// 帧形状非法（超限/前缀截断/载荷截断/走私/解码失败）。
    #[error("帧形状非法：{0}")]
    Frame(#[from] FrameError),
    /// 底层 I/O 失败。
    #[error("底层 I/O 失败：{0}")]
    Io(#[from] std::io::Error),
}

impl FrameIoError {
    /// 是否只是对端断联（正常收摊、I/O 断开或半帧截断）。
    pub fn is_disconnect(&self) -> bool {
        matches!(
            self,
            Self::Eof
                | Self::Io(_)
                | Self::Frame(FrameError::TruncatedPrefix { .. } | FrameError::TruncatedPayload { .. })
        )
    }
}

/// 写一帧并 flush（不 flush 就等于没发：多路复用的对端看不到半帧）。
pub async fn write_frame<W, T>(writer: &mut W, payload: &T) -> Result<(), FrameIoError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let framed = encode_frame(payload)?;
    writer.write_all(&framed).await?;
    writer.flush().await?;
    Ok(())
}

/// 读一帧（精确区分整帧边界 EOF 与半帧截断，超限在分配前拒绝）。
pub async fn read_frame<R, T>(reader: &mut R) -> Result<T, FrameIoError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut prefix = [0u8; 4];
    let mut prefix_read = 0usize;
    while prefix_read < 4 {
        let n = reader.read(&mut prefix[prefix_read..]).await?;
        if n == 0 {
            if prefix_read == 0 {
                return Err(FrameIoError::Eof);
            }
            return Err(FrameIoError::Frame(FrameError::TruncatedPrefix {
                available: prefix_read,
            }));
        }
        prefix_read += n;
    }

    let declared = peek_frame_len(&prefix)?;
    let mut framed = Vec::with_capacity(4 + declared);
    framed.extend_from_slice(&prefix);
    framed.resize(4 + declared, 0);

    let mut payload_read = 0usize;
    while payload_read < declared {
        let n = reader.read(&mut framed[4 + payload_read..]).await?;
        if n == 0 {
            return Err(FrameIoError::Frame(FrameError::TruncatedPayload {
                declared,
                available: payload_read,
            }));
        }
        payload_read += n;
    }

    decode_frame::<T>(&framed).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_ringing_v3::{ControlCall, ControlRequest, SystemRequest};
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn clean_eof_reports_eof_and_half_frames_report_truncation() {
        let (client, mut server) = tokio::io::duplex(64);
        drop(client);
        assert!(matches!(
            read_frame::<_, u32>(&mut server).await,
            Err(FrameIoError::Eof)
        ));

        let (mut client, mut server) = tokio::io::duplex(64);
        client.write_all(&[4, 0]).await.expect("half prefix");
        drop(client);
        assert!(matches!(
            read_frame::<_, u32>(&mut server).await,
            Err(FrameIoError::Frame(FrameError::TruncatedPrefix {
                available: 2
            }))
        ));

        let (mut client, mut server) = tokio::io::duplex(64);
        client.write_all(&[10, 0, 0, 0, 1, 2]).await.expect("half payload");
        drop(client);
        assert!(matches!(
            read_frame::<_, u32>(&mut server).await,
            Err(FrameIoError::Frame(FrameError::TruncatedPayload {
                declared: 10,
                available: 2
            }))
        ));
    }

    #[tokio::test]
    async fn frame_survives_duplex_roundtrip() {
        let (mut client, mut server) = tokio::io::duplex(4096);
        let frame = ControlCall {
            command_id: "cmd-test".to_string(),
            request: ControlRequest::System(SystemRequest::Health),
        };
        write_frame(&mut client, &frame).await.expect("write");
        let back: ControlCall = read_frame(&mut server).await.expect("read");
        assert_eq!(back, frame);
    }
}
