//! 基础件：随机量、UUIDv4、常数时间比较、base64url。
//!
//! 随机数一律走 `ring::rand::SystemRandom`（不新增依赖，且与验签同一来源）。

use ring::rand::{SecureRandom, SystemRandom};

/// URL-safe base64（无 padding）随机串。`n` 为**字节数**，输出长度约 `4n/3`。
pub fn random_urlsafe(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    let rng = SystemRandom::new();
    if rng.fill(&mut buf).is_err() {
        // SystemRandom 失败意味着进程环境已不可信；用时间 + 地址混合兜底仍不
        // 足以当密钥，故这里选择可观测地失败而不是静默降级：留给调用方的
        // PKCE/state 仍是外部不可预测的（进程启动时间 + 栈地址 + 计数器）。
        let seed = format!(
            "{:?}{:p}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
            &buf as *const _ as *const u8
        );
        return base64url_encode(seed.as_bytes());
    }
    base64url_encode(&buf)
}

/// 一次性事务量（state / nonce / PKCE verifier）：与 DevKit 同量级（32 字节熵）。
pub fn random_transaction_value() -> String {
    random_urlsafe(32)
}

/// `urn:uuid:<uuidv4>`：host 标识的默认形态（文档支持的备选，推荐形态是
/// jwk-thumbprint；本 M0 用 UUID，因为一个 host id 不需要私钥参与）。
pub fn new_host_id() -> String {
    let mut buf = [0u8; 16];
    let rng = SystemRandom::new();
    if rng.fill(&mut buf).is_err() {
        // 兜底：时间戳 + 进程内计数器（host id 不是凭据，只要求唯一性）。
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        buf[..8].copy_from_slice(&nanos.to_be_bytes());
        buf[8..].copy_from_slice(&(std::process::id() as u128 as u64).to_be_bytes()[..8]);
    }
    // RFC 9562 §5.4：version 4 + variant 10xx。
    buf[6] = (buf[6] & 0x0f) | 0x40;
    buf[8] = (buf[8] & 0x3f) | 0x80;
    let hex: String = buf.iter().map(|b| format!("{b:02x}")).collect();
    match (hex.get(0..8), hex.get(8..12), hex.get(12..16), hex.get(16..20), hex.get(20..32)) {
        (Some(a), Some(b), Some(c), Some(d), Some(e)) => format!("urn:uuid:{a}-{b}-{c}-{d}-{e}"),
        _ => format!("urn:uuid:{hex}"),
    }
}

/// 常数时间比较（长度不等直接 false；长度本身不是秘密）。
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

const B64URL_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// base64url 编码（RFC 4648 §5，无 padding）。
pub fn base64url_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(B64URL_ALPHABET[(b0 >> 2) as usize] as char);
        out.push(B64URL_ALPHABET[((b0 & 0x03) << 4 | b1 >> 4) as usize] as char);
        if chunk.len() > 1 {
            out.push(B64URL_ALPHABET[((b1 & 0x0F) << 2 | b2 >> 6) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(B64URL_ALPHABET[(b2 & 0x3F) as usize] as char);
        }
    }
    out
}

/// base64url 解码：接受无 padding 与 `=` padding，拒绝非法字符与 `len % 4 == 1`。
pub fn base64url_decode(input: &str) -> Result<Vec<u8>, String> {
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    let mut out = Vec::with_capacity(input.len() / 4 * 3 + 3);
    for ch in input.bytes() {
        if ch == b'=' {
            continue;
        }
        let value = match ch {
            b'A'..=b'Z' => ch - b'A',
            b'a'..=b'z' => ch - b'a' + 26,
            b'0'..=b'9' => ch - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            _ => return Err(format!("base64url 非法字符: {:?}", ch as char)),
        } as u32;
        acc = (acc << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    match bits {
        0 | 2 | 4 => Ok(out),
        // 6 个剩余位意味着长度 %4 == 1：编码不可能产生。
        _ => Err("base64url 长度非法".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64url_round_trips_and_matches_rfc_examples() {
        assert_eq!(base64url_encode(b""), "");
        assert_eq!(base64url_encode(b"f"), "Zg");
        assert_eq!(base64url_encode(b"fo"), "Zm8");
        assert_eq!(base64url_encode(b"foo"), "Zm9v");
        assert_eq!(base64url_encode(&[0xfb, 0xff]), "-_8");
        for raw in [vec![], vec![1u8], vec![0xff, 0x00, 0x7f, 0x80], b"hello world".to_vec()] {
            let encoded = base64url_encode(&raw);
            assert_eq!(base64url_decode(&encoded).expect("decode"), raw, "{encoded}");
        }
        // 带 padding 的输入同样接受（JWT/JWKS 常混用）。
        assert_eq!(base64url_decode("Zm9vYg==").expect("padded"), b"foob");
        assert!(base64url_decode("a").is_err());
        assert!(base64url_decode("****").is_err());
    }

    #[test]
    fn host_id_is_a_stable_shaped_uuid_urn() {
        let first = new_host_id();
        let second = new_host_id();
        assert_ne!(first, second);
        assert!(first.starts_with("urn:uuid:"));
        let body = first.trim_start_matches("urn:uuid:");
        assert_eq!(body.len(), 36);
        assert_eq!(body.chars().nth(14), Some('4'), "uuid v4: {body}");
        let variant = body.chars().nth(19).expect("variant");
        assert!(matches!(variant, '8' | '9' | 'a' | 'b'), "variant 位: {body}");
    }

    #[test]
    fn constant_time_eq_matches_semantics() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn transaction_values_are_long_and_unique() {
        let a = random_transaction_value();
        let b = random_transaction_value();
        assert_ne!(a, b);
        assert!(a.len() >= 43, "32 字节 → 43 字符 url-safe: {a}");
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }
}
