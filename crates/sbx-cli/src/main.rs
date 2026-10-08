//! sbx CLI 入口。
//!
//! sbx 是 Windows-only 工具（TokenPlane 受限令牌 + ProjFS 重定向），全部实现见
//! [`win`]。非 Windows 平台提供占位 main，使 `cargo check/clippy/test --workspace`
//! 在 macOS/Linux 上可编、可跑——否则本 crate 无条件引用 `sbx_win::*` 会让整个
//! workspace 门禁在这些平台编不过。

#[cfg(windows)]
mod win;

#[cfg(windows)]
fn main() {
    win::main();
}

#[cfg(not(windows))]
fn main() {
    eprintln!("sbx is a Windows-only tool (TokenPlane + ProjFS); not available on this platform");
    std::process::exit(2);
}
