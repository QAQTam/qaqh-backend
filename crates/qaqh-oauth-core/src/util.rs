//! 基础件：随机量、UUIDv4、常数时间比较、base64url、PKCE S256。
//!
//! 本模块是 `qaqh-siwc/src/util.rs` 与 `qaqh-dsaccount/src/util.rs` 两份
//! 自述「同一套实现」的拷贝的**唯一归宿**（v4.5 P2-1b，MUST.md 复制粘贴
//! 清剿项；两 crate 的重复文件已物理删除）。
//!
//! 随机数一律走 `aws_lc_rs::rand::SystemRandom`（与各 provider 的验签/摘要
//! 同一来源）；UUIDv4 走 `uuid` crate；常数时间比较走 `subtle`（P0-3：手写
//! 实现与「熵源失败降级补丁」一并退役——SystemRandom 失败即 panic，绝不
//! 静默降级成「时间戳+栈地址」拼出的伪随机，那是拿安全漏洞掩盖系统故障，
//! 宪法 #5 零容忍）。

use aws_lc_rs::rand::{SecureRandom, SystemRandom};
use subtle::ConstantTimeEq;

/// URL-safe base64（无 padding）随机串。`n` 为**字节数**，输出长度约 `4n/3`。
///
/// # Panics
/// 系统 CSPRNG（`SystemRandom::fill`）失败时 panic——熵源故障意味着进程环境
/// 已不可信，PKCE verifier / state / nonce 绝不允许降级生成，故障必须显性暴露。
pub fn random_urlsafe(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    SystemRandom::new()
        .fill(&mut buf)
        .expect("system CSPRNG failure: refusing to derive OAuth secrets without entropy");
    base64url_encode(&buf)
}

/// 一次性事务量（state / nonce / PKCE verifier）：32 字节熵 → 43 字符 URL-safe。
pub fn random_transaction_value() -> String {
    random_urlsafe(32)
}

/// RFC 4122 v4 UUID 文本形态（host 标识 / `device_id` / `attempt_id` 用）。
/// 不是凭据，只要求唯一性；`uuid` crate 的 v4 走系统熵源。
pub fn random_uuid_v4() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// 常数时间比较（`subtle`，长度不等直接 false——长度本身不是秘密）。
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    bool::from(a.ct_eq(b))
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

/// `code_challenge = BASE64URL(SHA256(ASCII(code_verifier)))`（RFC 7636 §4.2，S256）。
pub fn pkce_challenge(verifier: &str) -> String {
    let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, verifier.as_bytes());
    base64url_encode(digest.as_ref())
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
        for raw in [
            vec![],
            vec![1u8],
            vec![0xff, 0x00, 0x7f, 0x80],
            b"hello world".to_vec(),
        ] {
            let encoded = base64url_encode(&raw);
            assert_eq!(base64url_decode(&encoded).expect("decode"), raw, "{encoded}");
        }
        // 带 padding 的输入同样接受（JWT/JWKS 常混用）。
        assert_eq!(base64url_decode("Zm9vYg==").expect("padded"), b"foob");
        assert!(base64url_decode("a").is_err());
        assert!(base64url_decode("****").is_err());
    }

    #[test]
    fn random_values_stay_in_the_urlsafe_alphabet() {
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
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
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
        assert!(matches!(variant, '8' | '9' | 'a' | 'b'), "variant 位: {first}");
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

    #[test]
    fn pkce_challenge_matches_rfc7636_appendix_b() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }
}
