//! ULID 生成(uuid v7 字节 → Crockford base32,26 字符)。
//!
//! 自 canonical/identity.rs 迁入的最小基础函数;canonical 框架随 SQLite
//! 存储切换删除,业务 ID 生成保留。

use uuid::Uuid;

pub fn generate_ulid() -> String {
    encode_ulid_bytes(*Uuid::now_v7().as_bytes())
}

fn encode_ulid_bytes(bytes: [u8; 16]) -> String {
    const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let value = u128::from_be_bytes(bytes);
    let mut out = String::with_capacity(26);
    for position in (0..26).rev() {
        let index = ((value >> (position * 5)) & 0x1f) as usize;
        out.push(ALPHABET[index] as char);
    }
    out
}
