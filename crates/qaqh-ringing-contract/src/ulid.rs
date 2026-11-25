//! ULID 生成(128-bit → Crockford base32,26 字符)。
//!
//! 自 canonical/identity.rs 迁入的最小基础函数;canonical 框架随 SQLite
//! 存储切换删除,业务 ID 生成保留。
//!
//! v4.5 P2-2a(plan §三.4-6):生成与编码本体收编 `ulid` crate——
//! `generate_ulid` 用 `Ulid::generate()`(48-bit ms 时间戳 + 80-bit 随机;
//! plan 原文写的 `Ulid::new()` 在 ulid-3.0 API 中不存在,按 crate 实际 API
//! 落地,见 PR 登记),替代此前借道 `uuid::Uuid::now_v7` 的伪 ULID——v7 的
//! version/variant 固定 nibble 落在随机段里,本就违反 ULID 80-bit 全随机布局;
//! `ulid_from_text` 用 `Ulid::from_bytes(..).to_string()`;手写
//! 手写 base32 位循环物理删除。26 字符 Crockford base32 线契约与
//! `is_ulid_text` 校验逐字不变(`ulid` crate 的字母表/大端序与本模块退役前
//! 完全一致,确定性派生结果同字节同文本)。

use sha2::{Digest, Sha256};

use crate::fact::is_ulid_text;

pub fn generate_ulid() -> String {
    ulid::Ulid::generate().to_string()
}

/// 任意文本 → 稳定 ULID 别名(sha256 前 16 字节)。
///
/// v1 model providers 用任意 tool-call ID(`call_1`、`tc_...`),而需要 ULID
/// 形状的地方(turn/call/interaction id)用同一确定性派生,不改 wire 契约。
pub fn ulid_from_text(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    ulid::Ulid::from_bytes(bytes).to_string()
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
