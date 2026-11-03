fn main() {
    println!("cargo:rerun-if-env-changed=QAQH_BUILD_VERSION");
    let build_version = std::env::var("QAQH_BUILD_VERSION")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
    // UA 契约(见 platform.rs):对外只暴露不带构建后缀的稳定版本
    // (`2.1.20261031-1` → `2.1.20261031`)。收口在唯一注入点。
    let release_version = build_version
        .split_once('-')
        .map_or(build_version.as_str(), |(version, _)| version);
    println!("cargo:rustc-env=QAQH_BUILD_VERSION={release_version}");
}
