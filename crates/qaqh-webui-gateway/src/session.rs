//! Gateway-owned browser sessions and daemon leases.
//!
//! Browser sessions are opaque values stored server-side. The browser never
//! receives a daemon bearer token or `client_session_id`; those stay inside the
//! gateway process.

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::watch;

use crate::approval::{ApprovalChallenge, ApprovalKind, MAX_PENDING_APPROVALS};

pub const NONCE_TTL: Duration = Duration::from_secs(60);
pub const SESSION_IDLE_TTL: Duration = Duration::from_secs(30 * 60);
pub const MAX_SESSIONS: usize = 8;

const MAX_PENDING_NONCES: usize = 256;
const MAX_PENDING_PER_IP: usize = 32;
const MAX_NONCE_ISSUES_PER_MINUTE: usize = 30;
const MAX_NONCE_REDEEMS_PER_MINUTE: usize = 10;
const MAX_COMMANDS_PER_MINUTE: usize = 120;
const MAX_SERVICE_CALLS_PER_MINUTE: usize = 240;
const RATE_WINDOW: Duration = Duration::from_secs(60);

/// A daemon lease owned by one browser session.
#[derive(Debug, Clone)]
pub struct Lease {
    pub client_instance_id: String,
    pub client_session_id: String,
    pub server_epoch: String,
    pub ttl_ms: u64,
    pub renew_interval_ms: u64,
    pub last_renew: Instant,
}

impl Lease {
    pub fn new(
        client_instance_id: String,
        client_session_id: String,
        server_epoch: String,
        ttl_ms: u64,
        renew_interval_ms: u64,
    ) -> Self {
        Self {
            client_instance_id,
            client_session_id,
            server_epoch,
            ttl_ms,
            renew_interval_ms,
            last_renew: Instant::now(),
        }
    }
}

struct NonceRecord {
    issued_at: Instant,
    ip: IpAddr,
}

#[derive(Default)]
struct NonceInner {
    pending: HashMap<String, NonceRecord>,
    pending_by_ip: HashMap<IpAddr, usize>,
    issues: HashMap<IpAddr, VecDeque<Instant>>,
    redeems: HashMap<IpAddr, VecDeque<Instant>>,
}

/// One-shot, rate-limited nonce store for browser session exchange.
#[derive(Default)]
pub struct NonceStore {
    inner: Mutex<NonceInner>,
}

impl NonceStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn issue(&self, ip: IpAddr) -> Result<String, &'static str> {
        let now = Instant::now();
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        prune_expired_nonces(&mut inner, now);
        if let Some(events) = inner.issues.get_mut(&ip) {
            prune_events(events, now);
        }
        if inner.pending.len() >= MAX_PENDING_NONCES {
            return Err("nonce_pending_limit");
        }
        if inner.pending_by_ip.get(&ip).copied().unwrap_or(0) >= MAX_PENDING_PER_IP {
            return Err("nonce_ip_limit");
        }
        if inner
            .issues
            .get(&ip)
            .is_some_and(|events| events.len() >= MAX_NONCE_ISSUES_PER_MINUTE)
        {
            return Err("nonce_issue_rate");
        }

        let nonce = loop {
            let candidate = random_token();
            if !inner.pending.contains_key(&candidate) {
                break candidate;
            }
        };
        inner
            .pending
            .insert(nonce.clone(), NonceRecord { issued_at: now, ip });
        *inner.pending_by_ip.entry(ip).or_default() += 1;
        inner.issues.entry(ip).or_default().push_back(now);
        Ok(nonce)
    }

    /// Consume a nonce exactly once. Rate limit checks happen before consuming
    /// so a flooded caller cannot burn valid pending nonces.
    pub fn redeem(&self, ip: IpAddr, nonce: &str) -> Result<(), &'static str> {
        if nonce.is_empty() {
            return Err("invalid_nonce");
        }
        let now = Instant::now();
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        prune_expired_nonces(&mut inner, now);
        if let Some(events) = inner.redeems.get_mut(&ip) {
            prune_events(events, now);
        }
        if inner
            .redeems
            .get(&ip)
            .is_some_and(|events| events.len() >= MAX_NONCE_REDEEMS_PER_MINUTE)
        {
            return Err("nonce_redeem_rate");
        }

        let Some(record) = inner.pending.remove(nonce) else {
            return Err("invalid_nonce");
        };
        decrement_ip_pending(&mut inner.pending_by_ip, record.ip);
        if now.duration_since(record.issued_at) >= NONCE_TTL {
            return Err("invalid_nonce");
        }
        inner.redeems.entry(ip).or_default().push_back(now);
        Ok(())
    }
}

/// Server-side browser session. The cookie carries only [`Self::id`].
pub struct BrowserSession {
    id: String,
    csrf_token: String,
    lease: Mutex<Lease>,
    active_session: Mutex<Option<String>>,
    last_seen: Mutex<Instant>,
    command_events: Mutex<VecDeque<Instant>>,
    service_events: Mutex<VecDeque<Instant>>,
    approvals: Mutex<HashMap<String, ApprovalChallenge>>,
    cancel: watch::Sender<bool>,
}

impl BrowserSession {
    pub fn new(lease: Lease) -> Arc<Self> {
        let (cancel, _) = watch::channel(false);
        Arc::new(Self {
            id: random_token(),
            csrf_token: random_token(),
            lease: Mutex::new(lease),
            active_session: Mutex::new(None),
            last_seen: Mutex::new(Instant::now()),
            command_events: Mutex::new(VecDeque::new()),
            service_events: Mutex::new(VecDeque::new()),
            approvals: Mutex::new(HashMap::new()),
            cancel,
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn csrf_token(&self) -> &str {
        &self.csrf_token
    }

    pub fn lease_snapshot(&self) -> Lease {
        self.lease
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    pub fn replace_lease(&self, lease: Lease) {
        *self.lease.lock().unwrap_or_else(|error| error.into_inner()) = lease;
        self.clear_approvals();
    }

    pub fn mark_renewed(&self) {
        self.lease
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .last_renew = Instant::now();
    }

    pub fn allow_command(&self) -> bool {
        allow_rate(&self.command_events, MAX_COMMANDS_PER_MINUTE)
    }

    pub fn allow_service(&self) -> bool {
        allow_rate(&self.service_events, MAX_SERVICE_CALLS_PER_MINUTE)
    }

    pub fn active_session(&self) -> Option<String> {
        self.active_session
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    pub fn set_active_session(&self, session_id: Option<String>) {
        *self
            .active_session
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = session_id;
        self.clear_approvals();
    }

    pub fn issue_approval(
        &self,
        session_id: &str,
        kind: ApprovalKind,
        source_id: String,
        details: Value,
    ) -> Result<ApprovalChallenge, &'static str> {
        let mut approvals = self
            .approvals
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        cleanup_approvals(&mut approvals);
        if let Some(existing) = approvals.values().find(|challenge| {
            challenge.kind == kind
                && challenge.session_id == session_id
                && challenge.source_id == source_id
        }) {
            return Ok(existing.clone());
        }
        if approvals.len() >= MAX_PENDING_APPROVALS {
            return Err("approval_limit");
        }

        let id = loop {
            let candidate = random_token();
            if !approvals.contains_key(&candidate) {
                break candidate;
            }
        };
        let challenge = ApprovalChallenge {
            id: id.clone(),
            kind,
            source_id,
            session_id: session_id.to_string(),
            details,
            issued_at: Instant::now(),
        };
        approvals.insert(id, challenge.clone());
        Ok(challenge)
    }

    /// Consume a challenge before any daemon call. A failed or rejected
    /// command therefore cannot be retried with the same challenge.
    pub fn consume_approval(
        &self,
        id: &str,
        active_session: &str,
    ) -> Result<ApprovalChallenge, &'static str> {
        let mut approvals = self
            .approvals
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        cleanup_approvals(&mut approvals);
        let challenge = approvals.remove(id).ok_or("approval_not_found")?;
        if challenge.session_id != active_session {
            return Err("approval_scope_violation");
        }
        Ok(challenge)
    }

    fn clear_approvals(&self) {
        self.approvals
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
    }

    pub fn touch(&self) {
        *self
            .last_seen
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Instant::now();
    }

    pub fn is_expired(&self) -> bool {
        let last_seen = *self
            .last_seen
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        last_seen.elapsed() >= SESSION_IDLE_TTL
    }

    pub fn cancel(&self) {
        let _ = self.cancel.send(true);
    }

    pub fn subscribe_cancel(&self) -> watch::Receiver<bool> {
        self.cancel.subscribe()
    }
}

#[derive(Default)]
pub struct SessionStore {
    inner: Mutex<HashMap<String, Arc<BrowserSession>>>,
}

impl SessionStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, session: Arc<BrowserSession>) -> Result<(), &'static str> {
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        cleanup_sessions(&mut inner);
        if inner.len() >= MAX_SESSIONS {
            return Err("session_limit");
        }
        inner.insert(session.id().to_string(), session);
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<Arc<BrowserSession>> {
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        cleanup_sessions(&mut inner);
        let session = inner.get(id).cloned()?;
        if session.is_expired() {
            inner.remove(id);
            session.cancel();
            return None;
        }
        Some(session)
    }

    pub fn remove(&self, id: &str) -> Option<Arc<BrowserSession>> {
        let session = self
            .inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(id);
        if let Some(session) = &session {
            session.cancel();
        }
        session
    }
}

fn cleanup_approvals(approvals: &mut HashMap<String, ApprovalChallenge>) {
    approvals.retain(|_, challenge| !challenge.is_expired());
}

fn cleanup_sessions(sessions: &mut HashMap<String, Arc<BrowserSession>>) {
    let expired = sessions
        .iter()
        .filter_map(|(id, session)| session.is_expired().then_some(id.clone()))
        .collect::<Vec<_>>();
    for id in expired {
        if let Some(session) = sessions.remove(&id) {
            session.cancel();
        }
    }
}

fn prune_expired_nonces(inner: &mut NonceInner, now: Instant) {
    let expired = inner
        .pending
        .iter()
        .filter_map(|(nonce, record)| {
            (now.duration_since(record.issued_at) >= NONCE_TTL).then_some(nonce.clone())
        })
        .collect::<Vec<_>>();
    for nonce in expired {
        if let Some(record) = inner.pending.remove(&nonce) {
            decrement_ip_pending(&mut inner.pending_by_ip, record.ip);
        }
    }
}

fn decrement_ip_pending(pending: &mut HashMap<IpAddr, usize>, ip: IpAddr) {
    if let Some(count) = pending.get_mut(&ip) {
        *count = count.saturating_sub(1);
        if *count == 0 {
            pending.remove(&ip);
        }
    }
}

fn prune_events(events: &mut VecDeque<Instant>, now: Instant) {
    while events
        .front()
        .is_some_and(|issued| now.duration_since(*issued) >= RATE_WINDOW)
    {
        events.pop_front();
    }
}

fn allow_rate(events: &Mutex<VecDeque<Instant>>, max: usize) -> bool {
    let now = Instant::now();
    let mut events = events.lock().unwrap_or_else(|error| error.into_inner());
    prune_events(&mut events, now);
    if events.len() >= max {
        return false;
    }
    events.push_back(now);
    true
}

pub fn random_token() -> String {
    rand::random::<[u8; 32]>()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lease(id: &str) -> Lease {
        Lease::new(
            format!("instance-{id}"),
            format!("session-{id}"),
            "epoch".into(),
            30_000,
            10_000,
        )
    }

    #[test]
    fn nonce_is_single_use_and_issue_rate_capped() {
        let store = NonceStore::new();
        let ip = "127.0.0.1".parse().unwrap();
        let nonce = store.issue(ip).unwrap();
        assert!(store.redeem(ip, &nonce).is_ok());
        assert!(store.redeem(ip, &nonce).is_err());

        let rate_store = NonceStore::new();
        for _ in 0..MAX_NONCE_ISSUES_PER_MINUTE {
            rate_store.issue(ip).unwrap();
        }
        assert_eq!(rate_store.issue(ip), Err("nonce_issue_rate"));
    }

    #[test]
    fn session_store_enforces_active_session_cap() {
        let store = SessionStore::new();
        for index in 0..MAX_SESSIONS {
            store
                .insert(BrowserSession::new(lease(&index.to_string())))
                .unwrap();
        }
        assert_eq!(
            store.insert(BrowserSession::new(lease("overflow"))),
            Err("session_limit")
        );
    }

    #[test]
    fn session_replaces_lease_and_tracks_session() {
        let session = BrowserSession::new(lease("one"));
        session.set_active_session(Some("0123abcd".into()));
        assert_eq!(session.active_session().as_deref(), Some("0123abcd"));
        session.replace_lease(lease("two"));
        assert_eq!(session.lease_snapshot().client_session_id, "session-two");
    }
}
