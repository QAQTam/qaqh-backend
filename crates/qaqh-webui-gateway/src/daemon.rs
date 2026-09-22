//! Minimal daemon HTTP client used only by the gateway.
//!
//! This is intentionally not a general-purpose public client. It keeps the
//! daemon bearer token inside the gateway process and exposes only the small
//! operations needed by the browser proxy.

use std::time::Duration;

use axum::http::{HeaderMap, header};
use qaqh_ringing::{ClientOpenRequest, ClientOpenResponse, RINGING_SCHEMA, RINGING_VERSION};
use qaqh_types::DaemonDiscovery;
use reqwest::{Method, Response};

use crate::session::Lease;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone)]
pub struct DaemonClient {
    http: reqwest::Client,
    base_url: String,
    token: String,
    epoch: String,
}

impl DaemonClient {
    pub fn new(discovery: &DaemonDiscovery) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|error| format!("build daemon http client: {error}"))?;
        Ok(Self {
            http,
            base_url: discovery.endpoint.trim_end_matches('/').to_string(),
            token: discovery.token.clone(),
            epoch: discovery.server_epoch.clone(),
        })
    }

    pub fn epoch(&self) -> &str {
        &self.epoch
    }

    /// Verify that the reachable process is the same daemon recorded by
    /// discovery. `/health` is the only unauthenticated identity probe.
    pub async fn verify_epoch(&self) -> Result<(), String> {
        let response = self
            .http
            .get(format!("{}/health", self.base_url))
            .send()
            .await
            .map_err(|error| format!("daemon health request failed: {error}"))?;
        if !response.status().is_success() {
            return Err(format!("daemon health returned HTTP {}", response.status()));
        }
        let body = response
            .text()
            .await
            .map_err(|error| format!("read daemon health response: {error}"))?;
        let expected = format!("ok epoch={}", self.epoch);
        if body.trim() != expected {
            return Err("daemon discovery epoch does not match the live daemon".into());
        }
        Ok(())
    }

    pub async fn open(&self, client_instance_id: &str) -> Result<Lease, String> {
        let response = self
            .http
            .post(format!("{}/ringing/v1/clients/open", self.base_url))
            .bearer_auth(&self.token)
            .json(&ClientOpenRequest::new(client_instance_id))
            .send()
            .await
            .map_err(|error| format!("daemon open request failed: {error}"))?;
        if !response.status().is_success() {
            return Err(format!("daemon open returned HTTP {}", response.status()));
        }
        let open: ClientOpenResponse = response
            .json()
            .await
            .map_err(|error| format!("decode daemon open response: {error}"))?;
        if open.schema != RINGING_SCHEMA
            || open.version != RINGING_VERSION
            || !open.accepted
            || open.client_session_id.is_empty()
            || open.server_epoch.is_empty()
            || open.lease_ttl_ms == 0
            || open.renew_interval_ms == 0
        {
            return Err("daemon open returned an invalid lease".into());
        }
        Ok(Lease::new(
            client_instance_id.to_string(),
            open.client_session_id,
            open.server_epoch,
            open.lease_ttl_ms,
            open.renew_interval_ms,
        ))
    }

    pub async fn renew(&self, lease: &Lease) -> Result<(), String> {
        let response = self
            .request(Method::POST, "/ringing/v1/leases/renew", Some(lease), None)
            .await?;
        if !response.status().is_success() {
            return Err(format!("daemon renew returned HTTP {}", response.status()));
        }
        Ok(())
    }

    pub async fn get(&self, path: &str, lease: &Lease) -> Result<Response, String> {
        self.request(Method::GET, path, Some(lease), None).await
    }

    pub async fn get_stream(
        &self,
        path: &str,
        lease: &Lease,
        headers: &HeaderMap,
    ) -> Result<Response, String> {
        let mut request = self
            .http
            .get(format!("{}{}", self.base_url, path))
            .bearer_auth(&self.token)
            .header("x-qaqh-client-session-id", &lease.client_session_id);
        if let Some(value) = headers.get("last-event-id") {
            request = request.header("last-event-id", value.clone());
        }
        if let Some(value) = headers.get(header::ACCEPT) {
            request = request.header(header::ACCEPT, value.clone());
        }
        request
            .send()
            .await
            .map_err(|error| format!("daemon stream request failed: {error}"))
    }

    pub async fn post_bytes(
        &self,
        path: &str,
        lease: &Lease,
        body: Vec<u8>,
        content_type: &str,
    ) -> Result<Response, String> {
        self.http
            .post(format!("{}{}", self.base_url, path))
            .bearer_auth(&self.token)
            .header("x-qaqh-client-session-id", &lease.client_session_id)
            .header(header::CONTENT_TYPE, content_type)
            .body(body)
            .send()
            .await
            .map_err(|error| format!("daemon upload request failed: {error}"))
    }

    pub async fn post_json(
        &self,
        path: &str,
        lease: &Lease,
        body: &serde_json::Value,
    ) -> Result<Response, String> {
        self.request(Method::POST, path, Some(lease), Some(body))
            .await
    }

    pub async fn request(
        &self,
        method: Method,
        path: &str,
        lease: Option<&Lease>,
        body: Option<&serde_json::Value>,
    ) -> Result<Response, String> {
        let mut request = self
            .http
            .request(method, format!("{}{}", self.base_url, path))
            .bearer_auth(&self.token);
        if let Some(lease) = lease {
            request = request.header("x-qaqh-client-session-id", &lease.client_session_id);
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        request
            .send()
            .await
            .map_err(|error| format!("daemon request failed: {error}"))
    }
}
