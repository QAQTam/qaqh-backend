//! build.rs — 从已安装的 Windows App Runtime 的 WinMD 生成
//! `Microsoft.Windows.AppNotifications` 绑定（该命名空间不在 windows-rs
//! 的 feature 集内，需自定义生成）。
//!
//! 输入：
//! - `DEEPX_WINMD_DIR`（可选）：指向含 AppNotifications WinMD 的目录；
//!   缺省时自动探测 `C:\Program Files\WindowsApps\Microsoft.WindowsAppRuntime.*`。
//! - 默认 Windows 元数据（`input_default`）：fork windows-rs 的 metadata/winrt。

use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    // 1. 定位 Windows App Runtime 的 WinMD 目录
    let winmd_dir = std::env::var("DEEPX_WINMD_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| find_runtime_winmd_dir());
    assert!(
        winmd_dir.join("Microsoft.Windows.AppNotifications.winmd").exists(),
        "deepx-app-notifications: {winmd_dir:?} lacks Microsoft.Windows.AppNotifications.winmd"
    );

    // 2. 生成绑定（只保留 AppNotifications 命名空间）
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR");
    let out_file = Path::new(&out_dir).join("bindings.rs");
    windows_bindgen::Bindgen::new()
        .input(&winmd_dir)
        .input_default()
        .filters([
            "Microsoft.Windows.AppNotifications",
            "Windows.Foundation.TypedEventHandler",
        ])
        .output(&out_file)
        .write();

    // 3. WinMD 变化时重新生成
    if let Ok(entries) = std::fs::read_dir(&winmd_dir) {
        for entry in entries.flatten() {
            println!("cargo:rerun-if-changed={}", entry.path().display());
        }
    }
}

/// 探测已安装的 Windows App Runtime 包目录：
/// `C:\Program Files\WindowsApps\Microsoft.WindowsAppRuntime.*\`
/// 取含 AppNotifications WinMD 的最新版本（2.x > 1.x，按目录名排序）。
fn find_runtime_winmd_dir() -> PathBuf {
    let apps_dir = Path::new(r"C:\Program Files\WindowsApps");
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(apps_dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("Microsoft.WindowsAppRuntime."))
                })
                .collect()
        })
        .unwrap_or_default();
    candidates.sort();
    for dir in candidates.iter().rev() {
        if dir.join("Microsoft.Windows.AppNotifications.winmd").exists() {
            return dir.clone();
        }
    }
    panic!(
        "deepx-app-notifications: cannot locate Microsoft.Windows.AppNotifications.winmd. \
         Install Windows App Runtime (or set DEEPX_WINMD_DIR)."
    );
}
