//! Ringing v2 canonical cursor and opaque token encoding.
//!
//! The wire token is intentionally opaque to consumers. The logical cursor is
//! still available for daemon-side validation and diagnostics.

use std::fmt;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};

use crate::MAX_SAFE_INTEGER;

/// Reserved `projection_index` for a snapshot baseline (`END_OF_FACT`).
pub const END_OF_FACT: u16 = u16::MAX;

/// Logical canonical cursor shared by v2 replay and reset paths.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CanonicalCursor {
    pub log_id: String,
    pub fact_seq: u64,
    pub projection_index: u16,
}

impl CanonicalCursor {
    pub fn new(log_id: impl Into<String>, fact_seq: u64, projection_index: u16) -> Self {
        Self {
            log_id: log_id.into(),
            fact_seq,
            projection_index,
        }
    }

    pub fn snapshot(log_id: impl Into<String>, fact_seq: u64) -> Self {
        Self::new(log_id, fact_seq, END_OF_FACT)
    }

    pub fn validate_common(&self) -> Result<(), CursorError> {
        if self.log_id.trim().is_empty() {
            return Err(CursorError::InvalidCursor("log_id must not be empty"));
        }
        if self.fact_seq == 0 || self.fact_seq > MAX_SAFE_INTEGER {
            return Err(CursorError::InvalidCursor(
                "fact_seq must be in 1..=MAX_SAFE_INTEGER",
            ));
        }
        Ok(())
    }

    pub fn validate_reliable(&self) -> Result<(), CursorError> {
        self.validate_common()?;
        if self.projection_index == END_OF_FACT {
            return Err(CursorError::InvalidCursor(
                "reliable projection_index must be <= 65534",
            ));
        }
        Ok(())
    }

    pub fn validate_snapshot(&self) -> Result<(), CursorError> {
        self.validate_common()?;
        if self.projection_index != END_OF_FACT {
            return Err(CursorError::InvalidCursor(
                "snapshot projection_index must be END_OF_FACT",
            ));
        }
        Ok(())
    }

    /// Compare two cursors only when they belong to the same canonical log.
    pub fn is_after(&self, other: &Self) -> Result<bool, CursorError> {
        self.validate_common()?;
        other.validate_common()?;
        if self.log_id != other.log_id {
            return Err(CursorError::LogMismatch);
        }
        Ok((self.fact_seq, self.projection_index) > (other.fact_seq, other.projection_index))
    }
}

/// Opaque wire token: `v2.<base64url-no-pad(compact-json)>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CursorToken(String);

impl CursorToken {
    /// Encode a logical cursor without imposing reliable/snapshot semantics.
    ///
    /// Call [`Self::encode_reliable`] or [`Self::encode_snapshot`] on paths
    /// where the sentinel meaning is known.
    pub fn encode(cursor: &CanonicalCursor) -> Result<Self, CursorError> {
        cursor.validate_common()?;
        let json = serde_json::to_vec(cursor).map_err(CursorError::Json)?;
        Ok(Self(format!("v2.{}", URL_SAFE_NO_PAD.encode(json))))
    }

    pub fn encode_reliable(cursor: &CanonicalCursor) -> Result<Self, CursorError> {
        cursor.validate_reliable()?;
        Self::encode(cursor)
    }

    pub fn encode_snapshot(cursor: &CanonicalCursor) -> Result<Self, CursorError> {
        cursor.validate_snapshot()?;
        Self::encode(cursor)
    }

    /// Decode and validate the common cursor fields.
    ///
    /// This does not accept arbitrary JSON: the payload must round-trip to the
    /// exact canonical token form, rejecting padding, non-canonical base64 and
    /// trailing JSON.
    pub fn decode(&self) -> Result<CanonicalCursor, CursorError> {
        let encoded = self
            .0
            .strip_prefix("v2.")
            .ok_or(CursorError::InvalidPrefix)?;
        if encoded.is_empty() {
            return Err(CursorError::InvalidBase64);
        }
        let bytes = URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|_| CursorError::InvalidBase64)?;
        let cursor: CanonicalCursor = serde_json::from_slice(&bytes).map_err(CursorError::Json)?;
        cursor.validate_common()?;
        let canonical = Self::encode(&cursor)?;
        if canonical.0 != self.0 {
            return Err(CursorError::NonCanonical);
        }
        Ok(cursor)
    }

    pub fn decode_reliable(&self) -> Result<CanonicalCursor, CursorError> {
        let cursor = self.decode()?;
        cursor.validate_reliable()?;
        Ok(cursor)
    }

    pub fn decode_snapshot(&self) -> Result<CanonicalCursor, CursorError> {
        let cursor = self.decode()?;
        cursor.validate_snapshot()?;
        Ok(cursor)
    }

    pub fn decode_for_log(&self, log_id: &str) -> Result<CanonicalCursor, CursorError> {
        let cursor = self.decode()?;
        if cursor.log_id != log_id {
            return Err(CursorError::LogMismatch);
        }
        Ok(cursor)
    }

    /// Wrap an opaque token received from the wire.
    ///
    /// Validation is deferred to [`Self::decode`] so the caller can map a
    /// malformed token to a stable protocol error.
    pub fn from_opaque(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Display for CursorToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug)]
pub enum CursorError {
    InvalidPrefix,
    InvalidBase64,
    NonCanonical,
    InvalidCursor(&'static str),
    LogMismatch,
    Json(serde_json::Error),
}

impl fmt::Display for CursorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPrefix => formatter.write_str("cursor token must start with `v2.`"),
            Self::InvalidBase64 => formatter.write_str("cursor token is not base64url-no-pad"),
            Self::NonCanonical => formatter.write_str("cursor token is not canonical"),
            Self::InvalidCursor(message) => write!(formatter, "invalid cursor: {message}"),
            Self::LogMismatch => formatter.write_str("cursor log_id does not match"),
            Self::Json(error) => write!(formatter, "cursor payload is not valid JSON: {error}"),
        }
    }
}

impl std::error::Error for CursorError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Json(error) => Some(error),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_round_trip_is_canonical() {
        let cursor = CanonicalCursor::new("log-1", 42, 1);
        let token = CursorToken::encode_reliable(&cursor).expect("encode");
        assert!(token.as_str().starts_with("v2."));
        assert!(!token.as_str().contains('='));
        assert_eq!(token.decode_reliable().expect("decode"), cursor);
    }

    #[test]
    fn snapshot_cursor_uses_end_of_fact() {
        let cursor = CanonicalCursor::snapshot("log-1", 42);
        let token = CursorToken::encode_snapshot(&cursor).expect("encode snapshot");
        assert_eq!(token.decode_snapshot().expect("decode"), cursor);
        assert!(CursorToken::encode_reliable(&cursor).is_err());
        assert!(token.decode_reliable().is_err());
    }

    #[test]
    fn cursor_rejects_tampering_and_noncanonical_tokens() {
        let token =
            CursorToken::encode_reliable(&CanonicalCursor::new("log-1", 42, 1)).expect("encode");
        let mut tampered = token.as_str().to_string();
        tampered.replace_range(3..4, "!");
        assert!(matches!(
            CursorToken(tampered).decode(),
            Err(CursorError::InvalidBase64)
        ));

        let padded = format!("{}=", token.as_str());
        assert!(CursorToken(padded).decode().is_err());
        assert!(CursorToken("v1.abc".into()).decode().is_err());
    }

    #[test]
    fn cursors_from_different_logs_are_not_comparable() {
        let left = CanonicalCursor::new("log-1", 42, 1);
        let right = CanonicalCursor::new("log-2", 43, 1);
        assert!(matches!(
            left.is_after(&right),
            Err(CursorError::LogMismatch)
        ));
        assert!(matches!(
            CursorToken::encode_reliable(&left)
                .expect("encode")
                .decode_for_log("log-2"),
            Err(CursorError::LogMismatch)
        ));
    }

    #[test]
    fn reliable_cursor_must_have_nonzero_fact_seq() {
        let cursor = CanonicalCursor::new("log-1", 0, 1);
        assert!(CursorToken::encode_reliable(&cursor).is_err());
    }
}
