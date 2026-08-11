//! deepx-app-notifications — Microsoft.Windows.AppNotifications（Windows App SDK）
//! 的 WinMD 绑定与 DeepX 高层封装。
//!
//! 绑定由 build.rs 从已安装的 Windows App Runtime 的 WinMD 生成
//! （该命名空间不在 windows-rs 的 feature 集内）。
//!
//! Phase 0 spike：验证 IsSupported / Register / Show / NotificationInvoked
//! 完整链路（unpackaged + 自包含部署）。

#![allow(
    non_snake_case,
    non_camel_case_types,
    non_upper_case_globals,
    clippy::all,
    unused_imports
)]

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

// 顶层重导出：spike / 上层代码直接用 AppNotificationManager 等类型。
pub use Microsoft::Windows::AppNotifications::*;

// ═══════════════════════════════════════════════════════════════════
// 高层封装：Notifier
// ═══════════════════════════════════════════════════════════════════

use std::sync::mpsc;

/// 初始化结果。
pub enum InitOutcome {
    /// 可用（已注册事件 + Register）。
    Supported(Notifier),
    /// 平台不支持（IsSupported() == false）。
    Unsupported,
    /// 初始化失败（激活/注册），附原因。
    Failed(String),
}

impl std::fmt::Debug for InitOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InitOutcome::Supported(_) => f.write_str("Supported(Notifier)"),
            InitOutcome::Unsupported => f.write_str("Unsupported"),
            InitOutcome::Failed(msg) => f.debug_tuple("Failed").field(msg).finish(),
        }
    }
}

/// 桌面通知器。持有 manager 与事件 revoker（保活回调）。
pub struct Notifier {
    mgr: AppNotificationManager,
    _revoker: windows_core::EventRevoker,
}

// AppNotificationManager 为 agile COM 对象，引用计数线程安全；fork
// windows-core 的 IUnknown 未标 Send/Sync（保守），此处显式声明——
// Notifier 仅含 COM 句柄，跨线程移动/共享安全（回调线程持有）。
unsafe impl Send for Notifier {}
unsafe impl Sync for Notifier {}

impl Notifier {
    /// 初始化通知器。
    ///
    /// 先试**直连**（自包含部署的链接期 auto-initializer 生效时无需 Bootstrap）；
    /// 失败则加载 `Microsoft.WindowsAppRuntime.Bootstrap.dll` 调
    /// `MddBootstrapInitialize`（2.x 导出名）后重试——unpackaged 的标准路径。
    ///
    /// `on_invoked`：通知被点击时回调（参数 = `Argument()`，可为 None），
    /// 在专用线程上触发；需 `Send + 'static`。
    pub fn init(on_invoked: impl Fn(Option<String>) + Send + Sync + 'static) -> InitOutcome {
        if !AppNotificationManager::IsSupported().unwrap_or(false) {
            return InitOutcome::Unsupported;
        }
        let on_invoked: std::sync::Arc<dyn Fn(Option<String>) + Send + Sync + 'static> =
            std::sync::Arc::new(on_invoked);
        match try_init(on_invoked.clone()) {
            ok @ (InitOutcome::Supported(_) | InitOutcome::Unsupported) => ok,
            InitOutcome::Failed(first) => match try_init_bootstrap(on_invoked) {
                InitOutcome::Failed(second) => {
                    InitOutcome::Failed(format!("direct: {first}; bootstrap: {second}"))
                }
                other => other,
            },
        }
    }

    /// 弹一条通知。返回是否成功。
    pub fn show(&self, title: &str, body: &str) -> bool {
        let xml = format!(
            "<toast><visual><binding template=\"ToastGeneric\">\
             <text>{}</text><text>{}</text>\
             </binding></visual></toast>",
            escape_xml(title),
            escape_xml(body)
        );
        let Ok(notification) = AppNotification::CreateInstance(&windows_core::HSTRING::from(xml))
        else {
            return false;
        };
        self.mgr.Show(&notification).is_ok()
    }
}

fn try_init(on_invoked: std::sync::Arc<dyn Fn(Option<String>) + Send + Sync + 'static>) -> InitOutcome {
    let mgr = match AppNotificationManager::Default() {
        Ok(m) => m,
        Err(e) => return InitOutcome::Failed(format!("Default: {e:?}")),
    };
    // 事件处理器必须先于 Register() 注册（否则 0x80070490）。
    let (tx, rx) = mpsc::channel::<Option<String>>();
    let revoker = match mgr.NotificationInvoked(move |_sender, args| {
        let arg = args
            .as_ref()
            .and_then(|a| a.Argument().ok())
            .map(|s| s.to_string_lossy());
        let _ = tx.send(arg);
    }) {
        Ok(r) => r,
        Err(e) => return InitOutcome::Failed(format!("NotificationInvoked: {e:?}")),
    };
    if let Err(e) = mgr.Register() {
        return InitOutcome::Failed(format!("Register: {e:?}"));
    }
    std::thread::Builder::new()
        .name("deepx-notif-invoked".into())
        .spawn(move || {
            while let Ok(arg) = rx.recv() {
                on_invoked(arg);
            }
        })
        .expect("spawn notif callback thread");
    InitOutcome::Supported(Notifier {
        mgr,
        _revoker: revoker,
    })
}

/// Bootstrap 重试路径：加载 Bootstrap.dll 并调 MddBootstrapInitialize。
fn try_init_bootstrap(
    on_invoked: std::sync::Arc<dyn Fn(Option<String>) + Send + Sync + 'static>,
) -> InitOutcome {
    unsafe {
        let h = windows::Win32::libloaderapi::LoadLibraryW(&windows_core::HSTRING::from(
            "Microsoft.WindowsAppRuntime.Bootstrap.dll",
        ));
        if h.0.is_null() {
            return InitOutcome::Failed("Bootstrap.dll 未找到".into());
        }
        let Some(addr) = windows::Win32::libloaderapi::GetProcAddress(
            h,
            windows::core::s!("MddBootstrapInitialize"),
        ) else {
            return InitOutcome::Failed("MddBootstrapInitialize 导出未找到".into());
        };
        type MddBootstrapInit = unsafe extern "system" fn(u32, *const u16, u32) -> i32;
        let init: MddBootstrapInit = std::mem::transmute(addr);
        // majorMinorVersion = 2.3 (0x00020003)；versionTag = release；minVersion = 0。
        let hr = init(0x0002_0003, std::ptr::null(), 0);
        if hr < 0 {
            return InitOutcome::Failed(format!("MddBootstrapInitialize hr={hr:#010x}"));
        }
    }
    try_init(on_invoked)
}

fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
