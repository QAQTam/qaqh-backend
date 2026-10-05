//! Windows sbx 后端的适配面(方向恒为 "qaqh 适配 sbx",win-sandbox-rs 宪法第 2 条)。
//!
//! - [`map_policy`]:[`SandboxSpec`] → `sbx_win::policy::SbxPolicy` 纯映射
//!   (SbxPolicy 的 JSON 是冻结契约;本模块只搬字段,不做文件系统副作用)。
//! - [`resolve_windows_backend`]:显式请求的 Windows 后端可行性校验
//!   (redirect 需要 workspace + ProjFS 可选功能)。
//!
//! 已知如实丢弃/降级(均记录在 capability detail 与结构化日志):
//! - `max_open_files`:Job 对象无对应限额,丢弃;
//! - TokenPlane 的 `network: Deny` 零强制(token 后端只滤写,读/网透传)。

use qaqh_policy::{NetworkPolicy, SandboxBackend, SandboxSpec};
use sbx_win::policy::{IsolationKind, NetworkPolicy as SbxNetworkPolicy, SbxPolicy};

/// 纯映射:SandboxSpec → SbxPolicy。隔离后端恒 Token(AC 后端留实验档,
/// 不接生产;调研报告 §3)。
pub fn map_policy(spec: &SandboxSpec) -> SbxPolicy {
    SbxPolicy {
        writable_roots: spec.writable_roots.clone(),
        writable_files: spec.writable_files.clone(),
        deny_write_paths: spec.deny_write_paths.clone(),
        network: match spec.network {
            NetworkPolicy::Deny => SbxNetworkPolicy::Deny,
            NetworkPolicy::Allow => SbxNetworkPolicy::Allow,
        },
        isolation: IsolationKind::Token,
        readable_roots: Vec::new(),
        capabilities: Vec::new(),
        writable_registry_keys: Vec::new(),
        redirect: matches!(spec.backend, SandboxBackend::WindowsRedirect) || spec.redirect,
    }
}

/// 显式 Windows 后端的可行性校验。`Ok(None)` = 非Windows 后端请求(交还
/// 原有解析路径);`Err` = 显式请求但环境不满足(fail closed)。
pub fn resolve_windows_backend(spec: &SandboxSpec) -> Result<Option<SandboxBackend>, String> {
    match spec.backend {
        SandboxBackend::WindowsToken => Ok(Some(SandboxBackend::WindowsToken)),
        SandboxBackend::WindowsRedirect => {
            let Some(ws) = &spec.workspace_root else {
                return Err(
                    "WindowsRedirect requires workspace_root (redirect store = workspace root)"
                        .into(),
                );
            };
            if !ws.is_dir() {
                return Err(format!(
                    "WindowsRedirect workspace_root does not exist: {}",
                    ws.display()
                ));
            }
            if !sbx_win::projfs::available() {
                return Err(
                    "WindowsRedirect requires ProjFS. Enable once (admin): \
                     DISM /Online /Enable-Feature /FeatureName:Client-ProjFS /NoRestart"
                        .into(),
                );
            }
            Ok(Some(SandboxBackend::WindowsRedirect))
        }
        _ => Ok(None),
    }
}

/// 映射 + 序列化为 SbxPolicy JSON(冻结契约形态)。Windows 旁路的取用面;
/// 恒返回 `Some`(模块本身 cfg(windows)),Option 形态是为调用方的
/// 平台无关代码好写。
pub fn policy_json_for_exec(spec: &SandboxSpec) -> Option<String> {
    let policy = map_policy(spec);
    serde_json::to_string(&policy)
        .map_err(|error| {
            log::error!(target: "qaqh_sandbox", "serialize sbx policy: {error}");
            error
        })
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn workspace_write_maps_roots_and_defaults() {
        let spec = SandboxSpec::workspace_write(PathBuf::from("E:/ws"));
        let policy = map_policy(&spec);
        assert_eq!(policy.writable_roots, vec![PathBuf::from("E:/ws")]);
        assert_eq!(policy.isolation, IsolationKind::Token);
        assert!(!policy.redirect);
        assert!(policy.capabilities.is_empty());
    }

    #[test]
    fn explicit_windows_backends_map_fields() {
        let mut spec = SandboxSpec::workspace_write(PathBuf::from("E:/ws"));
        spec.backend = SandboxBackend::WindowsRedirect;
        spec.redirect = true;
        spec.writable_files = vec![PathBuf::from("E:/ws/config.toml")];
        spec.deny_write_paths = vec![PathBuf::from("E:/ws/.git")];
        let policy = map_policy(&spec);
        assert!(policy.redirect);
        assert_eq!(policy.writable_files, spec.writable_files);
        assert_eq!(policy.deny_write_paths, spec.deny_write_paths);
    }

    #[test]
    fn redirect_without_workspace_fails_closed() {
        let mut spec = SandboxSpec::workspace_write(PathBuf::from("E:/ws"));
        spec.workspace_root = None;
        spec.backend = SandboxBackend::WindowsRedirect;
        assert!(resolve_windows_backend(&spec).is_err());
        spec.backend = SandboxBackend::WindowsToken;
        assert!(resolve_windows_backend(&spec).is_ok());
        spec.backend = SandboxBackend::Auto;
        assert_eq!(resolve_windows_backend(&spec).expect("auto"), None);
    }
}
