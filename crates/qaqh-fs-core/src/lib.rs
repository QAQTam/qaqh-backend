//! qaqh-fs-core — 文件系统核心（P2 crate 拆分，研究文档 §4-d）。
//!
//! 自 `qaqh-workspace` 拆出的纯 FS 层：共享 IO/路径助手（[`file_shared`]）、
//! 行对齐读取缓存（[`file_cache`]）、文件状态账本（[`file_state`]）。
//! 文件工具组（read/edit/grep/apply_patch…）与门面均经本 crate 使用。

pub mod file_cache;
pub mod file_shared;
pub mod file_state;

pub use file_shared::{
    content_hash, is_binary_read_error, normalize_newlines, LineIndex, READ_MAX_BYTES,
    READ_MAX_CHARS, READ_MAX_LINES,
};

/// Unit tests mutate process-wide runtime state. Keep those mutations
/// deterministic even when the Rust test harness runs modules in parallel.
#[cfg(test)]
pub static TEST_RUNTIME_SERIAL: std::sync::LazyLock<std::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(()));
