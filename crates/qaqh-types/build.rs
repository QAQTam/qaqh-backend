fn main() {
    println!("cargo:rerun-if-env-changed=QAQH_BUILD_VERSION");
    let build_version = std::env::var("QAQH_BUILD_VERSION")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
    // UA 契约(见 platform.rs):对外暴露固定版本号。收口在唯一注入点。
    println!("cargo:rustc-env=QAQH_BUILD_VERSION={build_version}");
}
