//! JSON length-prefixed (JSON-LP) encoding / decoding helpers.
//!
//! IPC framing format:
//!
//! ```text
//! [4 bytes big-endian payload length][UTF-8 JSON payload]
//! ```
//!
//! The length field encodes the **byte count of the JSON payload only**
//! (the 4-byte header itself is excluded).

use serde::de::DeserializeOwned;
use serde::Serialize;
use std::io::Read;

// ── Error ──

/// Errors that can occur during JSON-LP encode / decode.
#[derive(Debug, thiserror::Error)]
pub enum SerdeError {
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Truncated frame: need {needed} bytes, got {available}")]
    Truncated { needed: usize, available: usize },

    #[error("Frame too large: {size} bytes (max {max})")]
    FrameTooLarge { size: usize, max: usize },
}

// ── Encode ──

/// Serialise `msg` as a JSON-LP frame.
///
/// Returns `Err` when serialisation fails or the payload exceeds
/// `MAX_FRAME_SIZE` (64 MiB by default).
pub fn encode_msg<T: Serialize>(msg: &T) -> Result<Vec<u8>, SerdeError> {
    encode_msg_with_max(msg, MAX_FRAME_SIZE)
}

/// Like [`encode_msg`] with an explicit maximum payload size.
pub fn encode_msg_with_max<T: Serialize>(
    msg: &T,
    max_payload: usize,
) -> Result<Vec<u8>, SerdeError> {
    let json = serde_json::to_vec(msg)?;
    let payload_len = json.len();

    if payload_len > max_payload {
        return Err(SerdeError::FrameTooLarge {
            size: payload_len,
            max: max_payload,
        });
    }

    let mut buf = Vec::with_capacity(4 + payload_len);
    buf.extend_from_slice(&(payload_len as u32).to_be_bytes());
    buf.extend_from_slice(&json);
    Ok(buf)
}

// ── Decode (streaming) ──

/// Read one JSON-LP frame from a [`Read`]-able stream.
///
/// Returns `(deserialised_value, bytes_consumed)`.
pub fn decode_msg<T: DeserializeOwned, R: Read>(reader: &mut R) -> Result<(T, usize), SerdeError> {
    let mut header = [0u8; 4];
    reader.read_exact(&mut header)?;
    let payload_len = u32::from_be_bytes(header) as usize;

    if payload_len > MAX_FRAME_SIZE {
        return Err(SerdeError::FrameTooLarge {
            size: payload_len,
            max: MAX_FRAME_SIZE,
        });
    }

    let mut json_buf = vec![0u8; payload_len];
    reader.read_exact(&mut json_buf)?;

    let value: T = serde_json::from_slice(&json_buf)?;
    Ok((value, 4 + payload_len))
}

// ── Decode (buffer) ──

/// Try to decode the **first** JSON-LP frame from an in-memory byte buffer.
///
/// - Returns `Ok(None)` when the buffer does not contain a complete frame
///   yet (not an error condition — call again with more data).
/// - Returns `Ok(Some((value, consumed)))` on success.  The caller should
///   advance the buffer by `consumed` bytes.
pub fn try_decode_msg<T: DeserializeOwned>(buf: &[u8]) -> Result<Option<(T, usize)>, SerdeError> {
    if buf.len() < 4 {
        return Ok(None);
    }

    let header = &buf[..4];
    let payload_len = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;

    if payload_len > MAX_FRAME_SIZE {
        return Err(SerdeError::FrameTooLarge {
            size: payload_len,
            max: MAX_FRAME_SIZE,
        });
    }

    let total = 4 + payload_len;
    if buf.len() < total {
        return Ok(None);
    }

    let value: T = serde_json::from_slice(&buf[4..total])?;
    Ok(Some((value, total)))
}

// ── Constants ──

/// Maximum allowed payload size (64 MiB).  Larger frames are rejected
/// eagerly to prevent memory exhaustion.
pub const MAX_FRAME_SIZE: usize = 64 * 1024 * 1024;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_small_message() {
        let msg = "hello IPC".to_string();
        let encoded = encode_msg(&msg).unwrap();
        let mut cursor = std::io::Cursor::new(&encoded);
        let (decoded, consumed): (String, _) = decode_msg(&mut cursor).unwrap();
        assert_eq!(decoded, msg);
        assert_eq!(consumed, encoded.len());
    }

    #[test]
    fn try_decode_from_partial_buffer() {
        let msg = serde_json::json!({"a": 1});
        let encoded = encode_msg(&msg).unwrap();

        // Only give 2 bytes of header → should return None
        let result: Option<(serde_json::Value, usize)> = try_decode_msg(&encoded[..2]).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn try_decode_full_frame() {
        let msg = serde_json::json!({"a": 1});
        let encoded = encode_msg(&msg).unwrap();
        let (decoded, consumed): (serde_json::Value, _) =
            try_decode_msg(&encoded).unwrap().unwrap();
        assert_eq!(decoded, msg);
        assert_eq!(consumed, encoded.len());
    }

    #[test]
    fn rejects_oversized_frame() {
        let err = encode_msg_with_max(&vec![0u8; 100], 50).unwrap_err();
        assert!(matches!(err, SerdeError::FrameTooLarge { .. }));
    }
}
