//! Background compaction task state.
//!
//! P2-4c-a moves the pending compact channel/id/causation out of `Loop`.
//! The port owns the task handle; the loop remains responsible for applying a
//! completed result at a safe point.

use std::sync::mpsc;

use super::engine_compact::CompactMeta;

pub(crate) struct PendingCompaction {
    pub(crate) compact_id: String,
    pub(crate) causation: Option<String>,
    rx: mpsc::Receiver<CompactMeta>,
}

pub(crate) enum CompactionPoll {
    Empty,
    Running,
    Ready(CompactMeta),
    Disconnected,
}

#[derive(Default)]
pub(crate) struct CompactionPort {
    pending: Option<PendingCompaction>,
}

impl CompactionPort {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn is_running(&self) -> bool {
        self.pending.is_some()
    }

    pub(crate) fn install(
        &mut self,
        rx: mpsc::Receiver<CompactMeta>,
        compact_id: String,
        causation: Option<String>,
    ) {
        self.pending = Some(PendingCompaction {
            compact_id,
            causation,
            rx,
        });
    }

    pub(crate) fn poll(&self) -> CompactionPoll {
        let Some(pending) = self.pending.as_ref() else {
            return CompactionPoll::Empty;
        };
        match pending.rx.try_recv() {
            Ok(meta) => CompactionPoll::Ready(meta),
            Err(mpsc::TryRecvError::Empty) => CompactionPoll::Running,
            Err(mpsc::TryRecvError::Disconnected) => CompactionPoll::Disconnected,
        }
    }

    pub(crate) fn take(&mut self) -> Option<PendingCompaction> {
        self.pending.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(compact_id: &str) -> CompactMeta {
        CompactMeta {
            compact_id: compact_id.to_string(),
            summary: "summary".to_string(),
            kept_user_count: 1,
            head_user_count: 1,
            context_revision: 1,
            error: None,
        }
    }

    #[test]
    fn empty_port_has_no_pending_task() {
        let mut port = CompactionPort::new();
        assert!(!port.is_running());
        assert!(matches!(port.poll(), CompactionPoll::Empty));
        assert!(port.take().is_none());
    }

    #[test]
    fn port_reports_running_ready_and_disconnected_states() {
        let (tx, rx) = mpsc::channel();
        let mut port = CompactionPort::new();
        port.install(rx, "compact-1".to_string(), Some("cmd-1".to_string()));

        assert!(port.is_running());
        assert!(matches!(port.poll(), CompactionPoll::Running));

        tx.send(meta("compact-1")).expect("send compact result");
        let CompactionPoll::Ready(result) = port.poll() else {
            panic!("expected ready compaction result");
        };
        assert_eq!(result.compact_id, "compact-1");
        let pending = port.take().expect("pending task");
        assert_eq!(pending.compact_id, "compact-1");
        assert_eq!(pending.causation.as_deref(), Some("cmd-1"));
        assert!(!port.is_running());

        let (tx, rx) = mpsc::channel();
        port.install(rx, "compact-2".to_string(), None);
        drop(tx);
        assert!(matches!(port.poll(), CompactionPoll::Disconnected));
        assert_eq!(
            port.take().expect("pending disconnected task").compact_id,
            "compact-2"
        );
    }
}
