//! Versioned Desktop/TUI <-> daemon control protocol.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Agent2Ui, SessionActivity};

pub const CONTROL_PROTOCOL_VERSION: u16 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlClientMessage {
    ClientHello {
        protocol_version: u16,
        client_version: String,
        client_kind: String,
        client_instance_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after_epoch: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after_seq: Option<u64>,
    },
    Request {
        request_id: String,
        method: String,
        #[serde(default)]
        params: Value,
    },
    SessionAttach {
        request_id: String,
        seed: String,
    },
    SessionDetach {
        request_id: String,
        seed: String,
    },
    Heartbeat {
        nonce: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlServerMessage {
    ServerHello {
        protocol_version: u16,
        server_version: String,
        server_epoch: String,
        heartbeat_interval_ms: u64,
        lease_timeout_ms: u64,
        max_frame_bytes: usize,
    },
    Response {
        request_id: String,
        #[serde(default)]
        result: Value,
    },
    Event {
        server_epoch: String,
        seq: u64,
        seed: String,
        session_seq: u64,
        event: Agent2Ui,
    },
    SessionActivity {
        activity: SessionActivity,
    },
    Snapshot {
        server_epoch: String,
        seq: u64,
        snapshot: ControlSnapshot,
    },
    LeaseDenied {
        request_id: String,
        seed: String,
        owner_kind: String,
        retry_after_ms: u64,
    },
    Error {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
        code: String,
        message: String,
    },
    Heartbeat {
        server_epoch: String,
        seq: u64,
        nonce: u64,
    },
    Shutdown {
        server_epoch: String,
        seq: u64,
        reason: String,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ControlSnapshot {
    #[serde(default)]
    pub sessions: Vec<Value>,
    #[serde(default)]
    pub activities: Vec<SessionActivity>,
    #[serde(default)]
    pub attached_sessions: Vec<String>,
    /// Canonical event projections for attached sessions. Each projection
    /// starts with SessionCreated or SessionRestored and can be reduced by any
    /// client to reconstruct the current UI state.
    #[serde(default)]
    pub session_events: HashMap<String, Vec<Agent2Ui>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonDiscovery {
    pub endpoint: String,
    pub token: String,
    pub pid: u32,
    pub server_epoch: String,
    pub protocol_version: u16,
    /// Semver of the daemon binary. Defaults keep pre-0.9 discovery files
    /// readable so clients can replace them gracefully.
    #[serde(default)]
    pub daemon_version: String,
    /// Source/build identity used to reject a stale daemon with a compatible
    /// wire protocol.
    #[serde(default)]
    pub build_id: String,
    /// Runtime lane (for example `stable` or `dev`). Development clients must
    /// never silently adopt the installed stable daemon, or vice versa.
    #[serde(default)]
    pub channel: String,
    /// Canonical executable path, primarily for diagnostics and ownership
    /// checks during upgrades.
    #[serde(default)]
    pub executable: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trip_is_tagged_and_typed() {
        let message = ControlClientMessage::Request {
            request_id: "r1".into(),
            method: "session.list".into(),
            params: serde_json::json!({}),
        };
        let json = serde_json::to_value(&message).unwrap();
        assert_eq!(json["type"], "request");
        assert_eq!(
            serde_json::from_value::<ControlClientMessage>(json).unwrap(),
            message
        );
    }

    #[test]
    fn discovery_round_trip_preserves_protocol_version() {
        let discovery = DaemonDiscovery {
            endpoint: "ws://127.0.0.1:42/control/v1".into(),
            token: "secret".into(),
            pid: 7,
            server_epoch: "epoch".into(),
            protocol_version: CONTROL_PROTOCOL_VERSION,
            daemon_version: "0.9.0".into(),
            build_id: "commit".into(),
            channel: "stable".into(),
            executable: "/opt/deepx/deepx-daemon".into(),
        };
        let json = serde_json::to_string(&discovery).unwrap();
        assert_eq!(
            serde_json::from_str::<DaemonDiscovery>(&json).unwrap(),
            discovery
        );
    }

    #[test]
    fn legacy_discovery_defaults_identity_fields() {
        let discovery: DaemonDiscovery = serde_json::from_value(serde_json::json!({
            "endpoint": "ws://127.0.0.1:42/control/v1",
            "token": "secret",
            "pid": 7,
            "server_epoch": "epoch",
            "protocol_version": CONTROL_PROTOCOL_VERSION
        }))
        .unwrap();
        assert!(discovery.daemon_version.is_empty());
        assert!(discovery.build_id.is_empty());
        assert!(discovery.channel.is_empty());
        assert!(discovery.executable.is_empty());
    }

    #[test]
    fn every_control_message_round_trips() {
        let client_messages = vec![
            ControlClientMessage::ClientHello {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                client_version: "0.9.0".into(),
                client_kind: "tui".into(),
                client_instance_id: "client-1".into(),
                after_epoch: Some("epoch".into()),
                after_seq: Some(41),
            },
            ControlClientMessage::SessionAttach {
                request_id: "r2".into(),
                seed: "seed".into(),
            },
            ControlClientMessage::SessionDetach {
                request_id: "r3".into(),
                seed: "seed".into(),
            },
            ControlClientMessage::Heartbeat { nonce: 7 },
        ];
        for message in client_messages {
            let json = serde_json::to_string(&message).unwrap();
            let decoded: ControlClientMessage = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, message);
        }

        let server_messages = vec![
            ControlServerMessage::ServerHello {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                server_version: "0.9.0".into(),
                server_epoch: "epoch".into(),
                heartbeat_interval_ms: 5_000,
                lease_timeout_ms: 15_000,
                max_frame_bytes: 1_048_576,
            },
            ControlServerMessage::Response {
                request_id: "r1".into(),
                result: serde_json::json!({"typed": true}),
            },
            ControlServerMessage::Event {
                server_epoch: "epoch".into(),
                seq: 1,
                seed: "seed".into(),
                session_seq: 1,
                event: Agent2Ui::Ready,
            },
            ControlServerMessage::Snapshot {
                server_epoch: "epoch".into(),
                seq: 1,
                snapshot: ControlSnapshot::default(),
            },
            ControlServerMessage::LeaseDenied {
                request_id: "r2".into(),
                seed: "seed".into(),
                owner_kind: "desktop".into(),
                retry_after_ms: 100,
            },
            ControlServerMessage::Error {
                request_id: Some("r3".into()),
                code: "request_failed".into(),
                message: "failed".into(),
            },
            ControlServerMessage::Shutdown {
                server_epoch: "epoch".into(),
                seq: 2,
                reason: "requested".into(),
            },
        ];
        for message in server_messages {
            let json = serde_json::to_string(&message).unwrap();
            let _: ControlServerMessage = serde_json::from_str(&json).unwrap();
        }
    }
}
