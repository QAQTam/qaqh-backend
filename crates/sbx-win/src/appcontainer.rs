//! AppContainer profile 生命周期(ADR-0002 第二隔离后端)。
//!
//! 非 packaged AppContainer:CreateAppContainerProfile 免提权(写 HKCU 与
//! %LOCALAPPDATA%\Packages),得到 S-1-15-2-<8 段> 的 AppContainer SID。
//! 空 capability 列表 = 无 internetClient 等能力 = 网络内核级拒绝;
//! 同时全部访问(含读)都要过 AC-SID DACL 检查 → 读隔离天然成立。
//!
//! 命名:sbx + fnv1a64(workspace_key) 的 16 hex —— 同工作区稳定复用,
//! 不依赖第三方哈希(仓宪法第 1 条)。SID 落入 sidstore,与 TokenPlane 的
//! cap-SID 台账/cleanup 机制完全同构(ACLE 恢复按逐行 sid)。

use windows::core::{w, PCWSTR, Result};
use windows::Win32::Foundation::{HLOCAL, LocalFree};
use windows::Win32::Security::Cryptography::ProcessPrng;
use windows::Win32::Security::{GetSidSubAuthorityCount, PSID};
use windows::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeleteAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
};

pub struct AppContainerProfile {
    pub name: String,
    /// "S-1-15-2-..." 展示形态
    pub sid_text: String,
    /// 二进制 SID(SDDL/DACL 机器形态)
    pub sid: Vec<u8>,
}

/// FNV-1a 64(依赖白名单内自实现,避免引入 rand/fnv)
pub fn fnv1a64(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// 由 workspace_key 派生确定性容器名(≤64 字符,仅 [0-9a-z])。
pub fn container_name(workspace_key: &str) -> String {
    let mut name = format!("sbx{:016x}", fnv1a64(workspace_key.as_bytes()));
    if name.len() > 64 {
        name.truncate(64);
    }
    name
}

/// 确保容器存在并返回其 SID。已存在则 Derive(幂等),否则创建(空 capability)。
pub fn ensure_profile(name: &str) -> Result<AppContainerProfile> {
    let name_w: Vec<u16> = name.encode_utf16().chain([0]).collect();
    let psid = match unsafe { DeriveAppContainerSidFromAppContainerName(PCWSTR(name_w.as_ptr())) } {
        Ok(psid) => psid,
        Err(_) => unsafe {
            // 空 capability 列表 = 无网络/设备等能力,内核强制
            CreateAppContainerProfile(
                PCWSTR(name_w.as_ptr()),
                PCWSTR(name_w.as_ptr()),
                w!("sbx app container"),
                None,
            )?
        },
    };
    let sid = unsafe { psid_bytes(psid) };
    let _ = unsafe { LocalFree(HLOCAL(psid.0)) };
    Ok(AppContainerProfile {
        name: name.to_string(),
        sid_text: crate::sid::sid_to_string(&sid),
        sid,
    })
}

/// 删除容器 profile(cleanup 可选调用;残留 ACE 在 SID 消失后天然失效)。
pub fn delete_profile(name: &str) -> Result<()> {
    let name_w: Vec<u16> = name.encode_utf16().chain([0]).collect();
    unsafe { DeleteAppContainerProfile(PCWSTR(name_w.as_ptr())) }
}

/// 从 PSID 读出二进制 SID(Revision/Count 头 + SubAuthorities)。
unsafe fn psid_bytes(psid: PSID) -> Vec<u8> {
    unsafe {
        let count = *GetSidSubAuthorityCount(psid) as usize;
        let len = 8 + 4 * count;
        let mut v = Vec::with_capacity(len);
        v.extend_from_slice(std::slice::from_raw_parts(psid.0 as *const u8, len));
        v
    }
}

/// 一次性随机段(测试容器名防并发冲突)
pub fn random_suffix() -> String {
    let mut buf = [0u8; 4];
    unsafe {
        let _ = ProcessPrng(&mut buf);
    }
    format!("{:02x}{:02x}{:02x}{:02x}", buf[0], buf[1], buf[2], buf[3])
}

/// capability 名称 → 二进制 SID(policy.capabilities 的解析点)。
///
/// 白名单只收网络三件套("断网但按需放开"是唯一已证用例);其他能力一律
/// 用原始 S-1-15-3-* SID 字符串表达(Microsoft "App capability SIDs" 表),
/// 未知名称报错——fail-closed,绝不静默忽略。
pub fn capability_sid_from_name(name: &str) -> std::result::Result<Vec<u8>, String> {
    const WELLKNOWN: &[(&str, &str)] = &[
        ("internetClient", "S-1-15-3-1"),
        ("internetClientServer", "S-1-15-3-2"),
        ("privateNetworkClientServer", "S-1-15-3-3"),
    ];
    if let Some((_, sid_text)) = WELLKNOWN.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)) {
        return crate::sid::parse_sid(sid_text);
    }
    // 原始 SID 直通:仅接受 AppContainer capability 家族(S-1-15-3-*)
    if name.starts_with("S-1-15-3-") {
        return crate::sid::parse_sid(name);
    }
    Err(format!(
        "unknown capability {name:?}: 可用 well-known 名(internetClient/internetClientServer/privateNetworkClientServer)或原始 S-1-15-3-* SID"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_name_is_stable_and_alnum() {
        let a = container_name("AB01");
        let b = container_name("AB01");
        assert_eq!(a, b);
        assert!(a.starts_with("sbx"));
        assert!(a.len() <= 64);
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric()));
        assert_ne!(a, container_name("CD02"));
    }

    #[test]
    fn ensure_profile_is_idempotent_and_derives_sid() {
        let name = format!("sbxtest{}", random_suffix());
        let p1 = ensure_profile(&name).unwrap();
        assert!(p1.sid_text.starts_with("S-1-15-2-"), "{}", p1.sid_text);
        let p2 = ensure_profile(&name).unwrap();
        assert_eq!(p1.sid, p2.sid, "Derive 应返回同一 SID");
        delete_profile(&name).unwrap();
    }

    #[test]
    fn capability_names_resolve_and_unknown_fail_closed() {
        assert_eq!(
            capability_sid_from_name("internetClient").unwrap(),
            crate::sid::parse_sid("S-1-15-3-1").unwrap()
        );
        // 大小写不敏感
        assert_eq!(
            capability_sid_from_name("InternetClientServer").unwrap(),
            crate::sid::parse_sid("S-1-15-3-2").unwrap()
        );
        // 原始 capability SID 直通(含 capability-group 形态)
        let raw = "S-1-15-3-1024-1065365936-1281604716-3521732928-1150926906-3434589291-628479260-3460056385-3255099947";
        assert_eq!(
            capability_sid_from_name(raw).unwrap(),
            crate::sid::parse_sid(raw).unwrap()
        );
        // 白名单外的名称与 SID 家族一律拒绝(fail-closed)
        assert!(capability_sid_from_name("runFullTrust").is_err());
        assert!(capability_sid_from_name("S-1-5-21-1-2-3-4").is_err());
        assert!(capability_sid_from_name("S-1-1-0").is_err());
        assert!(capability_sid_from_name("").is_err());
    }
}
