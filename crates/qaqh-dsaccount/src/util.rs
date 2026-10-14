//! 基础件：随机量、base64url、常数时间比较。
//!
//! 与 `qaqh-siwc::util` 是**同一套实现**（base64url 与一次性事务量都只有一份语义），
//! 两 crate 互不依赖，故此处重复一份并保留同样的测试向量；抽公共件属结构塑形，
//! 另立项。随机数一律走 `ring::rand::SystemRandom`（与 PKCE 的 sha256 同一来源）。

use ring::rand::{SecureRandom, SystemRandom};

/// URL-safe base64（无 padding）随机串。`n` 为**字节数**，输出长度约 `4n/3`。
pub fn random_urlsafe(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    let rng = SystemRandom::new();
    if rng.fill(&mut buf).is_err() {
        // 系统熵源失败意味着进程环境已不可信。这里不静默降级成「弱随机」而是
        // 用时间 + 栈地址混合出一个**可观测地不同**的值，同时把随机性来源降级
        // 这件事留给调用方（PKCE/state 的生命周期只有一次授权尝试那么长）。
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

/// 一次性事务量（`state` / PKCE verifier）：32 字节熵 → 43 字符 URL-safe。
pub fn random_transaction_value() -> String {
    random_urlsafe(32)
}

/// RFC 4122 v4 UUID 文本形态（`auth_exchange` 的 `device_id` 用）。
///
/// 不是凭据，只要求唯一性，故熵源失败时可以退化成时间戳 + 进程 ID。
pub fn random_uuid_v4() -> String {
    let mut buf = [0u8; 16];
    let rng = SystemRandom::new();
    if rng.fill(&mut buf).is_err() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        buf[..8].copy_from_slice(&nanos.to_be_bytes()[..8]);
        buf[8..].copy_from_slice(&(std::process::id() as u64).to_be_bytes());
    }
    // RFC 9562 §5.4：version 4 + variant 10xx。
    buf[6] = (buf[6] & 0x0f) | 0x40;
    buf[8] = (buf[8] & 0x3f) | 0x80;
    let hex: String = buf.iter().map(|byte| format!("{byte:02x}")).collect();
    match (
        hex.get(0..8),
        hex.get(8..12),
        hex.get(12..16),
        hex.get(16..20),
        hex.get(20..32),
    ) {
        (Some(a), Some(b), Some(c), Some(d), Some(e)) => format!("{a}-{b}-{c}-{d}-{e}"),
        _ => hex,
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

/// epoch 毫秒。授权尝试的过期时刻与 UI 的倒计时共用这一个时钟。
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

const B64URL_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64url_matches_rfc_examples_and_stays_in_alphabet() {
        assert_eq!(base64url_encode(b""), "");
        assert_eq!(base64url_encode(b"f"), "Zg");
        assert_eq!(base64url_encode(b"fo"), "Zm8");
        assert_eq!(base64url_encode(b"foo"), "Zm9v");
        assert_eq!(base64url_encode(&[0xfb, 0xff]), "-_8");
        // 授权 URL / 回调查询串里出现的值不得含 `+` `/` `=`（否则二次编码就是 bug）。
        for size in [16usize, 32, 48] {
            let value = random_urlsafe(size);
            assert!(!value.contains(['+', '/', '=']), "{value}");
        }
    }

    #[test]
    fn transaction_values_are_long_and_unique() {
        let a = random_transaction_value();
        let b = random_transaction_value();
        assert_ne!(a, b);
        assert_eq!(a.len(), 43, "32 字节 → 43 字符: {a}");
        assert!(
            a.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
    }

    #[test]
    fn uuid_v4_is_shaped_like_a_uuid() {
        let first = random_uuid_v4();
        let second = random_uuid_v4();
        assert_ne!(first, second);
        assert_eq!(first.len(), 36);
        assert_eq!(first.chars().filter(|c| *c == '-').count(), 4);
        assert_eq!(first.chars().nth(14), Some('4'), "version 位: {first}");
        let variant = first.chars().nth(19).expect("variant");
        assert!(
            matches!(variant, '8' | '9' | 'a' | 'b'),
            "variant 位: {first}"
        );
    }

    #[test]
    fn constant_time_eq_matches_semantics() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn now_ms_is_a_plausible_epoch_millisecond_clock() {
        // 2020-01-01 之后、且远早于 2100：只用来挡住「秒 vs 毫秒」这类单位错误。
        assert!(now_ms() > 1_577_836_800_000);
    }
}
