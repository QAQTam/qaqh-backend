//! 凭据 JSON 的原子落盘（provider 无关的机械层）。
//!
//! 两条纪律（两个 provider crate 旧拷贝的合一；v4.5 P2-2a 起机制收编
//! `tempfile::NamedTempFile`，plan §三.4-3——手写 `.json.tmp` 定名 + rename
//! 与 Windows「先删目标再 rename」补丁一并退役，`persist` 在 Windows 内部即
//! 带 REPLACE_EXISTING 的原子移动），天然覆盖既有目标）：
//!
//! 1. **同目录唯一临时文件 + 原子替换**：绝不就地截断写——半截文件在
//!    「损坏即空启动」策略下等于凭空丢登录；
//! 2. **尽力 0600**：Unix 上收紧权限位；Windows 上真正的保护是 DPAPI 密文
//!    本身（密文盒留在 `qaqh-config::secrets`，由 provider crate 使用）；
//!    失败路径由 `NamedTempFile` 的 `Drop` 清理临时文件（dsaccount 实证语义）。

use std::path::Path;

/// 失败阶段（错误码映射与文案归 provider crate）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreFailure {
    CreateDir { parent: String, reason: String },
    WriteTemp { temp: String, reason: String },
    Rename { path: String, reason: String },
}

/// 同目录唯一临时文件 + sync + 原子替换（见模块文档）。
///
/// 错误契约不变：`StoreFailure{parent,temp,path,reason}` 的字段文本由
/// provider 层做逐字文案映射（siwc `写临时文件失败 {temp}: {reason}` 等），
/// 本函数只保证 `reason` 是底层 IO 错误原文。
pub fn atomic_write_json(path: &Path, text: &str) -> Result<(), StoreFailure> {
    let parent = path.parent().unwrap_or(Path::new("."));
    if !parent.as_os_str().is_empty() {
        std::fs::create_dir_all(parent).map_err(|e| StoreFailure::CreateDir {
            parent: parent.display().to_string(),
            reason: e.to_string(),
        })?;
    }
    let mut temp = tempfile::NamedTempFile::new_in(parent).map_err(|e| StoreFailure::WriteTemp {
        temp: path.display().to_string(),
        reason: e.to_string(),
    })?;
    let write = (|| -> std::io::Result<()> {
        restrict_permissions(temp.path());
        std::io::Write::write_all(&mut temp, text.as_bytes())?;
        std::io::Write::flush(&mut temp)?;
        // 落盘后再替换：崩溃不会留下"替换成功但内容未持久化"的空壳。
        temp.as_file().sync_all()?;
        Ok(())
    })();
    if let Err(e) = write {
        return Err(StoreFailure::WriteTemp {
            temp: temp.path().display().to_string(),
            reason: e.to_string(),
        });
    }
    temp.persist(path)
        .map(|_| ())
        .map_err(|e| StoreFailure::Rename {
            path: path.display().to_string(),
            reason: e.error.to_string(),
        })
}

/// 尽力收紧权限（Unix 0600；其它平台由密文本身负责）。
fn restrict_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_replace_round_trips_and_leaves_no_temp_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("creds.json");
        atomic_write_json(&path, "{\"a\":1}").expect("first write creates parent");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "{\"a\":1}");
        atomic_write_json(&path, "{\"a\":2}").expect("replace existing");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "{\"a\":2}");
        assert!(
            !path.with_extension("json.tmp").exists(),
            "temp file must not linger"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).expect("meta").permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "凭据文件必须 0600，实际 {mode:o}");
        }
    }
}
