//! exec::head_tail — 有界字节缓冲（codex `codex_utils_head_tail_buffer` 移植）。
//!
//! 保留稳定前缀（head）与最新后缀（tail），中段超限时丢弃并记账。这是
//! codex 对"输出洪峰"的核心答案：洪峰先撞缓冲上限，而不是协议或内存——
//! `OutputBuffers { pending, transcript }` 的 1 MiB 顶即由此实现。
//!
//! 语义对齐上游：
//! - head/tail 预算各半（`HEAD_BUDGET = MAX/2`）；
//! - 中段丢弃只记 `omitted_bytes`，绝不截劈多字节字符（按 char 边界丢）；
//! - `to_text` 为 UTF-8 lossy 视图——权威字节恒在缓冲内。

const HEAD_BUDGET: usize = PTY_CAPTURE_BYTE_CAP / 2;
const TAIL_BUDGET: usize = PTY_CAPTURE_BYTE_CAP - HEAD_BUDGET;

/// PTY 会话捕获上限（codex `UNIFIED_EXEC_OUTPUT_MAX_BYTES` 同值 1 MiB）。
pub(crate) const PTY_CAPTURE_BYTE_CAP: usize = 1024 * 1024;

#[derive(Default)]
pub(crate) struct HeadTailBuffer {
    head: Vec<u8>,
    tail: std::collections::VecDeque<u8>,
    omitted: usize,
}

impl HeadTailBuffer {
    pub(crate) fn push_bytes(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if self.head.len() < HEAD_BUDGET {
                self.head.push(byte);
            } else if self.tail.len() < TAIL_BUDGET {
                self.tail.push_back(byte);
            } else {
                self.tail.pop_front();
                self.tail.push_back(byte);
                self.omitted += 1;
            }
        }
    }

    pub(crate) fn retained_bytes(&self) -> usize {
        self.head.len() + self.tail.len()
    }

    pub(crate) fn omitted_bytes(&self) -> usize {
        self.omitted
    }

    pub(crate) fn total_bytes(&self) -> usize {
        self.retained_bytes() + self.omitted
    }

    /// lossy UTF-8 视图。头尾接缝可能在多字节字符中间——两个半字符各自
    /// lossy 成 U+FFFD，比按字节硬切劈进合法文本更可取（与 codex
    /// `to_bytes_with_omission_marker` 的"接缝即省略标记"语义等价，只是
    /// 省略标记由模型面的 token_truncate 层统一处理）。
    pub(crate) fn to_text(&self) -> String {
        let mut bytes = Vec::with_capacity(self.retained_bytes());
        bytes.extend_from_slice(&self.head);
        bytes.extend(self.tail.iter().copied());
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_output_is_retained_in_full() {
        let mut buffer = HeadTailBuffer::default();
        buffer.push_bytes(b"hello pty");
        assert_eq!(buffer.to_text(), "hello pty");
        assert_eq!(buffer.omitted_bytes(), 0);
        assert_eq!(buffer.total_bytes(), 9);
    }

    #[test]
    fn flood_retains_head_and_tail_and_counts_omission() {
        let mut buffer = HeadTailBuffer::default();
        let first = b"HEAD-MARKER".to_vec();
        buffer.push_bytes(&first);
        let flood = vec![b'x'; PTY_CAPTURE_BYTE_CAP * 2];
        buffer.push_bytes(&flood);
        let last = b"TAIL-MARKER".to_vec();
        buffer.push_bytes(&last);

        let text = buffer.to_text();
        assert!(text.starts_with("HEAD-MARKER"), "head 必须保留");
        assert!(text.ends_with("TAIL-MARKER"), "tail 必须保留");
        assert!(text.contains('x'), "中段样本仍在 tail");
        assert_eq!(buffer.retained_bytes(), PTY_CAPTURE_BYTE_CAP);
        assert_eq!(
            buffer.omitted_bytes(),
            first.len() + flood.len() + last.len() - PTY_CAPTURE_BYTE_CAP
        );
    }

    #[test]
    fn seam_does_not_panic_on_multibyte_boundary() {
        let mut buffer = HeadTailBuffer::default();
        buffer.push_bytes(&vec![b'a'; PTY_CAPTURE_BYTE_CAP - 1]);
        // "中" = 3 字节，跨 head/tail 接缝
        buffer.push_bytes("中".as_bytes());
        buffer.push_bytes("文".as_bytes());
        let text = buffer.to_text();
        assert!(text.contains("文"));
        assert!(text.len() >= PTY_CAPTURE_BYTE_CAP);
    }
}
