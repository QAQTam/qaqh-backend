fn main() {
    println!("cargo:rerun-if-env-changed=QAQH_BUILD_VERSION");
    let build_version = std::env::var("QAQH_BUILD_VERSION")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
    println!("cargo:rustc-env=QAQH_BUILD_VERSION={build_version}");
}
