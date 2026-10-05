//! DACL 操作:capability-SID allow/deny ACE 的追加与探测。
//!
//! 免提权依据(owner 隐式 WRITE_DAC):本进程对用户自己的文件/目录
//! 持有隐式 WRITE_DAC,追加 ACE 无需特权(见 docs/adr/0001 的 spike 验证)。

use crate::events::EventSink;
use crate::policy::{AceMode, AceOp, AceTarget, Precreate};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use windows::core::{Error, PCWSTR, PWSTR, Result};
use windows::Win32::Foundation::{LocalFree, ERROR_SUCCESS, HLOCAL};
use windows::Win32::Security::Authorization::{
    BuildTrusteeWithSidW, ConvertSecurityDescriptorToStringSecurityDescriptorW,
    ConvertStringSecurityDescriptorToSecurityDescriptorW, DENY_ACCESS, EXPLICIT_ACCESS_W,
    GetNamedSecurityInfoW, GRANT_ACCESS, SE_FILE_OBJECT, SE_OBJECT_TYPE, SE_REGISTRY_KEY,
    SetEntriesInAclW, SetNamedSecurityInfoW,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegOpenKeyExW, HKEY, HKEY_CLASSES_ROOT, HKEY_CURRENT_USER,
    HKEY_LOCAL_MACHINE, HKEY_USERS, KEY_ALL_ACCESS, KEY_READ, KEY_WRITE, REG_OPTION_NON_VOLATILE,
};
use windows::Win32::Security::{
    ACL, ACL_SIZE_INFORMATION, AclSizeInformation, DACL_SECURITY_INFORMATION, GetAce,
    GetAclInformation, MakeAbsoluteSD, NO_INHERITANCE, PROTECTED_DACL_SECURITY_INFORMATION, PSID,
    PSECURITY_DESCRIPTOR, SUB_CONTAINERS_AND_OBJECTS_INHERIT,
};
use windows::Win32::Storage::FileSystem::FILE_GENERIC_WRITE;

/// FILE_ATTRIBUTE_REPARSE_POINT(0x400):符号链接 / junction / 一次性占位等。
pub const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

/// SDDL_REVISION_1
const SDDL_REVISION_1: u32 = 1;

/// 写授权掩码。FILE_GENERIC_WRITE 在目录对象上经通用映射覆盖
/// FILE_ADD_FILE / FILE_ADD_SUBDIRECTORY(可在目录下新建文件)。
pub const WRITE_MASK: u32 = FILE_GENERIC_WRITE.0;

/// 读授权掩码(appcontainer 后端 readable_roots 用):读 + 列目录 + 执行。
pub const READ_MASK: u32 = windows::Win32::Storage::FileSystem::FILE_GENERIC_READ.0
    | windows::Win32::Storage::FileSystem::FILE_EXECUTE.0;

/// FULL 级掩码(appcontainer 后端写授权用:26300 内核对窄掩码的既有文件
/// 打开仍会拒绝,实证见 ADR-0002;沙箱进程能力受限,可接受)
pub const FULL_MASK: u32 = 0x001F01FF; // FILE_ALL_ACCESS

/// 注册表写掩码(KEY_WRITE = KEY_SET_VALUE | KEY_CREATE_SUB_KEY | READ_CONTROL)
pub const REG_WRITE_MASK: u32 = KEY_WRITE.0;

/// 注册表读掩码(KEY_READ)
pub const REG_READ_MASK: u32 = KEY_READ.0;

/// 注册表 FULL 级掩码(KEY_ALL_ACCESS;与文件 FULL 同理,ADR-0002 事实 2)
pub const REG_FULL_MASK: u32 = KEY_ALL_ACCESS.0;

const ACE_TYPE_ALLOWED: u8 = 0; // ACCESS_ALLOWED_ACE_TYPE
const ACE_TYPE_DENIED: u8 = 1; // ACCESS_DENIED_ACE_TYPE

fn wide(p: &Path) -> Vec<u16> {
    let mut v: Vec<u16> = p.as_os_str().encode_wide().collect();
    v.push(0);
    v
}

fn win32_err(code: windows::Win32::Foundation::WIN32_ERROR) -> Error {
    Error::from(windows::core::HRESULT::from_win32(code.0))
}

/// 查询现有 DACL。dacl 指向 sd 内部分配,sd 被 Drop 语义释放(此处手动 LocalFree)。
fn get_dacl(path: &Path) -> Result<(Option<*mut ACL>, PSECURITY_DESCRIPTOR)> {
    get_security(AceTarget::File, path)
}

/// 注册表键路径解析:"HKCU\Software\x"(或全称)→ (根键, 子键, 全称形态)。
/// 全称形态供 Get/SetNamedSecurityInfoW(SE_REGISTRY_KEY) 使用;工程怪癖:
/// 这组 API 的注册表名**不带 HKEY_ 前缀**(CURRENT_USER\... / MACHINE\... /
/// CLASSES_ROOT\... / USERS\...),带前缀会报 E_INVALIDARG。未知根一律报错
/// (fail-closed,不猜测)。
fn registry_parts(path: &Path) -> io::Result<(HKEY, String, String)> {
    let text = path.as_os_str().to_string_lossy().replace('/', "\\");
    let (root_text, sub) = match text.split_once('\\') {
        Some((r, s)) => (r, s),
        None => (text.as_str(), ""),
    };
    let (hkey, full_root) = match root_text.to_ascii_uppercase().as_str() {
        "HKCU" | "HKEY_CURRENT_USER" => (HKEY_CURRENT_USER, "CURRENT_USER"),
        "HKLM" | "HKEY_LOCAL_MACHINE" => (HKEY_LOCAL_MACHINE, "LOCAL_MACHINE"),
        "HKCR" | "HKEY_CLASSES_ROOT" => (HKEY_CLASSES_ROOT, "CLASSES_ROOT"),
        "HKU" | "HKEY_USERS" => (HKEY_USERS, "USERS"),
        other => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unsupported registry root: {other:?}"),
            ));
        }
    };
    Ok((hkey, sub.to_string(), format!("{full_root}\\{sub}")))
}

/// AceTarget → SE_OBJECT_TYPE(文件/注册表统一 ACL 流水线的唯一映射点;
/// windows-rs 0.58 把注册表对象类型命名为 SE_REGISTRY_KEY,即 Win32 的
/// SE_REGISTRY_OBJECT=4)。
fn se_obj(target: AceTarget) -> SE_OBJECT_TYPE {
    match target {
        AceTarget::File => SE_FILE_OBJECT,
        AceTarget::Registry => SE_REGISTRY_KEY,
    }
}

/// Get/SetNamedSecurityInfoW 的对象名(文件 = 路径本身;注册表 = 全称键名)。
fn security_name_w(target: AceTarget, path: &Path) -> io::Result<Vec<u16>> {
    match target {
        AceTarget::Registry => {
            let text = registry_parts(path)?.2;
            Ok(text.encode_utf16().chain([0]).collect())
        }
        _ => Ok(wide(path)),
    }
}

/// 按对象种类查询 DACL(文件/注册表共用;ACE 二进制布局相同)。
fn get_security(
    target: AceTarget,
    path: &Path,
) -> Result<(Option<*mut ACL>, PSECURITY_DESCRIPTOR)> {
    let w = security_name_w(target, path)?;
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut sd = PSECURITY_DESCRIPTOR(std::ptr::null_mut());
    let code = unsafe {
        GetNamedSecurityInfoW(
            PCWSTR(w.as_ptr()),
            se_obj(target),
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(&mut dacl),
            None,
            &mut sd,
        )
    };
    if code != ERROR_SUCCESS {
        return Err(win32_err(code));
    }
    Ok((if dacl.is_null() { None } else { Some(dacl) }, sd))
}

/// DACL 中对 `sid` 的同向 ACE 计数(幂等性验证:重复 apply 不应堆积)。
/// 判定含掩码:该模式的 required_mask 必须被 ACE 掩码覆盖,
/// 否则同 SID 的 Read ACE 会被误判为 Allow 已存在(或反之)。
pub fn count_aces(path: &Path, sid: &[u8], mode: AceMode) -> Result<u32> {
    count_aces_obj(AceTarget::File, path, sid, mode)
}

/// [`count_aces`] 的对象种类泛化形态(注册表 DACL 的 ACE 布局与文件相同)。
fn count_aces_obj(target: AceTarget, path: &Path, sid: &[u8], mode: AceMode) -> Result<u32> {
    // 幂等判据的必需掩码按对象种类取——文件掩码族(FILE_GENERIC_WRITE)与
    // 注册表掩码族(KEY_*)不可混用,否则注册表 ACE 永远判不中
    let required = match target {
        AceTarget::File => mode.required_mask(),
        AceTarget::Registry => match mode {
            AceMode::Read => REG_READ_MASK,
            _ => REG_WRITE_MASK,
        },
    };
    let (dacl, sd) = get_security(target, path)?;
    let r = (|| {
        unsafe {
            let Some(dacl) = dacl else { return Ok(0u32) };
            let mut info = ACL_SIZE_INFORMATION::default();
            GetAclInformation(
                dacl,
                &mut info as *mut _ as *mut core::ffi::c_void,
                std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            )?;
            let want_type = match mode {
                AceMode::Allow | AceMode::Read => ACE_TYPE_ALLOWED,
                AceMode::Deny => ACE_TYPE_DENIED,
            };
            let mut count = 0u32;
            for i in 0..info.AceCount {
                let mut pace: *mut core::ffi::c_void = std::ptr::null_mut();
                GetAce(dacl, i, &mut pace)?;
                if pace.is_null() {
                    continue;
                }
                // ACE_HEADER(4B) + AccessMask(4B) 之后是 SID
                let bytes = pace as *const u8;
                if *bytes != want_type {
                    continue;
                }
                let mask = u32::from_le_bytes([bytes.add(4).read(), bytes.add(5).read(), bytes.add(6).read(), bytes.add(7).read()]);
                if mask & required != required {
                    continue;
                }
                let ace_sid = PSID(bytes.add(8) as *mut core::ffi::c_void);
                let target = PSID(sid.as_ptr() as *mut core::ffi::c_void);
                if equal_sid(ace_sid, target) {
                    count += 1;
                }
            }
            Ok(count)
        }
    })();
    unsafe {
        if !sd.0.is_null() {
            LocalFree(HLOCAL(sd.0));
        }
    }
    r
}

/// 撤销对 `sid` 的全部 ACE(REVOKE_ACCESS 重建 DACL)。cleanup 命令的底座。
pub fn revoke_ace(path: &Path, sid: &[u8]) -> Result<bool> {
    revoke_ace_for(path, AceTarget::File, sid)
}

/// [`revoke_ace`] 的对象种类泛化形态(cleanup 按台账 kind 分派)。
pub fn revoke_ace_for(path: &Path, target: AceTarget, sid: &[u8]) -> Result<bool> {
    // 存在性检查按对象种类分派(文件 Path::exists 对注册表路径恒 false)
    match target {
        AceTarget::File => {
            if !path.exists() {
                return Ok(false);
            }
        }
        AceTarget::Registry => {
            if !has_ace_for(path, target, sid, AceMode::Allow)?
                && !has_ace_for(path, target, sid, AceMode::Deny)?
            {
                // 键不存在时上一行已报错;两个计数皆 0 = 键在但无本 SID ACE
                return Ok(false);
            }
        }
    }
    let (old_dacl, sd) = get_security(target, path)?;
    let r = (|| {
        unsafe {
            let Some(old) = old_dacl else { return Ok(false) };
            let mut ea = EXPLICIT_ACCESS_W::default();
            BuildTrusteeWithSidW(&mut ea.Trustee, PSID(sid.as_ptr() as *mut core::ffi::c_void));
            ea.grfAccessMode = windows::Win32::Security::Authorization::REVOKE_ACCESS;
            ea.grfAccessPermissions = 0;
            let mut new_dacl: *mut ACL = std::ptr::null_mut();
            let code =
                SetEntriesInAclW(Some(std::slice::from_ref(&ea)), Some(old as *const ACL), &mut new_dacl);
            if code != ERROR_SUCCESS {
                return Err(win32_err(code));
            }
            let name_w = security_name_w(target, path)?;
            let code = SetNamedSecurityInfoW(
                PCWSTR(name_w.as_ptr()),
                se_obj(target),
                DACL_SECURITY_INFORMATION,
                PSID(std::ptr::null_mut()),
                PSID(std::ptr::null_mut()),
                Some(new_dacl),
                None,
            );
            let r = if code != ERROR_SUCCESS {
                Err(win32_err(code))
            } else {
                Ok(true)
            };
            if !new_dacl.is_null() {
                LocalFree(HLOCAL(new_dacl as *mut core::ffi::c_void));
            }
            r
        }
    })();
    unsafe {
        if !sd.0.is_null() {
            LocalFree(HLOCAL(sd.0));
        }
    }
    r
}

/// DACL 中是否已存在对 `sid` 的同向 ACE(幂等追加的判据,防每次运行堆积重复 ACE)。
pub fn has_ace(path: &Path, sid: &[u8], mode: AceMode) -> Result<bool> {
    has_ace_for(path, AceTarget::File, sid, mode)
}

/// [`has_ace`] 的对象种类泛化形态(cleanup 按台账 kind 分派)。
pub fn has_ace_for(path: &Path, target: AceTarget, sid: &[u8], mode: AceMode) -> Result<bool> {
    Ok(count_aces_obj(target, path, sid, mode)? > 0)
}

/// EqualSid 的 windows-rs 形态:返回 Result<(), _>,Ok 即相等。
fn equal_sid(a: PSID, b: PSID) -> bool {
    unsafe { windows::Win32::Security::EqualSid(a, b).is_ok() }
}

/// 追加一条 ACE(GRANT/DENY,capability SID)。幂等:已有同 SID 同向 ACE 则跳过。
pub fn add_ace(path: &Path, sid: &[u8], mode: AceMode, inherit_tree: bool) -> Result<bool> {
    add_ace_with_mask(path, sid, mode, inherit_tree, mode.required_mask())
}

/// [`add_ace`] 的显式掩码形态(appcontainer 后端需要 FULL 级掩码)。
pub fn add_ace_with_mask(
    path: &Path,
    sid: &[u8],
    mode: AceMode,
    inherit_tree: bool,
    mask: u32,
) -> Result<bool> {
    add_ace_with_mask_ex(path, sid, mode, inherit_tree, mask, false)
}

/// [`add_ace_with_mask`] 的 protected 形态:置 PROTECTED_DACL 标志重建 DACL
/// (**清掉全部继承来的 ACE**)。M2 视图根用——scratch 的 cap-allow ACE
/// (OI CI)会经继承渗进视图,等于给整个视图发写通行证;protected 重建
/// 后视图 DACL 仅含本次追加的 ACE(向下继承仍生效),未授权路径回归
/// "无 ACE = 拒绝"。
pub fn add_ace_protected(
    path: &Path,
    sid: &[u8],
    mode: AceMode,
    inherit_tree: bool,
    mask: u32,
) -> Result<bool> {
    add_ace_with_mask_ex(path, sid, mode, inherit_tree, mask, true)
}

/// [`add_ace_protected`] 的底层(protected 标志显式化;文件对象专用)。
pub fn add_ace_with_mask_ex(
    path: &Path,
    sid: &[u8],
    mode: AceMode,
    inherit_tree: bool,
    mask: u32,
    protected: bool,
) -> Result<bool> {
    add_ace_obj(AceTarget::File, path, sid, mode, inherit_tree, mask, protected)
}

/// 对象种类无关的 ACE 追加核心(文件/注册表共用;SetEntriesInAclW 的
/// trustee/继承位语义在 SE_REGISTRY_OBJECT 上完全同构)。
/// `protected` = 以 PROTECTED_DACL 重建(丢弃继承 ACE,见
/// [`add_ace_protected`])。protected 形态仅文件对象使用(注册表无此需求)。
fn add_ace_obj(
    target: AceTarget,
    path: &Path,
    sid: &[u8],
    mode: AceMode,
    inherit_tree: bool,
    mask: u32,
    protected: bool,
) -> Result<bool> {
    if !protected && has_ace_for(path, target, sid, mode)? {
        return Ok(false);
    }
    let (old_dacl, sd) = get_security(target, path)?;
    let r = (|| {
        unsafe {
            let mut ea = EXPLICIT_ACCESS_W::default();
            BuildTrusteeWithSidW(&mut ea.Trustee, PSID(sid.as_ptr() as *mut core::ffi::c_void));
            ea.grfAccessPermissions = mask;
            ea.grfAccessMode = match mode {
                AceMode::Allow | AceMode::Read => GRANT_ACCESS,
                AceMode::Deny => DENY_ACCESS,
            };
            ea.grfInheritance = if inherit_tree {
                SUB_CONTAINERS_AND_OBJECTS_INHERIT
            } else {
                NO_INHERITANCE
            };
            let mut new_dacl: *mut ACL = std::ptr::null_mut();
            let code =
                SetEntriesInAclW(Some(std::slice::from_ref(&ea)), old_dacl.map(|p| p as *const ACL), &mut new_dacl);
            if code != ERROR_SUCCESS {
                return Err(win32_err(code));
            }
            let name_w = security_name_w(target, path)?;
            let si = if protected {
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION
            } else {
                DACL_SECURITY_INFORMATION
            };
            let code = SetNamedSecurityInfoW(
                PCWSTR(name_w.as_ptr()),
                se_obj(target),
                si,
                PSID(std::ptr::null_mut()),
                PSID(std::ptr::null_mut()),
                Some(new_dacl),
                None,
            );
            let r = if code != ERROR_SUCCESS {
                Err(win32_err(code))
            } else {
                Ok(())
            };
            if !new_dacl.is_null() {
                LocalFree(HLOCAL(new_dacl as *mut core::ffi::c_void));
            }
            r
        }
    })();
    unsafe {
        if !sd.0.is_null() {
            LocalFree(HLOCAL(sd.0));
        }
    }
    r?;
    Ok(true)
}

/// 组件级 reparse-point 扫描:路径任意一环(含目标本身)是符号链接 / junction
/// / 占位符即拒绝。
///
/// 威胁模型:apply 由**完整令牌**的父进程执行且按路径解析,若穿透 reparse
/// point,ACE 会落到攻击者指向的最终对象上(如 workspace 里埋一个指向
/// ~/.ssh/authorized_keys 的"config.toml")。内核 DACL 只查最终对象、不查
/// 路径形态,因此这层防线必须在父进程侧先于 add_ace 完成。
/// 注意:symlink_metadata 只对"最后一环"不穿透,所以必须逐前缀扫描——
/// 中间环节(如 `E:\ws\link\file` 的 `link`)被穿透时,对完整路径的探测
/// 看到的是 junction 目标内的文件,形同无害。
pub fn ensure_no_reparse_components(path: &Path) -> io::Result<()> {
    let mut acc = PathBuf::new();
    let mut past_root = false;
    for comp in path.components() {
        match comp {
            Component::Prefix(_) | Component::RootDir => acc.push(comp.as_os_str()),
            Component::CurDir | Component::ParentDir | Component::Normal(_) => {
                acc.push(comp.as_os_str());
                if past_root {
                    if let Ok(md) = std::fs::symlink_metadata(&acc) {
                        if md.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidInput,
                                format!(
                                    "ace target path traverses a reparse point: {}",
                                    acc.display()
                                ),
                            ));
                        }
                    }
                }
            }
        }
        past_root = true;
    }
    Ok(())
}

/// 读当前 DACL 的 SDDL 字符串(apply 前快照;cleanup 按此恢复)。
/// 无 DACL(或路径不存在)→ Ok(None)。
pub fn dacl_sddl(path: &Path) -> Result<Option<String>> {
    sddl_for(AceTarget::File, path)
}

/// [`dacl_sddl`] 的对象种类泛化形态。
pub fn sddl_for(target: AceTarget, path: &Path) -> Result<Option<String>> {
    let (dacl, sd) = get_security(target, path)?;
    let r = (|| unsafe {
        let Some(_) = dacl else { return Ok(None) };
        let mut pw = PWSTR::null();
        let mut len = 0u32;
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            sd,
            SDDL_REVISION_1,
            DACL_SECURITY_INFORMATION,
            &mut pw,
            Some(&mut len),
        )?;
        if pw.is_null() {
            return Ok(None);
        }
        let mut end = 0usize;
        while *pw.0.add(end) != 0 {
            end += 1;
        }
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(pw.0, end));
        let _ = LocalFree(HLOCAL(pw.0 as *mut core::ffi::c_void));
        Ok(Some(s))
    })();
    unsafe {
        if !sd.0.is_null() {
            LocalFree(HLOCAL(sd.0));
        }
    }
    r
}

/// 按 SDDL 快照恢复 DACL(cleanup 用;快照 = apply 之前用户文件的原始形态)。
pub fn restore_dacl_from_sddl(path: &Path, sddl: &str) -> Result<()> {
    restore_sddl_for(AceTarget::File, path, sddl)
}

/// [`restore_dacl_from_sddl`] 的对象种类泛化形态。
pub fn restore_sddl_for(target: AceTarget, path: &Path, sddl: &str) -> Result<()> {
    unsafe {
        let s: Vec<u16> = sddl.encode_utf16().chain([0]).collect();
        let mut psd = PSECURITY_DESCRIPTOR::default();
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(s.as_ptr()),
            SDDL_REVISION_1,
            &mut psd,
            None,
        )?;
        let r = (|| {
            // ConvertStringSD 产出 self-relative SD;SetNamedSecurityInfoW 需要
            // 绝对形态的 DACL 指针 → MakeAbsoluteSD 两遍式(先问长度再取)。
            let mut abs_len = 0u32;
            let mut dacl_len = 0u32;
            let mut sacl_len = 0u32;
            let mut own_len = 0u32;
            let mut grp_len = 0u32;
            let _ = MakeAbsoluteSD(
                psd,
                PSECURITY_DESCRIPTOR::default(),
                &mut abs_len,
                None,
                &mut dacl_len,
                None,
                &mut sacl_len,
                PSID::default(),
                &mut own_len,
                PSID::default(),
                &mut grp_len,
            );
            if abs_len == 0 || dacl_len == 0 {
                return Err(Error::from(windows::core::HRESULT::from_win32(
                    windows::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER.0,
                )));
            }
            let mut abs_buf = vec![0u8; abs_len as usize];
            let mut dacl_buf = vec![0u8; dacl_len as usize];
            let mut sacl_buf = vec![0u8; sacl_len as usize];
            let mut own_buf = vec![0u8; own_len as usize];
            let mut grp_buf = vec![0u8; grp_len as usize];
            MakeAbsoluteSD(
                psd,
                PSECURITY_DESCRIPTOR(abs_buf.as_mut_ptr() as *mut core::ffi::c_void),
                &mut abs_len,
                Some(dacl_buf.as_mut_ptr() as *mut ACL),
                &mut dacl_len,
                Some(sacl_buf.as_mut_ptr() as *mut ACL),
                &mut sacl_len,
                PSID(own_buf.as_mut_ptr() as *mut core::ffi::c_void),
                &mut own_len,
                PSID(grp_buf.as_mut_ptr() as *mut core::ffi::c_void),
                &mut grp_len,
            )?;
            let name_w = security_name_w(target, path)?;
            let code = SetNamedSecurityInfoW(
                PCWSTR(name_w.as_ptr()),
                se_obj(target),
                DACL_SECURITY_INFORMATION,
                PSID::default(),
                PSID::default(),
                Some(dacl_buf.as_mut_ptr() as *mut ACL),
                None,
            );
            if code != ERROR_SUCCESS {
                return Err(win32_err(code));
            }
            Ok(())
        })();
        LocalFree(HLOCAL(psd.0));
        r
    }
}

/// 写类访问位(文件/目录共享语义):FILE_WRITE_DATA | FILE_APPEND_DATA |
/// FILE_WRITE_EA | FILE_WRITE_ATTRIBUTES | DELETE | WRITE_DAC
const WRITE_INTEREST_MASK: u32 = 0x0002 | 0x0004 | 0x0010 | 0x0100 | 0x0001_0000 | 0x0004_0000;

/// DACL 中是否存在 Everyone / Authenticated Users / BUILTIN\Users 的写类 allow ACE。
///
/// TokenPlane 的 Everyone 兜底 restricting SID 会让这类路径对沙箱**隐式可写**
/// (已知边界,ADR-0002);doctor 用它定位"事实上的沙箱写出口"。
pub fn wellknown_write_grants(path: &Path) -> Result<Vec<&'static str>> {
    const SUBJECTS: &[(&str, &str)] = &[
        ("Everyone", "S-1-1-0"),
        ("AuthenticatedUsers", "S-1-5-11"),
        ("BUILTIN\\Users", "S-1-5-32-545"),
    ];
    let subjects: Vec<(&'static str, Vec<u8>)> = SUBJECTS
        .iter()
        .filter_map(|(label, text)| crate::sid::parse_sid(text).ok().map(|s| (*label, s)))
        .collect();
    let (dacl, sd) = get_dacl(path)?;
    let r = (|| unsafe {
        let Some(dacl) = dacl else {
            return Ok(Vec::new());
        };
        let mut info = ACL_SIZE_INFORMATION::default();
        GetAclInformation(
            dacl,
            &mut info as *mut _ as *mut core::ffi::c_void,
            std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )?;
        let mut found: Vec<&'static str> = Vec::new();
        for i in 0..info.AceCount {
            let mut pace: *mut core::ffi::c_void = std::ptr::null_mut();
            if GetAce(dacl, i, &mut pace).is_err() || pace.is_null() {
                continue;
            }
            let bytes = pace as *const u8;
            if *bytes != ACE_TYPE_ALLOWED {
                continue;
            }
            let mask = u32::from_le_bytes([
                bytes.add(4).read(),
                bytes.add(5).read(),
                bytes.add(6).read(),
                bytes.add(7).read(),
            ]);
            if mask & WRITE_INTEREST_MASK == 0 {
                continue;
            }
            let ace_sid = PSID(bytes.add(8) as *mut core::ffi::c_void);
            for (label, sid) in &subjects {
                let target = PSID(sid.as_ptr() as *mut core::ffi::c_void);
                if equal_sid(ace_sid, target) && !found.contains(label) {
                    found.push(label);
                }
            }
        }
        Ok(found)
    })();
    unsafe {
        if !sd.0.is_null() {
            LocalFree(HLOCAL(sd.0));
        }
    }
    r
}

struct ScanState {
    findings: Vec<PathBuf>,
    visited: usize,
    max_entries: usize,
}

/// 遍历 `root` 子树(含 root 本身),报告 Everyone/AuthUsers/Users 持写类
/// allow ACE 的路径。诊断用:depth 与 entry 总数双上限,不穿 reparse point
/// (与 ACE apply 的组件扫描同款防线),无权限的分支静默跳过。
pub fn scan_everyone_writable(
    root: &Path,
    max_depth: u32,
    max_entries: usize,
) -> io::Result<Vec<PathBuf>> {
    let mut st = ScanState {
        findings: Vec::new(),
        visited: 0,
        max_entries,
    };
    if let Ok(md) = std::fs::symlink_metadata(root) {
        if md.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0
            && !wellknown_write_grants(root).unwrap_or_default().is_empty()
        {
            st.findings.push(root.to_path_buf());
        }
        if md.is_dir() {
            scan_walk(root, 0, max_depth, &mut st)?;
        }
    }
    Ok(st.findings)
}

fn scan_walk(dir: &Path, depth: u32, max_depth: u32, st: &mut ScanState) -> io::Result<()> {
    if depth > max_depth || st.visited >= st.max_entries {
        return Ok(());
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        if st.visited >= st.max_entries {
            return Ok(());
        }
        st.visited += 1;
        let path = entry.path();
        let Ok(md) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if md.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            continue;
        }
        if !wellknown_write_grants(&path).unwrap_or_default().is_empty() {
            st.findings.push(path.clone());
        }
        if md.is_dir() {
            scan_walk(&path, depth + 1, max_depth, st)?;
        }
    }
    Ok(())
}

/// 执行 spawn 前计划:reparse 扫描 + 预创建 + 快照 + 追加 ACE。事件逐条发 sink。
/// 返回与 ops 对齐的 SDDL 快照(None = 无可快照 DACL),供台账记录。
pub fn apply_ops(ops: &[AceOp], sink: &dyn EventSink) -> io::Result<Vec<Option<String>>> {
    let mut snapshots: Vec<Option<String>> = Vec::with_capacity(ops.len());
    for op in ops {
        // 注册表路径不是文件系统路径,reparse 扫描只对 File 目标有意义
        if op.kind == AceTarget::File {
            ensure_no_reparse_components(&op.path)?;
        }
        match op.kind {
            AceTarget::File => {
                if op.precreate != Precreate::None && !op.path.exists() {
                    match op.precreate {
                        Precreate::File => {
                            let _ = std::fs::File::create(&op.path)?;
                        }
                        Precreate::Dir => std::fs::create_dir_all(&op.path)?,
                        Precreate::None => {}
                    };
                }
                if !op.path.exists() {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("ace target missing: {}", op.path.display()),
                    ));
                }
            }
            AceTarget::Registry => {
                ensure_registry_key(&op.path, op.precreate)?;
            }
        }
        // 快照必须在 add_ace 之前(apply 之后的 DACL 已含 cap ACE,不可作恢复基准)
        let sddl = sddl_for(op.kind, &op.path).ok().flatten();
        // 掩码选择:文件窄掩码/FULL(AC);注册表恒 KEY_ALL_ACCESS —— reg.exe 等
        // 常见工具对授权键的打开要求超出 KEY_WRITE,窄掩码易使授权实际不可用;
        // 且注册表授权键由调用方逐一点名,区域即授权边界(ADR-0002 事实 2 同理由)
        let mask = match op.kind {
            AceTarget::File => {
                if op.full_mask {
                    FULL_MASK
                } else {
                    op.mode.required_mask()
                }
            }
            AceTarget::Registry => REG_FULL_MASK,
        };
        let applied = add_ace_obj(op.kind, &op.path, &op.sid, op.mode, op.inherit_tree, mask, false)
            .map_err(|e| io::Error::other(format!("add_ace {}: {e}", op.path.display())))?;
        let mode = match op.mode {
            AceMode::Allow => "allow",
            AceMode::Deny => "deny",
            AceMode::Read => "read",
        };
        sink.emit(
            &crate::events::Event::new("acl_apply")
                .with_path(op.path.display().to_string())
                .with_detail(serde_json::json!({ "mode": mode, "applied": applied })),
        );
        snapshots.push(sddl);
    }
    Ok(snapshots)
}

/// 注册表键存在性 + 预创建(Precreate::Dir/File = 不存在则创建)。
fn ensure_registry_key(path: &Path, precreate: Precreate) -> io::Result<()> {
    let (hroot, sub, _) = registry_parts(path)?;
    let sub_w: Vec<u16> = sub.encode_utf16().chain([0]).collect();
    unsafe {
        let mut hkey = HKEY::default();
        let code =
            RegOpenKeyExW(hroot, PCWSTR(sub_w.as_ptr()), 0, KEY_READ, &mut hkey);
        if code == ERROR_SUCCESS {
            let _ = RegCloseKey(hkey);
            return Ok(());
        }
        if precreate == Precreate::None {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("registry key missing: {}", path.display()),
            ));
        }
        let code = RegCreateKeyExW(
            hroot,
            PCWSTR(sub_w.as_ptr()),
            0,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_READ | KEY_WRITE,
            None,
            &mut hkey,
            None,
        );
        if code != ERROR_SUCCESS {
            return Err(io::Error::other(format!(
                "registry precreate {}: win32 error {}",
                path.display(),
                code.0
            )));
        }
        let _ = RegCloseKey(hkey);
        Ok(())
    }
}
