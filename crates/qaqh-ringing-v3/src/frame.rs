//! 长度前缀帧编解码（`ADR-0042` 铁律 2：单帧字节上限守卫）。
//!
//! 形状固定为 `u32_le(len) ++ postcard(payload)`。用长度前缀而不是流式
//! 自描述格式，是为了让接收端**在解码前**就能拒绝超限帧——缓冲区按 `len`
//! 预分配之前先比 [`MAX_FRAME_BYTES`]，恶意对端连一次大分配都换不到。

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::MAX_FRAME_BYTES;

/// 帧编解码失败原因（全部 Fail-Closed：不截断、不修补、不猜）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FrameError {
    /// 声明长度超过 [`MAX_FRAME_BYTES`]。
    #[error("帧声明 {declared} 字节超过上限 {MAX_FRAME_BYTES}")]
    TooLarge {
        /// 对端声明的载荷长度。
        declared: usize,
    },
    /// 长度前缀不完整（连接在 4 字节中间断掉）。
    #[error("帧长度前缀不完整：需要 4 字节，只有 {available}")]
    TruncatedPrefix {
        /// 实际可读字节数。
        available: usize,
    },
    /// 载荷不完整。
    #[error("帧载荷不完整：声明 {declared} 字节，只有 {available}")]
    TruncatedPayload {
        /// 声明长度。
        declared: usize,
        /// 实际可读字节数。
        available: usize,
    },
    /// 载荷长度前缀与解析出的载荷字节数不一致（防裁剪/拼接攻击）。
    #[error("帧长度前缀 {declared} 与载荷实际 {actual} 不一致")]
    LengthMismatch {
        /// 声明长度。
        declared: usize,
        /// 实际载荷长度。
        actual: usize,
    },
    /// `postcard` 编码失败。
    #[error("帧编码失败：{0}")]
    Encode(String),
    /// `postcard` 解码失败（含尾部多余字节）。
    #[error("帧解码失败：{0}")]
    Decode(String),
}

/// 把一条载荷编码成带长度前缀的完整帧。
pub fn encode_frame<T: Serialize>(payload: &T) -> Result<Vec<u8>, FrameError> {
    let body = postcard::to_stdvec(payload).map_err(|error| FrameError::Encode(error.to_string()))?;
    if body.len() > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge {
            declared: body.len(),
        });
    }
    let declared = u32::try_from(body.len()).map_err(|_| FrameError::TooLarge {
        declared: body.len(),
    })?;
    let mut framed = Vec::with_capacity(4 + body.len());
    framed.extend_from_slice(&declared.to_le_bytes());
    framed.extend_from_slice(&body);
    Ok(framed)
}

/// 从字节流头部解析出**声明长度**（不改动游标；调用方据此决定读多少、是否拒绝）。
pub fn peek_frame_len(prefix: &[u8]) -> Result<usize, FrameError> {
    if prefix.len() < 4 {
        return Err(FrameError::TruncatedPrefix {
            available: prefix.len(),
        });
    }
    let declared = u32::from_le_bytes([prefix[0], prefix[1], prefix[2], prefix[3]]) as usize;
    if declared > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge { declared });
    }
    Ok(declared)
}

/// 解析一条完整帧（`bytes` 必须恰好是一帧：4 字节前缀 + `len` 字节载荷）。
///
/// 严格到什么程度：
/// - 前缀声明长度超限 → [`FrameError::TooLarge`]（**先于**任何分配）；
/// - 载荷不足 → [`FrameError::TruncatedPayload`]；
/// - 尾部有多余字节 → [`FrameError::LengthMismatch`]（防止"一帧包里塞两帧"的
///   走私手法把上层解复用逻辑绕过去）；
/// - `postcard` 尾部残留 → [`FrameError::Decode`]。
pub fn decode_frame<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, FrameError> {
    let declared = peek_frame_len(bytes)?;
    let available = bytes.len() - 4;
    if available < declared {
        return Err(FrameError::TruncatedPayload {
            declared,
            available,
        });
    }
    if available > declared {
        return Err(FrameError::LengthMismatch {
            declared,
            actual: available,
        });
    }
    let (value, rest) = postcard::take_from_bytes::<T>(&bytes[4..])
        .map_err(|error| FrameError::Decode(error.to_string()))?;
    if !rest.is_empty() {
        return Err(FrameError::Decode(format!(
            "载荷尾部残留 {} 字节（postcard 必须吃干净）",
            rest.len()
        )));
    }
    Ok(value)
}

#[cfg(test)]
mod internal_tests {
    use super::*;

    #[test]
    fn oversize_declaration_is_rejected_before_payload_arrives() {
        let mut prefix = [0u8; 4];
        prefix.copy_from_slice(&((MAX_FRAME_BYTES as u32) + 1).to_le_bytes());
        assert_eq!(
            peek_frame_len(&prefix),
            Err(FrameError::TooLarge {
                declared: MAX_FRAME_BYTES + 1
            })
        );
    }

    #[test]
    fn truncated_prefix_reports_what_it_saw() {
        assert_eq!(
            peek_frame_len(&[1, 0]),
            Err(FrameError::TruncatedPrefix { available: 2 })
        );
    }

    #[test]
    fn smuggling_two_payloads_in_one_frame_is_a_length_mismatch() {
        let framed = encode_frame(&7u32).expect("encode");
        let mut doubled = framed.clone();
        doubled.extend_from_slice(&framed);
        assert!(matches!(
            decode_frame::<u32>(&doubled),
            Err(FrameError::LengthMismatch { .. })
        ));
    }

    #[test]
    fn short_payload_is_reported_not_padded() {
        let framed = encode_frame(&"hello".to_string()).expect("encode");
        let short = &framed[..framed.len() - 1];
        assert!(matches!(
            decode_frame::<String>(short),
            Err(FrameError::TruncatedPayload { .. })
        ));
    }
}
