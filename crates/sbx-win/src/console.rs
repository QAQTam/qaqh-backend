//! 子进程输出解码:管道字节流优先按严格 UTF-8 解释(git/现代工具),
//! 失败回退 OEM 代码页(cmd/powershell 的历史行为;中文系统为 GBK/936)。

use windows::Win32::Globalization::{GetOEMCP, MB_PRECOMPOSED, MultiByteToWideChar};
use windows::Win32::System::Console::SetConsoleOutputCP;

/// 控制台输出切 UTF-8(重定向下无效,由消费方决定编码)。
pub fn enable_utf8_console() {
    unsafe {
        let _ = SetConsoleOutputCP(65001);
    }
}

pub fn decode_console_bytes(bytes: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    let cp = unsafe { GetOEMCP() };
    let needed = unsafe { MultiByteToWideChar(cp, MB_PRECOMPOSED, bytes, None) };
    if needed <= 0 {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let mut wide = vec![0u16; needed as usize];
    let written = unsafe { MultiByteToWideChar(cp, MB_PRECOMPOSED, bytes, Some(&mut wide)) };
    if written <= 0 {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    String::from_utf16_lossy(&wide[..written as usize])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_passthrough() {
        assert_eq!(decode_console_bytes("拒绝访问".as_bytes()), "拒绝访问");
    }

    #[test]
    fn oem_gbk_roundtrip() {
        // 仅在 OEM 代码页为 936(简体中文)的机器上做双向断言
        let cp = unsafe { GetOEMCP() };
        if cp != 936 {
            return;
        }
        let src: Vec<u16> = "拒绝访问".encode_utf16().collect();
        let mut gbk = [0u8; 32];
        let n = unsafe {
            windows::Win32::Globalization::WideCharToMultiByte(
                936,
                0,
                &src,
                Some(&mut gbk),
                windows::core::PCSTR::null(),
                None,
            )
        };
        assert!(n > 0);
        assert_eq!(decode_console_bytes(&gbk[..n as usize]), "拒绝访问");
    }
}
