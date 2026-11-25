//! P2-2a parity：`ulid` crate 换手写 `encode_ulid_bytes` 的逐字符等价性。
//!
//! ULID 是 wire 契约（turn/call/interaction id 的 26 字符 Crockford Base32
//! 形状，受 `is_ulid_text` 校验）。本测试复刻退役前的手写位循环，对固定字节
//! 语料断言新实现（`ulid::Ulid::from_bytes(..).to_string()`，即
//! `ulid_from_text` 的内层）与旧实现逐字符一致——保证同字节 → 同文本。

use sha2::{Digest, Sha256};

use qaqh_ringing_contract::fact::is_ulid_text;
use qaqh_ringing_contract::ulid::{generate_ulid, ulid_from_text};

/// 退役前 `ulid::encode_ulid_bytes` 逐字复刻。
fn old_encode_ulid_bytes(bytes: [u8; 16]) -> String {
    const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let value = u128::from_be_bytes(bytes);
    let mut out = String::with_capacity(26);
    for position in (0..26).rev() {
        let index = ((value >> (position * 5)) & 0x1f) as usize;
        out.push(ALPHABET[index] as char);
    }
    out
}

#[test]
fn ulid_from_text_matches_retired_encode_byte_for_byte() {
    // `ulid_from_text` 取 sha256 前 16 字节 → ULID。复刻同一派生链验证等价。
    let corpus = ["", "call_1", "tc_abc", "0", "ses_01ABC", "界面路径/中文"];
    for value in corpus {
        let digest = Sha256::digest(value.as_bytes());
        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(&digest[..16]);
        let expected = old_encode_ulid_bytes(bytes);
        assert_eq!(ulid_from_text(value), expected, "value: {value:?}");
    }
}

#[test]
fn ulid_text_shape_is_stable_and_crockford() {
    // 确定性派生：同文本 → 同 ULID，且过 `is_ulid_text`（wire 契约）。
    let a = ulid_from_text("call_42");
    let b = ulid_from_text("call_42");
    assert_eq!(a, b);
    assert!(is_ulid_text(&a), "{a}");
}

#[test]
fn generate_ulid_passes_crockford_validator() {
    for _ in 0..100 {
        let id = generate_ulid();
        assert!(is_ulid_text(&id), "generate_ulid produced non-conforming: {id}");
    }
}

#[test]
fn generated_ulids_are_unique() {
    // 同毫秒批量仍唯一（80-bit 全随机段——正是退役 uuidv7 伪 ULID 的理由：
    // v7 的 version/variant 固定 nibble 侵占随机段）。跨毫秒时间戳前缀单调，
    // 同毫秒内 `Ulid::generate` 刻意不保证随机段单调（crate 文档明示，
    // 需单调请走 `Generator`）——本仓 id 只要求唯一 + 粗排序，契约不变。
    let mut seen = std::collections::HashSet::new();
    for _ in 0..1000 {
        let id = generate_ulid();
        assert!(seen.insert(id.clone()), "duplicate ULID: {id}");
    }
}
