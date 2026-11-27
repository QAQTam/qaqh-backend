use std::collections::HashMap;
use std::sync::OnceLock;

use qaqh_core_vocab::SessionId;

static REMOVALS: OnceLock<std::sync::Mutex<HashMap<SessionId, tokio::sync::watch::Sender<bool>>>> =
    OnceLock::new();
static EPOCHS: OnceLock<std::sync::Mutex<HashMap<SessionId, u64>>> = OnceLock::new();

pub(crate) fn epoch(session_id: &SessionId, fallback: u64) -> u64 {
    EPOCHS
        .get()
        .and_then(|epochs| {
            epochs
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .get(session_id)
                .copied()
        })
        .unwrap_or(fallback)
}

pub(super) fn set_epoch(session_id: &SessionId, value: u64) {
    EPOCHS
        .get_or_init(|| std::sync::Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(session_id.clone(), value);
}

pub(super) struct RemovalGuard(SessionId);

impl RemovalGuard {
    pub(super) fn new(session_id: SessionId) -> Self {
        Self(session_id)
    }
}

impl Drop for RemovalGuard {
    fn drop(&mut self) {
        if let Some(removals) = REMOVALS.get() {
            removals
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .remove(&self.0);
        }
        if let Some(epochs) = EPOCHS.get() {
            epochs
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .remove(&self.0);
        }
    }
}

pub(crate) fn notify_removed(session_id: &SessionId) {
    let mut removals = REMOVALS
        .get_or_init(|| std::sync::Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if let Some(sender) = removals.remove(session_id) {
        let _ = sender.send_replace(true);
    }
}

pub(super) fn removal_receiver(session_id: &SessionId) -> tokio::sync::watch::Receiver<bool> {
    let mut removals = REMOVALS
        .get_or_init(|| std::sync::Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if let Some(sender) = removals.get(session_id) {
        return sender.subscribe();
    }
    let (sender, receiver) = tokio::sync::watch::channel(false);
    removals.insert(session_id.clone(), sender);
    receiver
}
