//! DeepX client wrapper for the TUI.
//!
//! Connects to (or launches) the daemon, subscribes to control messages,
//! and provides convenience methods for session management.

use anyhow::{Context, Result};
use deepx_client::DeepxClient;

pub struct TuiClient {
    pub client: DeepxClient,
}

impl TuiClient {
    /// Connect to (or launch) the daemon.
    pub async fn connect_or_launch() -> Result<Self> {
        let client = DeepxClient::connect_or_launch("deepx-tui", None)
            .await
            .context("Failed to connect to daemon")?;

        Ok(Self { client })
    }

    /// Request something from the daemon (JSON-RPC style).
    pub async fn request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        self.client
            .request(method, params)
            .await
            .context("Request failed")
    }

    /// Subscribe to a session's events.
    pub async fn attach_session(&self, seed: &str) -> Result<serde_json::Value> {
        self.client
            .attach(seed)
            .await
            .context("Failed to attach to session")
    }

    /// Unsubscribe from a session's events.
    pub async fn detach_session(&self, seed: &str) -> Result<serde_json::Value> {
        self.client
            .detach(seed)
            .await
            .context("Failed to detach from session")
    }

    /// List all sessions.
    pub async fn list_sessions(&self) -> Result<serde_json::Value> {
        self.request("session.list", serde_json::json!({})).await
    }

    /// Create a new session.
    pub async fn create_session(&self) -> Result<String> {
        let result = self.request("session.new", serde_json::json!({})).await?;
        result
            .as_str()
            .map(str::to_owned)
            .context("session.new returned a non-string seed")
    }

    /// Start or restore the agent process for an attached session.
    pub async fn resume_session(&self, seed: &str) -> Result<serde_json::Value> {
        self.request("session.resume", serde_json::json!({ "seed": seed }))
            .await
    }

    /// Send a user message to the current session.
    pub async fn send_text(&self, seed: &str, text: &str) -> Result<serde_json::Value> {
        self.request(
            "session.send_message",
            serde_json::json!({
                "seed": seed,
                "text": text,
            }),
        )
        .await
    }

    /// Delete a session.
    pub async fn delete_session(&self, seed: &str) -> Result<serde_json::Value> {
        self.request("session.delete", serde_json::json!({ "seed": seed }))
            .await
    }
}
