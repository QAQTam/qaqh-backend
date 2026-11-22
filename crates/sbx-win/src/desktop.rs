//! 私有桌面(codex desktop.rs 同款)。
//!
//! 受限子进程连接桌面也要过 DACL 写检查,默认桌面的 ACL 没有 capability SID
//! → CreateProcessAsUserW 直接 ERROR_ACCESS_DENIED(0x80070005)。
//! 解法:每次运行建一个 DACL 只授 capability SID 的私有桌面,
//! 子进程的窗口/控制台全部落在里面(顺带获得窗口消息隔离)。

use windows::Win32::Foundation::GENERIC_ALL;
use windows::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, CreateDesktopW, DESKTOP_CONTROL_FLAGS, HDESK,
};
use windows::core::{PCWSTR, Result};

pub struct Desktop {
    handle: HDESK,
    name: String,
}

unsafe impl Send for Desktop {}
unsafe impl Sync for Desktop {}

impl Desktop {
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl Drop for Desktop {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseDesktop(self.handle);
        }
    }
}

/// SDDL_REVISION_1
const SDDL_REVISION_1: u32 = 1;

/// 创建私有桌面：DACL 同时授权当前用户 SID 与 `cap_sid_text`。
///
/// 必须同时包含当前用户 SID 与 capability SID：Windows 对 `WRITE_RESTRICTED`
/// 令牌做双通道 AccessCheck（主 SID 列表 + Restricting SID 列表必须同时命中），
/// 缺当前用户 SID 会导致任何静态链接 `user32.dll` 的子进程（如 pwsh/python/node）
/// 在 `DllMain` 连接桌面时直接以 `0xC0000142 (STATUS_DLL_INIT_FAILED)` 崩溃。
pub fn create_private_desktop(cap_sid_text: &str) -> Result<Desktop> {
    // 名字唯一性:同进程并行创建不能撞名(撞名时 CreateDesktopW 报 ACCESS_DENIED)
    static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let user_sid_text = crate::token::process_user_sid()
        .map(|bytes| crate::sid::sid_to_string(&bytes))
        .ok();
    let sddl = match user_sid_text {
        Some(user_sid) if !user_sid.eq_ignore_ascii_case(cap_sid_text) => {
            format!("D:P(A;;GA;;;{user_sid})(A;;GA;;;{cap_sid_text})")
        }
        _ => format!("D:P(A;;GA;;;{cap_sid_text})"),
    };
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
        let created = CreateDesktopW(
            PCWSTR(name_w.as_ptr()),
            PCWSTR::null(),
            None,
            DESKTOP_CONTROL_FLAGS(0),
            GENERIC_ALL.0,
            Some(&sa),
        );
        local_free_sd(&psd);
        let hdesk = created?;
        Ok(Desktop {
            handle: hdesk,
            name,
        })
    }
}

unsafe fn local_free_sd(psd: &PSECURITY_DESCRIPTOR) {
    unsafe {
        if !psd.0.is_null() {
            use windows::Win32::Foundation::{HLOCAL, LocalFree};
            let _ = LocalFree(Some(HLOCAL(psd.0)));
        }
    }
}
