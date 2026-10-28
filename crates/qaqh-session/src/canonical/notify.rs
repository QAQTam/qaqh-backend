//! Process-global commit notifications for the canonical log.
//!
//! A small registry maps canonical session directories to a monotonic
//! generation counter plus a condvar. [`crate::canonical::CanonicalLog`]
//! bumps the generation right after the durable `events.commit.json` write;
//! `wait_agent`-style waiters block on the condvar instead of sleep-polling
//! the canonical log (which re-validated the whole committed prefix every
//! 25 ms).
//!
//! Lost-wakeup contract: a waiter captures the generation *after* its
//! baseline read of the committed prefix. Any commit that raced the baseline
//! either is already reflected in the read facts or bumps the generation past
//! the captured value, so [`wait_for_commit_change`] returns immediately.
//!
//! Memory: one entry (~100 B) per session directory that ever commits in this
//! process. Entries are intentionally never pruned — pruning would race with
//! concurrent waiters — so growth is bounded by session count, not by
//! history size.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard},
    time::Instant,
};

#[derive(Default)]
struct CommitSignal {
    generation: Mutex<u64>,
    changed: Condvar,
}

static COMMIT_SIGNALS: LazyLock<Mutex<HashMap<PathBuf, Arc<CommitSignal>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// A poisoned lock here must not turn an unrelated panic elsewhere in the
/// daemon into a waiter failure; the generation counter has no invariants to
/// violate, so recover the guard and continue.
fn registry() -> MutexGuard<'static, HashMap<PathBuf, Arc<CommitSignal>>> {
    COMMIT_SIGNALS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn signal_for(session_dir: &Path) -> Arc<CommitSignal> {
    let mut signals = registry();
    signals
        .entry(session_dir.to_path_buf())
        .or_insert_with(|| Arc::new(CommitSignal::default()))
        .clone()
}

fn lock_generation(signal: &CommitSignal) -> MutexGuard<'_, u64> {
    signal
        .generation
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Record that the durable commit marker for `session_dir` advanced.
///
/// Callers must invoke this only after the marker write is durable; waiters
/// treat a generation change as "re-read the committed prefix". The critical
/// section is a counter bump plus `notify_all`: no I/O and no join, so it is
/// safe to call while holding the canonical log's writer lock.
pub fn notify_commit(session_dir: &Path) {
    let signal = signal_for(session_dir);
    let mut generation = lock_generation(&signal);
    *generation = generation.wrapping_add(1);
    signal.changed.notify_all();
}

/// Current notification generation for `session_dir` (0 before the first
/// in-process commit notification).
pub fn commit_generation(session_dir: &Path) -> u64 {
    *lock_generation(&signal_for(session_dir))
}

/// Block until the generation for `session_dir` differs from
/// `observed_generation` or `deadline` passes.
///
/// Returns `true` when a commit notification is pending (including one that
/// raced the caller's baseline read), `false` on deadline. The condvar may
/// wake spuriously; the caller must re-read state after every `true` return.
pub fn wait_for_commit_change(
    session_dir: &Path,
    observed_generation: u64,
    deadline: Instant,
) -> bool {
    let signal = signal_for(session_dir);
    let mut generation = lock_generation(&signal);
    loop {
        if *generation != observed_generation {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        let (guard, _) = signal
            .changed
            .wait_timeout(generation, deadline.saturating_duration_since(now))
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        generation = guard;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn unique_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "qaqh-canonical-notify-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ))
    }

    #[test]
    fn generation_starts_at_zero_and_bumps_on_notify() {
        let dir = unique_dir("gen");
        assert_eq!(commit_generation(&dir), 0);
        notify_commit(&dir);
        assert_eq!(commit_generation(&dir), 1);
        notify_commit(&dir);
        assert_eq!(commit_generation(&dir), 2);
    }

    #[test]
    fn wait_returns_immediately_when_generation_already_advanced() {
        let dir = unique_dir("ahead");
        let observed = commit_generation(&dir);
        notify_commit(&dir);
        let started = Instant::now();
        assert!(wait_for_commit_change(
            &dir,
            observed,
            Instant::now() + Duration::from_secs(5)
        ));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "a pending generation change must not block"
        );
    }

    #[test]
    fn wait_blocks_until_notify_then_wakes() {
        let dir = unique_dir("wake");
        let observed = commit_generation(&dir);
        let waiter_dir = dir.clone();
        let waiter = std::thread::spawn(move || {
            wait_for_commit_change(
                &waiter_dir,
                observed,
                Instant::now() + Duration::from_secs(5),
            )
        });
        // Give the waiter a moment to register on the condvar; this only
        // checks liveness, not exact ordering.
        std::thread::sleep(Duration::from_millis(50));
        let started = Instant::now();
        notify_commit(&dir);
        assert!(waiter.join().expect("waiter must not panic"));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "notify_commit must wake a blocked waiter promptly"
        );
    }

    #[test]
    fn wait_expires_at_deadline_without_commit() {
        let dir = unique_dir("deadline");
        let observed = commit_generation(&dir);
        let started = Instant::now();
        assert!(!wait_for_commit_change(
            &dir,
            observed,
            Instant::now() + Duration::from_millis(120)
        ));
        assert!(started.elapsed() >= Duration::from_millis(100));
    }
}
