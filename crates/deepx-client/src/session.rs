//! Ringing V1 session negotiation and lease renewal.

use std::sync::Arc;

use tokio::sync::Mutex;

use crate::error::{ClientError, Result};
use crate::types::{OpenRequest, OpenResponse};

/// Negotiated session state (mirrors `RingingSessionOpen` in TS).
#[derive(Debug, Clone)]
pub struct SessionState {
    pub client_instance_id: String,
    pub client_session_id: String,
    pub server_epoch: String,
    pub lease_ttl_ms: u64,
    pub renew_interval_ms: u64,
}

/// Ringing V1 session: open + background lease renewal.
pub struct RingingSession {
    base_url: String,
    token: String,
    http: reqwest::Client,
    state: Arc<Mutex<Option<SessionState>>>,
    /// Consecutive renewal failures; `>= 2` marks the lease unhealthy.
    renew_failures: Arc<Mutex<u32>>,
}

const MAX_RENEW_FAILURES: u32 = 2;
const CAPABILITIES: [&str; 4] = [
    "Ringing_v1",
    "Ringing_batch_v1",
    "Ringing_bootstrap_v1",
    "Ringing_command_status_v1",
];

impl RingingSession {
    pub fn new(base_url: String, token: String, http: reqwest::Client) -> Self {
        Self {
            base_url,
            token,
            http,
            state: Arc::new(Mutex::new(None)),
            renew_failures: Arc::new(Mutex::new(0)),
        }
    }

    pub fn client_instance_id(&self) -> String {
        uuid::Uuid::new_v4().to_string()
    }

    /// `POST /ringing/v1/clients/open` — capability negotiation.
    pub async fn open(&self) -> Result<SessionState> {
        let client_instance_id = self.client_instance_id();
        let response = self
            .http
            .post(format!("{}/ringing/v1/clients/open", self.base_url))
            .bearer_auth(&self.token)
            .json(&OpenRequest {
                schema: "deepx.Ringing",
                version: 1,
                client_instance_id: client_instance_id.clone(),
                capabilities: CAPABILITIES.to_vec(),
            })
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(ClientError::Http {
                status: response.status().as_u16(),
                path: "/ringing/v1/clients/open".into(),
            });
        }
        let result: OpenResponse = response.json().await?;
        if !result.accepted {
            return Err(ClientError::Negotiation("open not accepted by daemon".into()));
        }
        if result.client_session_id.is_empty()
            || result.server_epoch.is_empty()
            || result.lease_ttl_ms == 0
            || result.renew_interval_ms == 0
        {
            return Err(ClientError::Negotiation("open returned an incomplete session".into()));
        }
        let state = SessionState {
            client_instance_id,
            client_session_id: result.client_session_id,
            server_epoch: result.server_epoch,
            lease_ttl_ms: result.lease_ttl_ms,
            renew_interval_ms: result.renew_interval_ms,
        };
        *self.state.lock().await = Some(state.clone());
        Ok(state)
    }

    /// Adopt a session opened elsewhere (e.g. by a control client in the same process).
    pub async fn adopt(&self, state: SessionState) {
        *self.state.lock().await = Some(state);
    }

    /// Current session state, if negotiated.
    pub async fn state(&self) -> Option<SessionState> {
        self.state.lock().await.clone()
    }

    /// Start the background renewal loop. Returns when the loop exits (stop flag).
    pub async fn run_renewal(&self, mut stop: tokio::sync::watch::Receiver<bool>) {
        let Some(state) = self.state.lock().await.clone() else {
            return;
        };
        let interval = std::time::Duration::from_millis(
            std::cmp::max(1000, state.renew_interval_ms / 2),
        );
        let mut ticker = tokio::time::interval(interval);
        // First tick fires immediately; skip it so the first renewal happens after
        // one interval (mirrors TS `setInterval` semantics).
        ticker.tick().await;
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if self.renew_once().await.is_err() {
                        let failures = {
                            let mut f = self.renew_failures.lock().await;
                            *f += 1;
                            *f
                        };
                        if failures >= MAX_RENEW_FAILURES {
                            log::warn!("[deepx-client] lease renewal unhealthy after {failures} failures");
                        }
                    } else {
                        *self.renew_failures.lock().await = 0;
                    }
                }
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() {
                        return;
                    }
                }
            }
        }
    }

    /// `POST /ringing/v1/leases/renew` — single renewal attempt.
    async fn renew_once(&self) -> Result<()> {
        let Some(state) = self.state.lock().await.clone() else {
            return Err(ClientError::Negotiation("no session to renew".into()));
        };
        let response = self
            .http
            .post(format!("{}/ringing/v1/leases/renew", self.base_url))
            .bearer_auth(&self.token)
            .header("X-DeepX-Client-Session-Id", &state.client_session_id)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(ClientError::Http {
                status: response.status().as_u16(),
                path: "/ringing/v1/leases/renew".into(),
            });
        }
        Ok(())
    }
}
