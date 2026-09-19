//! 进度模型（base spec §9 + 09-19 补充稿 §4.5）。
//!
//! 进度是**临时展示数据**：有界、可丢弃、不承载终态信息；最终 outcome 不得
//! 依赖进度帧才能重建。终态只经 [`super::output::ToolOutcome`]。

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;

use super::output::ToolContentBlock;

/// 进度流标识（base spec §9.2：exec 的 stdout/stderr）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressStream {
    /// 标准输出。
    Stdout,
    /// 标准错误。
    Stderr,
}

/// 进度帧（base spec §9.1）。每帧必须有界。
#[derive(Debug, Clone, PartialEq)]
pub enum ToolProgress {
    /// 流式文本（exec 的 stdout/stderr 走此变体）。
    Text {
        /// 流标识。
        stream: ProgressStream,
        /// 本帧文本（有界）。
        text: String,
    },
    /// 阶段提示（如 "正在解析…"）。
    Phase {
        /// 阶段标识。
        phase: String,
        /// 人类可读消息。
        message: String,
    },
    /// 内容块（图片/资源）。
    Content {
        /// 内容块集合。
        blocks: Vec<ToolContentBlock>,
    },
    /// 工具自定义子类型（schema 须先登记，客户端可忽略）。
    Custom {
        /// 子类型标识。
        subkind: String,
        /// 载荷。
        payload: serde_json::Value,
    },
}

/// 有界进度接收端（宿主构造，工具侧只写）。
///
/// - 队列满或接收端关闭时**丢弃本帧并计数**（进度可丢弃）；
/// - 克隆共享同一丢弃计数。
#[derive(Clone, Debug)]
pub struct ProgressSink {
    tx: mpsc::SyncSender<ToolProgress>,
    dropped: Arc<AtomicU64>,
}

impl ProgressSink {
    /// 创建有界通道（`capacity` 至少为 1）。
    pub fn channel(capacity: usize) -> (Self, mpsc::Receiver<ToolProgress>) {
        let (tx, rx) = mpsc::sync_channel(capacity.max(1));
        (
            Self {
                tx,
                dropped: Arc::new(AtomicU64::new(0)),
            },
            rx,
        )
    }

    /// 非阻塞发送；丢弃时计数（不阻塞工具执行）。
    pub fn emit(&self, progress: ToolProgress) {
        if self.tx.try_send(progress).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// 被丢弃帧数（诊断用）。
    pub fn dropped_frames(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_in_order_until_capacity() {
        let (sink, rx) = ProgressSink::channel(2);
        sink.emit(ToolProgress::Phase {
            phase: "a".into(),
            message: "1".into(),
        });
        sink.emit(ToolProgress::Phase {
            phase: "b".into(),
            message: "2".into(),
        });
        assert_eq!(sink.dropped_frames(), 0);
        let first = rx.recv().expect("first frame");
        assert!(matches!(first, ToolProgress::Phase { .. }));
    }

    #[test]
    fn drops_and_counts_when_full_or_closed() {
        let (sink, rx) = ProgressSink::channel(1);
        sink.emit(ToolProgress::Text {
            stream: ProgressStream::Stdout,
            text: "1".into(),
        });
        sink.emit(ToolProgress::Text {
            stream: ProgressStream::Stdout,
            text: "2".into(),
        });
        assert_eq!(sink.dropped_frames(), 1, "队列满时丢弃并计数");

        drop(rx);
        let (sink2, rx2) = ProgressSink::channel(1);
        drop(rx2);
        sink2.emit(ToolProgress::Text {
            stream: ProgressStream::Stderr,
            text: "x".into(),
        });
        assert_eq!(sink2.dropped_frames(), 1, "接收端关闭时丢弃并计数");
    }

    #[test]
    fn clones_share_drop_counter() {
        let (sink, _rx) = ProgressSink::channel(1);
        let clone = sink.clone();
        clone.emit(ToolProgress::Text {
            stream: ProgressStream::Stdout,
            text: "1".into(),
        });
        clone.emit(ToolProgress::Text {
            stream: ProgressStream::Stdout,
            text: "2".into(),
        });
        assert_eq!(sink.dropped_frames(), 1, "克隆共享同一计数");
    }
}
