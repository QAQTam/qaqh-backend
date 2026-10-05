//! 最小子进程环境(spec §5.5 重指表的 v0 子集)。
//!
//! junction 农场(.gitconfig 等只读链接)列 M1 后续;v0 已知降级:
//! HOME/USERPROFILE 指向 scratch\home,原 home 其余内容读不到。

use std::path::Path;

/// 直通白名单(direct.rs minimal_child_env 的子集:受限令牌下必需项)。
const PASSTHROUGH: &[&str] = &[
    "SystemRoot",
    "SystemDrive",
    "ComSpec",
    "PATH",
    "PATHEXT",
    "PROCESSOR_ARCHITECTURE",
    "NUMBER_OF_PROCESSORS",
    "OS",
    "ProgramData",
    "ProgramFiles",
    "ProgramFiles(x86)",
    "WINDIR",
];

/// 构造子进程环境。`scratch_tmp`/`scratch_home` 由调用方先行创建。
pub fn minimal_child_env(scratch_tmp: &Path, scratch_home: &Path) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = PASSTHROUGH
        .iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
        .collect();
    let tmp = scratch_tmp.display().to_string();
    env.push(("TEMP".into(), tmp.clone()));
    env.push(("TMP".into(), tmp));
    env.push(("USERPROFILE".into(), scratch_home.display().to_string()));
    // git/cargo 一族先查 HOME 再查 USERPROFILE,缺 HOME 时会回探真实家目录
    env.push(("HOME".into(), scratch_home.display().to_string()));
    env
}
