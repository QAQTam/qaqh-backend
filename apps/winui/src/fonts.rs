//! 系统字体枚举：读取 Windows 注册表字体列表。
//!
//! 数据源：`HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Fonts`，
//! 值名形如 `"Arial (TrueType)"` / `"Microsoft YaHei & Microsoft YaHei UI
//! (TrueType)"`，值数据为字体文件名。
//!
//! 本仓库链接的 `windows` crate 为裁剪版（无 Win32::System::Registry 模块），
//! 因此这里用最小 FFI 直连 `advapi32.dll`（RegOpenKeyExW / RegEnumValueW /
//! RegCloseKey），仅枚举值名，不读取文件路径。
//!
//! 解析规则：
//! - 仅收集带 `(TrueType)` / `(OpenType)` 后缀的项（排除 .fon 位图字体）；
//! - `"A & B"` 复合项拆分为两个候选族名；
//! - 剥离尾部字重/字型变体词（Bold/Italic/Light/…），只保留族名；
//! - 去重 + 按名称排序。

use std::ffi::c_void;
use std::ptr;
use std::sync::OnceLock;

/// 进程级缓存：首次调用枚举注册表，之后直接返回（字体列表不常变）。
static CACHE: OnceLock<Vec<String>> = OnceLock::new();

/// 返回系统字体族列表（带缓存）。失败/空系统返回空列表，
/// 调用方应把空值解释为「仅系统默认」。
pub fn system_fonts_cached() -> &'static Vec<String> {
    CACHE.get_or_init(system_fonts)
}

/// `HKEY_LOCAL_MACHINE`。
const HKEY_LOCAL_MACHINE: *mut c_void = 0x8000_0002usize as *mut c_void;
/// `KEY_READ`（STANDARD_RIGHTS_READ | KEY_QUERY_VALUE | KEY_ENUMERATE_SUB_KEYS）。
const KEY_READ: u32 = 0x0002_0019;
const ERROR_SUCCESS: i32 = 0;
const ERROR_NO_MORE_ITEMS: i32 = 259;

#[link(name = "advapi32")]
unsafe extern "system" {
    fn RegOpenKeyExW(
        hkey: *mut c_void,
        lp_sub_key: *const u16,
        ul_options: u32,
        sam_desired: u32,
        phk_result: *mut *mut c_void,
    ) -> i32;
    fn RegEnumValueW(
        hkey: *mut c_void,
        dw_index: u32,
        lp_value_name: *mut u16,
        lpcch_value_name: *mut u32,
        lp_reserved: *mut c_void,
        lp_type: *mut u32,
        lp_data: *mut u8,
        lpcb_data: *mut u32,
    ) -> i32;
    fn RegCloseKey(hkey: *mut c_void) -> i32;
}

/// 尾部变体词（按长度降序，先剥离长词再剥离短词）。
const VARIANT_SUFFIXES: [&str; 27] = [
    " ExtraBold Italic",
    " ExtraLight Italic",
    " SemiBold Italic",
    " Semibold Italic",
    " SemiLight Italic",
    " Semilight Italic",
    " DemiBold Italic",
    " Bold Italic",
    " Light Italic",
    " Thin Italic",
    " Black Italic",
    " Medium Italic",
    " ExtraBold",
    " ExtraLight",
    " SemiBold",
    " Semibold",
    " SemiLight",
    " Semilight",
    " DemiBold",
    " Bold",
    " Italic",
    " Light",
    " Medium",
    " Black",
    " Thin",
    " Regular",
    " Heavy",
];

/// 枚举系统已安装字体族名（去重 + 排序）。失败/空系统返回空列表，
/// 调用方应把空值解释为「仅系统默认」。
pub fn system_fonts() -> Vec<String> {
    let mut fonts: Vec<String> = Vec::new();
    let sub_key: Vec<u16> = "SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion\\Fonts"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    unsafe {
        let mut key: *mut c_void = ptr::null_mut();
        if RegOpenKeyExW(HKEY_LOCAL_MACHINE, sub_key.as_ptr(), 0, KEY_READ, &mut key)
            != ERROR_SUCCESS
        {
            return fonts;
        }

        let mut index: u32 = 0;
        loop {
            let mut name_buf = [0u16; 512];
            let mut name_len: u32 = 512;
            let rc = RegEnumValueW(
                key,
                index,
                name_buf.as_mut_ptr(),
                &mut name_len,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            );
            if rc == ERROR_NO_MORE_ITEMS {
                break;
            }
            index += 1;
            if rc != ERROR_SUCCESS {
                continue;
            }
            let value_name = String::from_utf16_lossy(&name_buf[..name_len as usize]);
            for family in parse_font_name(&value_name) {
                if !fonts.contains(&family) {
                    fonts.push(family);
                }
            }
        }
        RegCloseKey(key);
    }

    fonts.sort();
    fonts
}

/// 解析注册表值名 → 候选族名列表。
///
/// 仅接受带 `(TrueType)` / `(OpenType)` 标记的项；`"A & B"` 复合项拆分为
/// 多个候选；每个候选剥离尾部变体词得到族名。
fn parse_font_name(value_name: &str) -> Vec<String> {
    let base = value_name
        .strip_suffix(" (TrueType)")
        .or_else(|| value_name.strip_suffix(" (OpenType)"))
        .unwrap_or(value_name);
    // 无 TrueType/OpenType 标记（如 .fon 位图字体）→ 排除。
    if base == value_name {
        return Vec::new();
    }
    base.split(" & ")
        .map(|s| strip_variant(s.trim()))
        .filter(|s| !s.is_empty())
        .collect()
}

/// 剥离尾部字重/字型变体词，保留族名（"Calibri Light Italic" → "Calibri"）。
fn strip_variant(name: &str) -> String {
    let mut out = name.to_string();
    loop {
        let mut stripped = false;
        for sfx in VARIANT_SUFFIXES {
            if out.ends_with(sfx) {
                out.truncate(out.len() - sfx.len());
                stripped = true;
            }
        }
        if !stripped {
            break;
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_true_type_names() {
        assert_eq!(parse_font_name("Arial (TrueType)"), vec!["Arial"]);
        assert_eq!(
            parse_font_name("Microsoft YaHei & Microsoft YaHei UI (TrueType)"),
            vec!["Microsoft YaHei", "Microsoft YaHei UI"]
        );
        assert_eq!(parse_font_name("Segoe UI (OpenType)"), vec!["Segoe UI"]);
    }

    #[test]
    fn rejects_non_true_type_entries() {
        // .fon 位图字体等无 TrueType/OpenType 标记 → 排除。
        assert!(parse_font_name("Modern").is_empty());
        assert!(parse_font_name("Roman").is_empty());
    }

    #[test]
    fn strips_variant_suffixes() {
        assert_eq!(strip_variant("Calibri Light Italic"), "Calibri");
        assert_eq!(strip_variant("Arial Bold"), "Arial");
        assert_eq!(strip_variant("Segoe UI Semibold Italic"), "Segoe UI");
        assert_eq!(strip_variant("Consolas"), "Consolas");
        assert_eq!(strip_variant("Cascadia Code"), "Cascadia Code");
    }
}
