//! Phase 0 spike：验证 App Notifications 完整链路。
//!
//! 流程：IsSupported → Register → NotificationInvoked 回调 → Show →
//! 等待点击（25s 超时自动退出）。全部结果打印到 stdout。
//!
//! 运行：cargo run -p deepx-app-notifications --example spike

use deepx_app_notifications::*;
use windows::core::Result;
use windows::Win32::ro::{RoInitialize, RO_INIT_SINGLETHREADED};
use windows_core::HSTRING;

fn main() -> Result<()> {
    // Bootstrap 初始化：unpackaged 应用绑定已安装的 Windows App Runtime。
    // 自包含部署（DeepX.exe 场景）由 Microsoft.WindowsAppRuntime.dll
    // auto-initializer 完成，无需此步；spike 是裸 exe，需要显式 Bootstrap。
    // DEEPX_SPIKE_NO_BOOTSTRAP=1 时跳过（模拟自包含场景对照）。
    let skip_bootstrap = std::env::var("DEEPX_SPIKE_NO_BOOTSTRAP").is_ok_and(|v| v == "1");
    if !skip_bootstrap {
        unsafe {
            let h = windows::Win32::libloaderapi::LoadLibraryW(&HSTRING::from(
                "Microsoft.WindowsAppRuntime.Bootstrap.dll",
            ));
            if h.0.is_null() {
                println!("[0] Bootstrap.dll 未找到（跳过）");
            } else {
                type MddBootstrapInit = unsafe extern "system" fn(u32, *const u16, u32) -> i32;
                let addr = windows::Win32::libloaderapi::GetProcAddress(
                    h,
                    windows::core::s!("MddBootstrapInitialize"),
                );
                if let Some(addr) = addr {
                    let init: MddBootstrapInit = std::mem::transmute(addr);
                    // majorMinorVersion=2.3 (0x00020003), versionTag=release, minVersion=0
                    let hr = init(0x0002_0003, std::ptr::null(), 0);
                    println!("[0] MddBootstrapInitialize hr={hr:#010x}");
                } else {
                    println!("[0] MddBootstrapInitialize 导出未找到");
                }
            }
        }
    } else {
        println!("[0] Bootstrap 跳过（自包含对照）");
    }

    // WinRT 初始化（STA）——控制台进程需要；WinUI 进程内已有。
    let hr = unsafe { RoInitialize(RO_INIT_SINGLETHREADED) };
    if hr.is_err() {
        println!("[0] RoInitialize failed: {hr:?}");
        return Ok(());
    }
    let wr_loaded = unsafe {
        windows::Win32::libloaderapi::GetModuleHandleW(&HSTRING::from(
            "Microsoft.WindowsAppRuntime.dll",
        ))
    };
    println!(
        "[0] Microsoft.WindowsAppRuntime.dll loaded = {}",
        !wr_loaded.0.is_null()
    );
    if wr_loaded.0.is_null() {
        // 显式加载（自包含/延迟加载场景的对照实验）
        let h = unsafe {
            windows::Win32::libloaderapi::LoadLibraryW(&HSTRING::from(
                "Microsoft.WindowsAppRuntime.dll",
            ))
        };
        println!("[0] manual LoadLibrary(WindowsAppRuntime) -> {:?}", h.0);
    }

    // 1. IsSupported —— 干净机器验证点（Singleton 注册决定 true/false）
    let supported = AppNotificationManager::IsSupported()?;
    println!("[1] IsSupported = {supported}");

    let mgr = AppNotificationManager::Default()?;

    // 3. NotificationInvoked —— 点击通知的回调（必须在 Register 之前注册）
    let _revoker = mgr.NotificationInvoked(|_sender, args| {
        let arg = args.as_ref().and_then(|a| a.Argument().ok());
        println!("[3] NotificationInvoked! Argument={arg:?}");
    })?;
    println!("[3] Invoked handler registered");

    // 2. Register —— unpackaged 应用注册 AUMID
    mgr.Register()?;
    println!("[2] Register OK");

    // 4. Show —— 弹一条通知（payload 是 XML）
    let xml = "<toast><visual><binding template=\"ToastGeneric\">\
        <text>DeepX spike</text><text>Phase 0 通知链路验证（点我触发回调）</text>\
        </binding></visual></toast>";
    let notification = AppNotification::CreateInstance(&HSTRING::from(xml))?;
    mgr.Show(&notification)?;
    println!("[4] Show OK — 25s 内点击通知验证回调");

    std::thread::sleep(std::time::Duration::from_secs(25));
    println!("[5] timeout — done");
    Ok(())
}
