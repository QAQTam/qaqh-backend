//! NT 路径形态规范化(spec §6.3 的纯函数核心)。
//!
//! v0 范围:前缀归一(`\\?\`、`\??\`、`\\?\UNC\`)、正斜杠折反斜杠、
//! 连续分隔符折叠、大小写折叠、尾部空点修剪;8.3 短名与 `\Device\`
//! 卷形态识别为未解析,调用方按 deny(PathUnresolved) 处置(spec §4.2 规则 1)。
//! 卷符号映射(QueryDosDeviceW)待 hook 平面接入时补齐。

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NormalizeError {
    /// 空路径、裸盘符等无法解析的形态
    Malformed(String),
    /// 8.3 短名(`~N`)或 `\Device\HarddiskVolumeN\` 形态:v0 离线不可解析
    Unresolved(String),
}

impl fmt::Display for NormalizeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NormalizeError::Malformed(s) => write!(f, "malformed path: {s:?}"),
            NormalizeError::Unresolved(s) => write!(f, "path requires runtime resolution: {s:?}"),
        }
    }
}

impl std::error::Error for NormalizeError {}

/// 规范化为小写、反斜杠、无 Win32/NT 前缀的路径形态。
/// UNC 路径保持 `\\server\share\...` 形态原样返回。
pub fn normalize(input: &str) -> Result<String, NormalizeError> {
    let s = input.trim();
    if s.is_empty() {
        return Err(NormalizeError::Malformed(input.to_string()));
    }

    let mut s = strip_prefix_forms(s);

    if s.starts_with("\\Device\\") || s.starts_with("\\DEVICE\\") {
        return Err(NormalizeError::Unresolved(input.to_string()));
    }

    // 正斜杠折反斜杠 + 连续分隔符折叠(UNC 前缀的 `\\` 保留)
    let mut folded = String::with_capacity(s.len());
    let mut body = s.as_str();
    if body.starts_with("\\\\") {
        folded.push_str("\\\\");
        body = &body[2..];
    }
    let mut prev_sep = body.is_empty();
    for ch in body.chars() {
        if ch == '/' || ch == '\\' {
            if !prev_sep {
                folded.push('\\');
                prev_sep = true;
            }
        } else {
            folded.push(ch);
            prev_sep = false;
        }
    }
    s = folded;

    // 大小写折叠(Windows 路径比较语义的保守近似)
    let mut s = s.to_lowercase();

    // 尾部分隔符 / 尾点 / 尾空格;裸盘符("c:")保持带分隔符
    while s.len() > 2 && s.ends_with(['\\', '.', ' ']) {
        s.pop();
    }
    if s.ends_with(':') {
        s.push('\\');
    }

    // 8.3 短名检测:任一分量形如 `XXX~N`
    if s.split('\\').any(is_short_name_component) {
        return Err(NormalizeError::Unresolved(input.to_string()));
    }

    if !s.contains(':') && !s.starts_with("\\\\") {
        return Err(NormalizeError::Malformed(input.to_string()));
    }

    Ok(s)
}

/// 前缀判定:`root` 是否包含 `path`(含相等)。两侧必须先过 `normalize`。
pub fn is_within(root: &str, path: &str) -> bool {
    if root == path {
        return true;
    }
    let r = root.trim_end_matches('\\');
    path.strip_prefix(r).is_some_and(|rest| rest.starts_with('\\'))
}

fn strip_prefix_forms(s: &str) -> String {
    for p in ["\\\\?\\UNC\\", "\\??\\UNC\\"] {
        if let Some(rest) = s.strip_prefix(p) {
            return format!("\\\\{rest}");
        }
    }
    for p in ["\\\\?\\", "\\??\\"] {
        if let Some(rest) = s.strip_prefix(p) {
            return rest.to_string();
        }
    }
    s.to_string()
}

/// `PROGRA~1` / `ABCDEF~12` 形态:波浪线后 1-2 位纯数字,且波浪线前非空。
fn is_short_name_component(comp: &str) -> bool {
    match comp.rsplit_once('~') {
        Some((stem, digits)) => {
            !stem.is_empty()
                && (1..=2).contains(&digits.len())
                && digits.bytes().all(|b| b.is_ascii_digit())
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(s: &str) -> String {
        normalize(s).unwrap()
    }

    #[test]
    fn basic_forms() {
        assert_eq!(n("C:\\a\\b"), "c:\\a\\b");
        assert_eq!(n("c:/a/b/"), "c:\\a\\b");
        assert_eq!(n("\\\\?\\C:\\a\\B"), "c:\\a\\b");
        assert_eq!(n("\\??\\C:\\a"), "c:\\a");
        assert_eq!(n("C:\\a\\\\b\\"), "c:\\a\\b");
        assert_eq!(n("  C:\\A\\b.  "), "c:\\a\\b");
    }

    #[test]
    fn unc_kept() {
        assert_eq!(n("\\\\?\\UNC\\server\\share\\x"), "\\\\server\\share\\x");
        assert_eq!(n("\\\\SERVER\\Share\\X"), "\\\\server\\share\\x");
    }

    #[test]
    fn root_drive() {
        assert_eq!(n("C:\\"), "c:\\");
        assert_eq!(n("C:"), "c:\\");
    }

    #[test]
    fn unresolved_forms() {
        assert!(matches!(normalize("C:\\PROGRA~1\\x"), Err(NormalizeError::Unresolved(_))));
        assert!(matches!(normalize("\\Device\\HarddiskVolume3\\a"), Err(NormalizeError::Unresolved(_))));
    }

    #[test]
    fn malformed() {
        assert!(matches!(normalize(""), Err(NormalizeError::Malformed(_))));
        assert!(matches!(normalize("relative\\path"), Err(NormalizeError::Malformed(_))));
    }

    #[test]
    fn containment() {
        assert!(is_within("c:\\ws", "c:\\ws\\a.txt"));
        assert!(is_within("c:\\ws", "c:\\ws"));
        assert!(is_within("c:\\ws", "c:\\workspace\\f") == false);
        assert!(is_within("c:\\ws", "c:\\wsx\\f") == false);
    }
}
