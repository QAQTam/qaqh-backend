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

/// 转正开关（CLEAN-3/T10）：`QAQH_SBX_REDIRECT=0` 退出 Auto→Redirect 晋升。
pub fn redirect_promoted() -> bool {
    !matches!(
        std::env::var("QAQH_SBX_REDIRECT").as_deref(),
        Ok("0") | Ok("false")
    )
}

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
///
/// 转正（CLEAN-3/T10）：`Auto` 在 Windows 上不再 fail-open——工作区 +
/// ProjFS 可用时优先选 RedirectPlane（最强写隔离，turn 结束同步 merge），
/// 否则 `Ok(None)` 交回调用方兜底（`wrap_command` 落 TokenPlane）。
/// `QAQH_SBX_REDIRECT=0` 可整体退出晋升（灰度/排障开关）。
pub fn resolve_windows_backend(spec: &SandboxSpec) -> Result<Option<SandboxBackend>, String> {
    match spec.backend {
        SandboxBackend::WindowsToken => Ok(Some(SandboxBackend::WindowsToken)),
        SandboxBackend::Auto if redirect_promoted() => {
            let Some(ws) = &spec.workspace_root else {
                return Ok(None);
            };
            if !ws.is_dir() || !sbx_win::projfs::available() {
                return Ok(None);
            }
            Ok(Some(SandboxBackend::WindowsRedirect))
        }
        SandboxBackend::Auto => Ok(None),
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

    /// 转正（CLEAN-3/T10）：Auto + workspace + ProjFS 可用 → 晋升
    /// WindowsRedirect；ProjFS 不可用回落 None（wrap_command 落 TokenPlane）。
    /// 断言按宿主 ProjFS 实况分支，两台机器上都诚实。
    #[test]
    fn auto_promotes_to_redirect_when_projfs_available() {
        static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let spec = SandboxSpec::workspace_write(PathBuf::from(env!("CARGO_MANIFEST_DIR")));
        let resolved = resolve_windows_backend(&spec).expect("promotion must not fail closed");
        if sbx_win::projfs::available() {
            assert_eq!(resolved, Some(SandboxBackend::WindowsRedirect));
        } else {
            assert_eq!(resolved, None);
        }
    }

    /// 退出开关：`QAQH_SBX_REDIRECT=0` 时 Auto 不晋升（灰度/排障用）。
    #[test]
    fn auto_promotion_can_be_opted_out() {
        static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        unsafe { std::env::set_var("QAQH_SBX_REDIRECT", "0") };
        let spec = SandboxSpec::workspace_write(PathBuf::from(env!("CARGO_MANIFEST_DIR")));
        assert_eq!(resolve_windows_backend(&spec).expect("auto"), None);
        unsafe { std::env::remove_var("QAQH_SBX_REDIRECT") };
    }
}
