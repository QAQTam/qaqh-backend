//! Durable sidecar types for the canonical session log.
//!
//! These shapes are frozen by `session-fact-v2` §5.1-5.2. Fact payload types
//! live in [`crate::session_fact_v2`]; this module only owns storage identity,
//! writer fencing and commit high-water metadata.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::session_fact_v2::{EventId, LogId, SessionId};

pub const WRITER_FENCE_SCHEMA: &str = "qaqh.writer-fence/v1";
pub const EVENTS_COMMIT_SCHEMA: &str = "qaqh.events-commit/v1";

mod u128_string {
    use serde::de::Error;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(value: &u128, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&value.to_string())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<u128, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if value.is_empty() || (value.len() > 1 && value.starts_with('0')) {
            return Err(D::Error::custom(
                "fencing token must be canonical unsigned decimal",
            ));
        }
        value
            .parse::<u128>()
            .map_err(|_| D::Error::custom("fencing token overflow or invalid decimal"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WriterId(pub String);

impl WriterId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for WriterId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriterFence {
    pub schema: String,
    pub session_id: SessionId,
    pub log_id: LogId,
    pub writer_id: WriterId,
    pub generation_epoch: u64,
    #[serde(with = "u128_string")]
    pub fencing_token: u128,
    pub acquired_at_ms: i64,
    pub lease_expires_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriterLease {
    pub writer_id: WriterId,
    pub log_id: LogId,
    pub generation_epoch: u64,
    #[serde(with = "u128_string")]
    pub fencing_token: u128,
    pub lease_expires_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppendRejected {
    pub code: String,
    #[serde(with = "u128_string")]
    pub expected_token: u128,
    #[serde(with = "u128_string")]
    pub presented_token: u128,
    pub epoch: u64,
}

impl AppendRejected {
    pub fn stale_writer(expected_token: u128, presented_token: u128, epoch: u64) -> Self {
        Self {
            code: "stale_writer".into(),
            expected_token,
            presented_token,
            epoch,
        }
    }
}

impl fmt::Display for AppendRejected {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}: expected token {}, presented token {}, epoch {}",
            self.code, self.expected_token, self.presented_token, self.epoch
        )
    }
}

impl std::error::Error for AppendRejected {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventsCommit {
    pub schema: String,
    pub log_id: LogId,
    pub committed_fact_seq: u64,
    pub committed_offset: u64,
    pub last_barrier_event_id: Option<EventId>,
    pub commit_generation: u64,
}

impl EventsCommit {
    pub fn empty(log_id: LogId) -> Self {
        Self {
            schema: EVENTS_COMMIT_SCHEMA.into(),
            log_id,
            committed_fact_seq: 0,
            committed_offset: 0,
            last_barrier_event_id: None,
            commit_generation: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fence() -> WriterFence {
        WriterFence {
            schema: WRITER_FENCE_SCHEMA.into(),
            session_id: SessionId::new("0198f1a0-0000-7000-8000-000000000001"),
            log_id: LogId::new("0198f1a0-0000-7000-8000-000000000002"),
            writer_id: WriterId::new("writer-a"),
            generation_epoch: 3,
            fencing_token: 1007,
            acquired_at_ms: 10,
            lease_expires_at_ms: 20,
        }
    }

    #[test]
    fn fencing_tokens_are_decimal_strings() {
        let value = serde_json::to_value(fence()).expect("serialize fence");
        assert_eq!(value["fencing_token"], "1007");
        assert!(value["fencing_token"].is_string());

        let number = serde_json::json!({
            "schema": WRITER_FENCE_SCHEMA,
            "session_id": "0198f1a0-0000-7000-8000-000000000001",
            "log_id": "0198f1a0-0000-7000-8000-000000000002",
            "writer_id": "writer-a",
            "generation_epoch": 3,
            "fencing_token": 1007,
            "acquired_at_ms": 10,
            "lease_expires_at_ms": 20
        });
        assert!(serde_json::from_value::<WriterFence>(number).is_err());
    }

    #[test]
    fn append_rejected_uses_the_same_string_shape() {
        let rejected = AppendRejected::stale_writer(1007, 1006, 3);
        assert_eq!(
            serde_json::to_value(rejected).expect("serialize rejection"),
            serde_json::json!({
                "code": "stale_writer",
                "expected_token": "1007",
                "presented_token": "1006",
                "epoch": 3
            })
        );
    }
}
