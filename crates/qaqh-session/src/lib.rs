//! qaqh-session — SQLite 业务权威：`Db`（单写执行器 + schema 阶梯 + sidecar
//! 吸入）与 `OwnerDirectory`（会话 owner 目录 + 身份生成 + 资源清理）。
//! v4.4 P1-3：`SessionManager` 已物理删除，职责并入 `OwnerDirectory`。

#![cfg_attr(not(test), forbid(unsafe_code))]
pub mod db;
pub mod grouping;
pub mod owner;
#[cfg(test)]
mod owner_tests;

/// Free-function epoch helper.
pub fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before Unix epoch")
        .as_secs()
}
