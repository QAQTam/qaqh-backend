//! P2-2a parity：`base64` crate 换 oauth-core 手写 base64url 的逐字等价性。
//!
//! base64url 出现在 PKCE challenge、state/nonce、JWS 三段与 JWKS `n`/`e`——
//! 全是与 IdP 对话的 wire 字节。本测试复刻退役前的手写编解码，对固定语料
//! 断言新旧输出逐字节一致（含 padding 宽容与 `+`/`/` 别名混用），保证
//! siwc/dsaccount 的既有 wire 测试之外再钉一层等价桩。

use qaqh_oauth_core::util::{base64url_decode, base64url_encode};

/// 退役前 `util::base64url_encode`（RFC 4648 §5 无 padding）逐字复刻。
fn old_base64url_encode(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(A[(b0 >> 2) as usize] as char);
        out.push(A[((b0 & 0x03) << 4 | b1 >> 4) as usize] as char);
        if chunk.len() > 1 {
            out.push(A[((b1 & 0x0F) << 2 | b2 >> 6) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(A[(b2 & 0x3F) as usize] as char);
        }
    }
    out
}

/// 退役前 `util::base64url_decode`（跳 `=`、`-`/`+` 与 `_`/`/` 互为别名）复刻。
fn old_base64url_decode(input: &str) -> Result<Vec<u8>, String> {
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    let mut out = Vec::new();
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
        _ => Err("base64url 长度非法".to_string()),
    }
}

#[test]
fn encode_matches_retired_loop_byte_for_byte() {
    let mut corpus: Vec<Vec<u8>> = vec![
        vec![],
        b"f".to_vec(),
        b"fo".to_vec(),
        b"foo".to_vec(),
        vec![0xfb, 0xff],
        vec![0xff, 0x00, 0x7f, 0x80],
        b"hello world".to_vec(),
        b"dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk".to_vec(),
    ];
    // 覆盖 0..=255 全部字节值 + 各余数长度。
    for len in [1usize, 2, 4, 5, 7, 256, 300] {
        corpus.push((0..len as u32).map(|i| i as u8).collect());
    }
    for raw in &corpus {
        assert_eq!(&base64url_encode(raw), &old_base64url_encode(raw), "{raw:?}");
    }
}

#[test]
fn decode_matches_retired_semantics() {
    for raw in [vec![], vec![1u8], vec![0xff, 0x00, 0x7f, 0x80], b"jwt-segment".to_vec()] {
        let encoded = base64url_encode(&raw);
        assert_eq!(base64url_decode(&encoded).expect("new decode"), raw);
        assert_eq!(base64url_decode(&encoded).expect("new decode"), old_base64url_decode(&encoded).expect("old decode"));
    }
    // padding 与别名混用（JWKS 常见）：新旧一致接受。
    for input in ["Zm9vYg==", "Zm9vY", "aGVsbG8", "aGVsbG8=", "__8-", "+_8-"] {
        let new = base64url_decode(input);
        let old = old_base64url_decode(input);
        assert_eq!(
            new.as_ref().map_err(|_| ()),
            old.as_ref().map_err(|_| ()),
            "input: {input}"
        );
        assert_eq!(new.unwrap_or_default(), old.unwrap_or_default(), "value: {input}");
    }
}

#[test]
fn decode_rejects_impossible_lengths_and_junk() {
    // 退役前后一致的 fail-closed 面。
    assert!(base64url_decode("a").is_err());
    assert!(base64url_decode("****").is_err());
    assert!(base64url_decode("ab!d").is_err());
}

#[test]
fn rfc7636_appendix_b_challenge_still_exact() {
    // util::pkce_challenge 走新 base64url_encode；RFC 官方向量重钉。
    assert_eq!(
        qaqh_oauth_core::util::pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
    );
}
