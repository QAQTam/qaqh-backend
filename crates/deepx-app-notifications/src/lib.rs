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
