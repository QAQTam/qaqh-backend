//! ULID 生成(uuid v7 字节 → Crockford base32,26 字符)。
//!
//! 自 canonical/identity.rs 迁入的最小基础函数;canonical 框架随 SQLite
//! 存储切换删除,业务 ID 生成保留。

use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::fact::is_ulid_text;

pub fn generate_ulid() -> String {
    encode_ulid_bytes(*Uuid::now_v7().as_bytes())
}

/// 任意文本 → 稳定 ULID 别名(sha256 前 16 字节)。
///
/// v1 model providers 用任意 tool-call ID(`call_1`、`tc_...`),而需要 ULID
/// 形状的地方(turn/call/interaction id)用同一确定性派生,不改 wire 契约。
pub fn ulid_from_text(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    encode_ulid_bytes(bytes)
}

/// 客户端 `command_id` → causation id:ULID 原样,异形 id 确定性派生。
/// 移动端提交 UUID,回执折叠用同一函数保证 causation 可反查回执。
pub fn causation_for_command(command_id: &str) -> Option<String> {
    if command_id.is_empty() {
        return None;
    }
    if is_ulid_text(command_id) {
        return Some(command_id.to_string());
    }
    Some(ulid_from_text(command_id))
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
