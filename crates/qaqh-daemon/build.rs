fn main() {
    println!("cargo:rerun-if-env-changed=QAQH_BUILD_ID");
    println!("cargo:rerun-if-env-changed=QAQH_BUILD_VERSION");
    println!("cargo:rerun-if-env-changed=QAQH_CHANNEL");

    embed_windows_icon();

    let build_id = std::env::var("QAQH_BUILD_ID")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
    println!("cargo:rustc-env=QAQH_BUILD_ID={build_id}");

    let build_version = std::env::var("QAQH_BUILD_VERSION")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
    println!("cargo:rustc-env=QAQH_BUILD_VERSION={build_version}");
}

/// Embed the product icon (and basic file metadata) into Windows executables.
/// `winresource` 仅在 Windows target 下作为 build-dependency 声明；
/// 非 Windows 编译期不可见，必须用 cfg 门控（而非仅运行时早退）。
#[cfg(target_os = "windows")]
fn embed_windows_icon() {
    const ICON_PATH: &str = "../../assets/qaqh-harness.ico";
    println!("cargo:rerun-if-changed={ICON_PATH}");
    let mut res = winresource::WindowsResource::new();
    res.set_icon(ICON_PATH);
    res.set("FileDescription", "QAQ-Harness Daemon");
    res.set("ProductName", "QAQ-Harness");
    res.compile()
        .expect("failed to compile Windows resources (icon)");
}

#[cfg(not(target_os = "windows"))]
fn embed_windows_icon() {}
