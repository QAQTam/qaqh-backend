//! Direct streaming output for high-frequency model deltas.
//!
//! The renderer owns frame-level coalescing. Keeping the worker transport
//! immediate prevents hidden server-side latency at high token rates.
//!
//! M3 后：仅 Ringing 线格式（`emit_domain` / `emit_timeline`）；
//! legacy `emit` / `emit_delta`（Agent2Ui 路径）已完全拆除。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use qaqh_session::canonical::FactCausation;

use super::types::Emitter;
use super::types::WriterEvent;

pub struct PacedEmitter {
    /// 当前会话 seed（Ringing 事件信封路由键）。会话切换后经
    /// `set_seed` 更新；构造时快照的旧值在 resume 模式下为空，
    /// 必须由 Loop 在 init_session 后同步。
    session_id: Arc<Mutex<String>>,
    tx: mpsc::SyncSender<WriterEvent>,
    writer_dead: Arc<AtomicBool>,
    causation: FactCausation,
}

impl PacedEmitter {
    pub fn new(
        session_id: impl Into<String>,
        tx: mpsc::SyncSender<WriterEvent>,
        writer_dead: Arc<AtomicBool>,
    ) -> Self {
        Self {
            session_id: Arc::new(Mutex::new(session_id.into())),
            tx,
            writer_dead,
            causation: FactCausation::new(),
        }
    }

    /// 进入一个命令执行的作用域：期间 `emit_domain` 产出的事件携带
    /// `causation_id`。返回的 guard 在 Drop 时恢复上一个作用域（支持嵌套）。
    pub fn enter_causation(&self, causation: Option<&str>) -> CausationGuard {
        let previous = self.causation.current();
        self.causation.set(causation.map(str::to_string));
        CausationGuard {
            slot: self.causation.clone(),
            previous,
        }
    }

    /// The scope cell itself. The session actor's canonical ledger is bound to
    /// this same cell so the facts a dispatch appends carry the command id its
    /// events already carry — one scope, both lanes.
    pub fn causation_handle(&self) -> FactCausation {
        self.causation.clone()
    }

    /// 同步当前会话 seed。会话创建/恢复（含 auto-create、worker 内切换）
    /// 后调用，使 Ringing 事件信封携带正确的路由键。
    pub fn set_session(&self, session_id: &str) {
        let mut slot = self.session_id.lock().unwrap_or_else(|e| e.into_inner());
        *slot = session_id.to_string();
    }
}

/// 命令作用域 guard：Drop 时恢复进入前的 causation。
pub struct CausationGuard {
    slot: FactCausation,
    previous: Option<String>,
}

impl Drop for CausationGuard {
    fn drop(&mut self) {
        self.slot.set(self.previous.take());
    }
}

impl Emitter for PacedEmitter {
    fn emit_domain(&self, event: qaqh_domain::DomainEvent) {
        if self.writer_dead.load(Ordering::SeqCst) {
            return;
        }
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let session_id = self
            .session_id
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let causation = self.causation.current();
        let env = qaqh_ringing::RingingWorkerEventEnvelope::new(
            session_id.as_str(),
            format!("w-{seq}"),
            event.into(),
        );
        let env = match causation {
            Some(c) => env.with_causation(c),
            None => env,
        };
        let _ = self.tx.send(WriterEvent::Ringing(env));
    }

    fn emit_timeline(&self, intent: qaqh_domain::TimelineIntent) {
        if self.writer_dead.load(Ordering::SeqCst) {
            return;
        }
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let session_id = self
            .session_id
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let causation = self.causation.current();
        let env = qaqh_ringing::RingingTimelineIntentEnvelope::new(
            session_id.as_str(),
            format!("timeline-{seq}"),
            intent,
        );
        let env = match causation {
            Some(c) => env.with_causation(c),
            None => env,
        };
        let _ = self.tx.send(WriterEvent::Timeline(env));
    }

    fn event_tx(&self) -> Option<std::sync::mpsc::SyncSender<WriterEvent>> {
        Some(self.tx.clone())
    }
}

#[cfg(test)]
mod tests {
    //! The canonical fact writer is bound to this exact scope cell
    //! (`Loop::from_channels` → `AgentState::bind_fact_causation` →
    //! `ToolLedger::bind_causation`), which is what lets a command's facts fold
    //! its own receipt. If the scope ever diverges from the cell, receipts stop
    //! reaching a terminal state (2026-10-05 incident).

    use super::*;

    #[test]
    fn causation_handle_sees_every_scope_entry_and_restore() {
        let (tx, _rx) = mpsc::sync_channel::<WriterEvent>(4);
        let emitter = PacedEmitter::new("seed", tx, Arc::new(AtomicBool::new(false)));
        let cell = emitter.causation_handle();
        assert_eq!(cell.current(), None, "a fresh actor dispatches uncaused");

        let outer = emitter.enter_causation(Some("cmd-outer"));
        assert_eq!(cell.current().as_deref(), Some("cmd-outer"));
        {
            let inner = emitter.enter_causation(Some("cmd-inner"));
            assert_eq!(cell.current().as_deref(), Some("cmd-inner"));
            drop(inner);
        }
        assert_eq!(
            cell.current().as_deref(),
            Some("cmd-outer"),
            "a nested scope restores its predecessor"
        );
        drop(outer);
        assert_eq!(cell.current(), None, "the outermost scope clears");
    }
}
