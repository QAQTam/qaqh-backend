//! 私有桌面(codex desktop.rs 同款)。
//!
//! 受限子进程连接桌面也要过 DACL 写检查,默认桌面的 ACL 没有 capability SID
//! → CreateProcessAsUserW 直接 ERROR_ACCESS_DENIED(0x80070005)。
//! 解法:每次运行建一个 DACL 只授 capability SID 的私有桌面,
//! 子进程的窗口/控制台全部落在里面(顺带获得窗口消息隔离)。

use windows::core::{PCWSTR, Result};
use windows::Win32::Foundation::{CloseHandle, GENERIC_ALL, HANDLE};
use windows::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::System::StationsAndDesktops::{DESKTOP_CONTROL_FLAGS, CreateDesktopW};

pub struct Desktop {
    handle: HANDLE,
    name: String,
}

impl Desktop {
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl Drop for Desktop {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

/// SDDL_REVISION_1
const SDDL_REVISION_1: u32 = 1;

/// 创建 DACL 仅授 `cap_sid_text`(如 "S-1-5-21-...")GENERIC_ALL 的私有桌面。
pub fn create_private_desktop(cap_sid_text: &str) -> Result<Desktop> {
    // 名字唯一性:同进程并行创建不能撞名(撞名时 CreateDesktopW 报 ACCESS_DENIED)
    static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let sddl = format!("D:(A;;GA;;;{cap_sid_text})");
    let sddl_w: Vec<u16> = sddl.encode_utf16().chain([0]).collect();
    let name = format!(
        "sbx-desktop-{}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        crate::events::now_millis()
    );
    let name_w: Vec<u16> = name.encode_utf16().chain([0]).collect();

    unsafe {
        let mut psd = PSECURITY_DESCRIPTOR::default();
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl_w.as_ptr()),
            SDDL_REVISION_1,
            &mut psd,
            None,
        )?;
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: psd.0,
            bInheritHandle: false.into(),
        };
        let created =
            CreateDesktopW(PCWSTR(name_w.as_ptr()), PCWSTR::null(), None, DESKTOP_CONTROL_FLAGS(0), GENERIC_ALL.0, Some(&sa));
        local_free_sd(&psd);
        let hdesk = created?;
        Ok(Desktop {
            handle: HANDLE(hdesk.0 as _),
            name,
        })
    }
}

unsafe fn local_free_sd(psd: &PSECURITY_DESCRIPTOR) {
    unsafe {
        if !psd.0.is_null() {
            use windows::Win32::Foundation::{HLOCAL, LocalFree};
            let _ = LocalFree(HLOCAL(psd.0));
        }
    }
}
