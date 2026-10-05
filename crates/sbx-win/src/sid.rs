//! SID 构造/解析/生成。二进制布局自建(避免 ntapi 依赖),
//! 随机性用 SystemPrng(不引入第三方 rand)。

use windows::Win32::Security::Cryptography::ProcessPrng;

pub const SID_REVISION: u8 = 1;

/// SECURITY_NT_AUTHORITY (S-1-5-...) / SECURITY_WORLD_SID_AUTHORITY (S-1-1-0)
pub const NT_AUTHORITY: [u8; 6] = [0, 0, 0, 0, 0, 5];
pub const WORLD_AUTHORITY: [u8; 6] = [0, 0, 0, 0, 0, 1];

/// 按二进制 SID 布局拼装:Revision(1) + SubAuthorityCount(1) + IdentifierAuthority(6) + SubAuthorities(4×N, LE)
pub fn build_sid(authority: &[u8; 6], subauthorities: &[u32]) -> Vec<u8> {
    let mut v = Vec::with_capacity(8 + 4 * subauthorities.len());
    v.push(SID_REVISION);
    v.push(subauthorities.len() as u8);
    v.extend_from_slice(authority);
    for s in subauthorities {
        v.extend_from_slice(&s.to_le_bytes());
    }
    v
}

/// Everyone(S-1-1-0):受限令牌的兜底 restricting SID——
/// DACL 授予 Everyone 的写(如临时目录)对受限检查可见。
pub fn everyone_sid() -> Vec<u8> {
    build_sid(&WORLD_AUTHORITY, &[0])
}

/// 生成 capability SID:S-1-5-21-<a>-<b>-<c>-<rid>,各段非零随机。
/// 随机源用 ProcessPrng(SystemPrng 在部分新系统已不再导出,ProcessPrng 为其继任)。
/// 按 workspace 持久化属 M1 正式版(spike 阶段每次运行新生成,
/// DACL 里是否已有同 SID 的 ACE 由 acl::has_ace 幂等兜底)。
pub fn capability_sid() -> std::result::Result<Vec<u8>, String> {
    let mut buf = [0u8; 20];
    if !unsafe { ProcessPrng(&mut buf) }.as_bool() {
        return Err("ProcessPrng failed".to_string());
    }
    let word = |i: usize| {
        let mut w = u32::from_le_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]);
        if w == 0 {
            w = 1;
        }
        w
    };
    let rid = 1000 + (word(16) % 60000);
    Ok(build_sid(&NT_AUTHORITY, &[21, word(0), word(4), word(8), rid]))
}

/// "S-1-5-21-a-b-c-r" 字符串解析(供 --cap-sid 复用)。
pub fn parse_sid(text: &str) -> Result<Vec<u8>, String> {
    let parts: Vec<&str> = text.trim().split('-').collect();
    if parts.len() < 3 || parts[0] != "S" {
        return Err(format!("not a SID string: {text:?}"));
    }
    let revision: u8 = parts[1].parse().map_err(|e| format!("{e}"))?;
    // IdentifierAuthority 是 48 位大端,字符串形态是十进制单值
    let authority_val: u64 = parts[2].parse().map_err(|e| format!("{e}"))?;
    let subs: Vec<u32> = parts[3..]
        .iter()
        .map(|p| p.parse::<u32>().map_err(|e| format!("{e}")))
        .collect::<Result<_, _>>()?;
    let mut v = Vec::with_capacity(8 + 4 * subs.len());
    v.push(revision);
    v.push(subs.len() as u8);
    v.extend_from_slice(&authority_val.to_be_bytes()[2..8]);
    for s in &subs {
        v.extend_from_slice(&s.to_le_bytes());
    }
    Ok(v)
}

/// "S-1-5-21-a-b-c-r" 展示形态(手工格式化,不走 ConvertSidToStringSidW)。
pub fn sid_to_string(sid: &[u8]) -> String {
    if sid.len() < 8 || sid[1] as usize * 4 + 8 != sid.len() {
        return "<invalid sid>".to_string();
    }
    let mut authority: u64 = 0;
    for &b in &sid[2..8] {
        authority = (authority << 8) | b as u64;
    }
    let subs: Vec<String> = sid[8..]
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]).to_string())
        .collect();
    format!("S-{}-{}-{}", sid[0], authority, subs.join("-"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn everyone_layout() {
        let s = everyone_sid();
        assert_eq!(sid_to_string(&s), "S-1-1-0");
        assert_eq!(parse_sid("S-1-1-0").unwrap(), s);
    }

    #[test]
    fn capability_shape() {
        let s = capability_sid().unwrap();
        let text = sid_to_string(&s);
        assert!(text.starts_with("S-1-5-21-"), "{text}");
        assert_eq!(parse_sid(&text).unwrap(), s);
    }

    #[test]
    fn parse_roundtrip() {
        let text = "S-1-5-21-1234567890-2345678901-3456789012-4242";
        let sid = parse_sid(text).unwrap();
        assert_eq!(sid_to_string(&sid), text);
    }
}
