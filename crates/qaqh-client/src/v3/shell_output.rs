//! Shell PTY 流（**原始字节，不做 UTF-8 假设**）。
//!
//! 终端模拟器要的是字节，不是字符串：颜色序列、光标控制、半截多字节字符都可能
//! 出现在任意一帧里。所以这条流只给 `Bytes` 与偏移，不解释。
//!
//! ## 偏移是权威水位
//!
//! 每个 `ShellOutputFrame` 都带 `start_offset`/`end_offset`。断线重连时用
//! `attach_shell(shell_id, from_offset)` 从上次看到的偏移续读；服务端若已把那一段
//! 回放历史丢掉，`attach.seq` 会明确给出**当前**水位——此时应当清屏重建，
//! 而不是把新旧字节拼在一起（拼出来的终端一定是坏的）。

use qaqh_ringing_v3::{ShellOutputFrame, StreamAttach};
use tokio::io::AsyncRead;

use super::ClientError;
use super::framing::{FrameIoError, read_frame};

/// Shell 流的体检数据。
#[derive(Debug, Default, Clone)]
pub struct ShellStreamStats {
    /// 收到的帧数。
    pub frames: u64,
    /// 收到的字节数。
    pub bytes: u64,
    /// 偏移断裂次数（`0` 表示字节连续）。
    pub gaps: u64,
    /// 已收到的最后偏移。
    pub last_offset: u64,
}

/// 一条 shell PTY 流。
pub struct ShellOutputStream<S> {
    stream: S,
    attach: StreamAttach,
    stats: ShellStreamStats,
    expected_offset: u64,
}

impl<S> ShellOutputStream<S> {
    pub(super) fn new(stream: S, attach: StreamAttach) -> Self {
        let stats = ShellStreamStats {
            frames: 0,
            bytes: 0,
            gaps: 0,
            last_offset: attach.seq,
        };
        Self {
            stream,
            attach,
            stats,
            expected_offset: 0,
        }
    }

    /// attach 点（`seq` = 服务端当前输出水位）。
    pub fn attach(&self) -> &StreamAttach {
        &self.attach
    }

    /// 体检数据。
    pub fn stats(&self) -> &ShellStreamStats {
        &self.stats
    }
}

impl<S> ShellOutputStream<S>
where
    S: AsyncRead + Unpin,
{
    /// 拿下一帧原始字节。
    pub async fn next_frame(&mut self) -> Result<ShellOutputFrame, ClientError> {
        let frame: ShellOutputFrame = match read_frame(&mut self.stream).await {
            Ok(frame) => frame,
            Err(FrameIoError::Eof) => {
                return Err(ClientError::Protocol("shell 流被服务端关闭".to_string()));
            }
            Err(error) => return Err(ClientError::Frame(error)),
        };
        if frame.start_offset > self.expected_offset {
            // 中间少了字节：绝不"接着拼"，只如实记一笔，由调用方决定是否清屏重建。
            self.stats.gaps += 1;
        }
        self.expected_offset = self.expected_offset.max(frame.end_offset);
        self.stats.frames += 1;
        self.stats.bytes += frame.bytes.len() as u64;
        self.stats.last_offset = frame.end_offset;
        Ok(frame)
    }
}
