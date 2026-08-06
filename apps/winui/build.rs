//! Stages the Windows App SDK runtime next to the built executable.
//!
//! Self-contained mode downloads `Microsoft.WindowsAppSDK.Runtime` +
//! `Microsoft.Web.WebView2` (Core.dll) from NuGet on first build, so the app
//! runs without a system-installed Windows App SDK runtime.

fn main() {
    windows_reactor_setup::as_self_contained();
}
