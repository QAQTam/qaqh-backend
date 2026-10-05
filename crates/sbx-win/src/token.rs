//! 受限令牌构造(Codex 同款:CreateRestrictedToken + WRITE_RESTRICTED)。
//!
//! 语义:restricting SID 只参与写访问检查 → 未挂 capability ACE 的路径
//! 写失败(内核 DACL),读走原始用户权限(全盘可读,spec N2)。

use windows::core::{PCWSTR, Result, w};
use windows::Win32::Foundation::{CloseHandle, HANDLE, LUID};
use windows::Win32::Security::{
    AdjustTokenPrivileges, CreateRestrictedToken, DISABLE_MAX_PRIVILEGE,
    GetSidSubAuthorityCount, GetTokenInformation, LUA_TOKEN, LUID_AND_ATTRIBUTES, PSID,
    LookupPrivilegeValueW, SE_PRIVILEGE_ENABLED, SID_AND_ATTRIBUTES,
    TOKEN_ADJUST_DEFAULT, TOKEN_ADJUST_SESSIONID, TOKEN_APPCONTAINER_INFORMATION,
    TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE, TOKEN_ELEVATION, TOKEN_GROUPS, TOKEN_IMPERSONATE,
    TOKEN_PRIVILEGES, TOKEN_QUERY, TokenAppContainerSid, TokenCapabilities, TokenElevation,
    TokenGroups, TokenUser, TOKEN_USER, WRITE_RESTRICTED,
};

/// SE_GROUP_ENABLED(0x4,Win32_System_SystemServices):能力 SID 参与
/// 访问检查所需属性位。本地定义避免为一个常量引入 cargo feature。
const SE_GROUP_ENABLED: u32 = 0x0000_0004;
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtCreateLowBoxToken(
        token_handle: *mut HANDLE,
        existing_token_handle: HANDLE,
        desired_access: u32,
        object_attributes: *const core::ffi::c_void,
        package_sid: *const core::ffi::c_void,
        capability_count: u32,
        capabilities: *const core::ffi::c_void,
        handle_count: u32,
        handles: *const core::ffi::c_void,
    ) -> windows::core::HRESULT;
}

/// 持有的令牌句柄,Drop 时关闭。
pub struct Token(HANDLE);

unsafe impl Send for Token {}
unsafe impl Sync for Token {}

impl Token {
    pub fn handle(&self) -> HANDLE {
        self.0
    }
}

impl Drop for Token {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// 从当前进程令牌派生受限主令牌。
///
/// 免 `SeAssignPrimaryTokenPrivilege` 的关键(V2,spike 实证):内核只对
/// "调用者主令牌的**直接**子令牌" 免赋权检查——中间不能再插一级
/// DuplicateTokenEx,必须 CreateRestrictedToken 直接作用于进程主令牌,
/// 否则 CreateProcessAsUserW 报 0x80070522(ERROR_PRIVILEGE_NOT_HELD)。
///
/// restricting SIDs 顺序对齐 codex(token.rs):capability SIDs → Everyone。
/// 特权裁剪用 DISABLE_MAX_PRIVILEGE,仅回补 SeChangeNotifyPrivilege
/// (目录遍历所需;codex token.rs 尾部同款)。
pub fn create_restricted_token(restricting_sids: &[Vec<u8>]) -> Result<Token> {
    unsafe {
        let mut h_base = HANDLE::default();
        // 输出句柄的权限位继承源句柄;CreateProcessAsUserW 要求
        // TOKEN_QUERY|TOKEN_DUPLICATE|TOKEN_ASSIGN_PRIMARY,源必须一次开足
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ASSIGN_PRIMARY
                | TOKEN_DUPLICATE
                | TOKEN_QUERY
                | TOKEN_ADJUST_DEFAULT
                | TOKEN_ADJUST_SESSIONID,
            &mut h_base,
        )?;
        let _base = HandleGuard(h_base);

        let entries: Vec<SID_AND_ATTRIBUTES> = restricting_sids
            .iter()
            .map(|bytes| SID_AND_ATTRIBUTES {
                Sid: windows::Win32::Security::PSID(bytes.as_ptr() as *mut core::ffi::c_void),
                Attributes: 0,
            })
            .collect();

        let mut h_low = HANDLE::default();
        CreateRestrictedToken(
            h_base,
            DISABLE_MAX_PRIVILEGE | LUA_TOKEN | WRITE_RESTRICTED,
            None,
            None,
            Some(&entries),
            &mut h_low,
        )?;

        enable_change_notify(h_low)?;
        Ok(Token(h_low))
    }
}

/// AppContainer lowbox 令牌(ADR-0002 第二隔离后端)。
///
/// NtCreateLowBoxToken 直接作用于进程主令牌——与 [`create_restricted_token`]
/// 同款"直接子令牌"豁免,免 SeAssignPrimaryTokenPrivilege。capability 数为 0
/// → 无 internetClient 等能力(内核断网);lowbox 令牌的**全部**访问检查都
/// 追加 package-SID 检查 → 读也可被 DACL 隔离(与 WRITE_RESTRICTED 的关键差异)。
///
/// 工程事实(2026-10-04,build 26300):CreateProcessW 的 SECURITY_CAPABILITIES
/// 属性路径恒报 ERROR_FILE_NOT_FOUND;lowbox 令牌 + CreateProcessAsUserW 成功。
pub fn create_lowbox_token(package_sid: &[u8]) -> Result<Token> {
    create_lowbox_token_with_capabilities(package_sid, &[])
}

/// 带能力列表的 lowbox 令牌:`capability_sids` = S-1-15-3-* 二进制 SID
/// (解析见 [`crate::appcontainer::capability_sid_from_name`])。空列表 = 断网。
pub fn create_lowbox_token_with_capabilities(
    package_sid: &[u8],
    capability_sids: &[Vec<u8>],
) -> Result<Token> {
    unsafe {
        let mut h_base = HANDLE::default();
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ASSIGN_PRIMARY
                | TOKEN_DUPLICATE
                | TOKEN_QUERY
                | TOKEN_IMPERSONATE
                | TOKEN_ADJUST_DEFAULT
                | TOKEN_ADJUST_SESSIONID,
            &mut h_base,
        )?;
        let _base = HandleGuard(h_base);

        // 能力 SID 以 SID_AND_ATTRIBUTES 数组传入;SE_GROUP_ENABLED 使其参与
        // 访问检查(与 DeriveCapabilitySidsFromName 的语义一致)
        let caps: Vec<SID_AND_ATTRIBUTES> = capability_sids
            .iter()
            .map(|s| SID_AND_ATTRIBUTES {
                Sid: PSID(s.as_ptr() as *const core::ffi::c_void as *mut _),
                Attributes: SE_GROUP_ENABLED,
            })
            .collect();

        let mut h_low = HANDLE::default();
        let nt = NtCreateLowBoxToken(
            &mut h_low,
            h_base,
            (TOKEN_ASSIGN_PRIMARY
                | TOKEN_DUPLICATE
                | TOKEN_QUERY
                | TOKEN_IMPERSONATE
                | TOKEN_ADJUST_DEFAULT
                | TOKEN_ADJUST_SESSIONID)
                .0,
            std::ptr::null(),
            package_sid.as_ptr() as *const core::ffi::c_void,
            caps.len() as u32,
            // 内核对 count=0 要求指针为 NULL(悬垂 as_ptr() 会报 0xC0000030)
            if caps.is_empty() {
                std::ptr::null()
            } else {
                caps.as_ptr() as *const core::ffi::c_void
            },
            0,
            std::ptr::null(),
        );
        if nt.0 < 0 {
            return Err(windows::core::Error::from_hresult(windows::core::HRESULT(nt.0)));
        }
        enable_change_notify(h_low)?;
        Ok(Token(h_low))
    }
}

/// 查询令牌的组 SID 列表(TokenGroups,二进制 SID;含 lowbox 的容器 SID)。
pub fn token_groups(token: HANDLE) -> Result<Vec<Vec<u8>>> {
    Ok(query_sid_and_attrs(token, TokenGroups)?
        .into_iter()
        .map(|(sid, _)| sid)
        .collect())
}

/// 查询令牌的能力 SID 列表(TokenCapabilities,二进制 SID + 属性位)。
/// lowbox 的 capability 在**这里**而非 TokenGroups——capabilities 授权的内核级断言点。
pub fn token_capabilities(token: HANDLE) -> Result<Vec<(Vec<u8>, u32)>> {
    query_sid_and_attrs(token, TokenCapabilities)
}

/// 查询 lowbox 令牌的容器(包)SID(TokenAppContainerSid)。
/// 包 SID 不进 TokenGroups/TokenCapabilities,这是它的正规查询点
/// (非 AC 令牌调用会报错,由调用方处理)。
pub fn token_app_container_sid(token: HANDLE) -> Result<Vec<u8>> {
    unsafe {
        let mut len = 0u32;
        let _ = GetTokenInformation(token, TokenAppContainerSid, None, 0, &mut len);
        let mut buf = vec![0u8; len as usize];
        GetTokenInformation(
            token,
            TokenAppContainerSid,
            Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
            len,
            &mut len,
        )?;
        let psid = (*(buf.as_ptr() as *const TOKEN_APPCONTAINER_INFORMATION)).TokenAppContainer;
        let count = *GetSidSubAuthorityCount(psid) as usize;
        Ok(std::slice::from_raw_parts(psid.0 as *const u8, 8 + 4 * count).to_vec())
    }
}

fn query_sid_and_attrs(
    token: HANDLE,
    class: windows::Win32::Security::TOKEN_INFORMATION_CLASS,
) -> Result<Vec<(Vec<u8>, u32)>> {
    unsafe {
        let mut len = 0u32;
        let _ = GetTokenInformation(token, class, None, 0, &mut len);
        if len == 0 {
            return Ok(vec![]);
        }
        let mut buf = vec![0u8; len as usize];
        GetTokenInformation(
            token,
            class,
            Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
            len,
            &mut len,
        )?;
        let tg = &*(buf.as_ptr() as *const TOKEN_GROUPS);
        let mut out = Vec::with_capacity(tg.GroupCount as usize);
        for entry in tg.Groups.iter().take(tg.GroupCount as usize) {
            let psid = entry.Sid;
            let count = *GetSidSubAuthorityCount(psid) as usize;
            let bytes = std::slice::from_raw_parts(psid.0 as *const u8, 8 + 4 * count);
            out.push((bytes.to_vec(), entry.Attributes));
        }
        Ok(out)
    }
}

fn enable_change_notify(token: HANDLE) -> Result<()> {
    unsafe {
        let mut luid = LUID::default();
        LookupPrivilegeValueW(PCWSTR::null(), w!("SeChangeNotifyPrivilege"), &mut luid)?;
        let tp = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: luid,
                Attributes: SE_PRIVILEGE_ENABLED,
            }],
        };
        // ERROR_NOT_ALL_ASSIGNED 时 BOOL 仍为真(特权未持有时)→ 不视为错误
        let _ = AdjustTokenPrivileges(token, false, Some(&tp), 0, None, None);
        Ok(())
    }
}

/// 当前进程用户的 SID(M2 redirect 用:视图 DACL 重建时保留用户访问,
/// 使父进程能继续管理视图;也让台账号 SID 与 ACE 侧身份可对照)。
pub fn process_user_sid() -> Result<Vec<u8>> {
    unsafe {
        let mut h = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut h)?;
        let _g = HandleGuard(h);
        let mut len = 0u32;
        let _ = GetTokenInformation(h, TokenUser, None, 0, &mut len);
        let mut buf = vec![0u8; len as usize];
        GetTokenInformation(
            h,
            TokenUser,
            Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
            len,
            &mut len,
        )?;
        let tu = &*(buf.as_ptr() as *const TOKEN_USER);
        let psid = tu.User.Sid;
        let count = *GetSidSubAuthorityCount(psid) as usize;
        Ok(std::slice::from_raw_parts(psid.0 as *const u8, 8 + 4 * count).to_vec())
    }
}

/// 当前进程是否提权(测试/自检用:spike 必须在非提权下跑)。
pub fn is_elevated() -> bool {    unsafe {
        let mut h = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut h).is_err() {
            return false;
        }
        let _g = HandleGuard(h);
        let mut elevation = TOKEN_ELEVATION::default();
        let mut ret = 0u32;
        if GetTokenInformation(
            h,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut core::ffi::c_void),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut ret,
        )
        .is_err()
        {
            return false;
        }
        elevation.TokenIsElevated != 0
    }
}

struct HandleGuard(HANDLE);

impl Drop for HandleGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
