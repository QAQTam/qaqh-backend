//! 受限令牌 spawn:CreateProcessAsUserW + Job(kill-on-close)+ 匿名管道 stdio。
//!
//! codex process.rs 的工程约束已吸收:lpApplicationName 留空走命令行解析;
//! CREATE_SUSPENDED → AssignProcessToJobObject → ResumeThread 的顺序;
//! 管道读端句柄清除继承位。私有桌面(lpDesktop)列 M1 后续补。

use std::fs::File;
use std::io::{self, Read};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::path::Path;
use windows::core::{PCWSTR, PWSTR, Result};
use windows::Win32::Foundation::{
    CloseHandle, HANDLE, HANDLE_FLAGS, HANDLE_FLAG_INHERIT, SetHandleInformation,
};
use windows::Win32::Security::SECURITY_ATTRIBUTES;
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOBOBJECT_BASIC_UI_RESTRICTIONS,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOB_OBJECT_UILIMIT_GLOBALATOMS, JOB_OBJECT_UILIMIT_HANDLES, JOB_OBJECT_UILIMIT_READCLIPBOARD,
    JOB_OBJECT_UILIMIT_WRITECLIPBOARD, JobObjectBasicUIRestrictions,
    JobObjectExtendedLimitInformation, SetInformationJobObject,
};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::Threading::{
    CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, EXTENDED_STARTUPINFO_PRESENT,
    CreateProcessAsUserW, DeleteProcThreadAttributeList, GetExitCodeProcess,
    InitializeProcThreadAttributeList, INFINITE, LPPROC_THREAD_ATTRIBUTE_LIST,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROCESS_INFORMATION, ResumeThread, STARTF_USESTDHANDLES,
    STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
};

/// 隔离后端(ADR-0002)。两种后端都以令牌形态进入 CreateProcessAsUserW:
/// - RestrictedToken:WRITE_RESTRICTED 受限令牌(只滤写,读透传)
/// - AppContainer:NtCreateLowBoxToken lowbox 令牌(全访问过 AC-SID 检查,
///   读可隔离;capability 空 = 内核断网)。
///
/// 工程事实(2026-10-04,build 26300 实证):PROC_THREAD_ATTRIBUTE_SECURITY_
/// CAPABILITIES 属性路径 CreateProcessW 恒报 ERROR_FILE_NOT_FOUND(镜像以
/// lowbox 上下文打开失败);改走 lowbox 令牌 + CreateProcessAsUserW 成功,
/// 且同样命中"直接子令牌"免提权豁免(ADR-0001 事实 1)。
pub enum Isolation {
    RestrictedToken { token: HANDLE },
    AppContainer { token: HANDLE },
}

pub struct Exit {
    pub code: u32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

pub struct Child {
    pub pid: u32,
    process: HANDLE,
    thread: HANDLE,
    job: HANDLE,
    stdout: Option<OwnedHandle>,
    stderr: Option<OwnedHandle>,
    waited: bool,
}

unsafe impl Send for Child {}

impl Child {
    /// 非阻塞探测退出;None = 仍在运行(超时轮询用)。
    pub fn try_wait(&self) -> io::Result<Option<u32>> {
        unsafe {
            let ev = WaitForSingleObject(self.process, 0);
            if ev == windows::Win32::Foundation::WAIT_OBJECT_0 {
                let mut code = 0u32;
                GetExitCodeProcess(self.process, &mut code)?;
                Ok(Some(code))
            } else {
                Ok(None)
            }
        }
    }

    /// 等待退出并收割管道输出。
    pub fn wait(mut self) -> io::Result<Exit> {
        self.waited = true;
        let t_out = self.stdout.take().map(spawn_pipe_reader);
        let t_err = self.stderr.take().map(spawn_pipe_reader);
        unsafe {
            let _ = WaitForSingleObject(self.process, INFINITE);
        }
        let mut code = 0u32;
        unsafe {
            GetExitCodeProcess(self.process, &mut code)?;
        }
        let stdout = t_out.and_then(|t| t.join().ok()).unwrap_or_default();
        let stderr = t_err.and_then(|t| t.join().ok()).unwrap_or_default();
        Ok(Exit {
            code,
            stdout,
            stderr,
        })
    }

    /// 关闭 Job 句柄 = 终结整棵进程树(spec §5.6)。
    pub fn kill_tree(&mut self) {
        unsafe {
            if !self.job.is_invalid() {
                let _ = CloseHandle(self.job);
                self.job = HANDLE::default();
            }
        }
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        unsafe {
            if !self.waited {
                if !self.job.is_invalid() {
                    let _ = CloseHandle(self.job);
                }
                let _ = TerminateProcess(self.process, 1);
            }
            let _ = CloseHandle(self.thread);
            let _ = CloseHandle(self.process);
        }
    }
}

fn spawn_pipe_reader(f: OwnedHandle) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut file = File::from(f);
        let mut buf = Vec::new();
        let _ = file.read_to_end(&mut buf);
        buf
    })
}

fn wide(p: &Path) -> Vec<u16> {
    let mut v: Vec<u16> = p.as_os_str().encode_wide().collect();
    v.push(0);
    v
}

/// 环境块:按键名大小写不敏感排序,UTF-16,双 null 结尾。
pub fn build_env_block(env: &[(String, String)]) -> Vec<u16> {
    let mut sorted: Vec<&(String, String)> = env.iter().collect();
    sorted.sort_by(|a, b| a.0.to_uppercase().cmp(&b.0.to_uppercase()));
    let mut block: Vec<u16> = Vec::new();
    for (k, v) in sorted {
        block.extend(k.encode_utf16());
        block.push('=' as u16);
        block.extend(v.encode_utf16());
        block.push(0);
    }
    block.push(0);
    block
}

/// 以给定隔离后端 spawn `cmdline`(完整命令行字符串)。
/// `desktop`:私有桌面名(受限令牌/AppContainer 下必填,见 desktop.rs 模块注释)。
pub fn spawn(
    isolation: &Isolation,
    cmdline: &str,
    cwd: &Path,
    env: &[(String, String)],
    desktop: Option<&str>,
) -> Result<Child> {
    unsafe {
        let job = CreateJobObjectW(None, PCWSTR::null())?;
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &limits as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )?;
        // UI 限制:私有桌面只隔离窗口消息,剪贴板与全局原子表跟窗口站走。
        // UILIMIT_HANDLES 禁止沙箱进程使用 job 外的 USER 句柄(conhost 在 job 内,不受影响)。
        let ui = JOBOBJECT_BASIC_UI_RESTRICTIONS {
            UIRestrictionsClass: JOB_OBJECT_UILIMIT_READCLIPBOARD
                | JOB_OBJECT_UILIMIT_WRITECLIPBOARD
                | JOB_OBJECT_UILIMIT_HANDLES
                | JOB_OBJECT_UILIMIT_GLOBALATOMS,
        };
        SetInformationJobObject(
            job,
            JobObjectBasicUIRestrictions,
            &ui as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_BASIC_UI_RESTRICTIONS>() as u32,
        )?;

        // 匿名管道:stdout/stderr 读端归父(清继承位),写端随 bInheritHandles 进入子进程;
        // stdin 用空管道读端(写端立即关闭 → 子进程读入即 EOF),替代无效句柄。
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: std::ptr::null_mut(),
            bInheritHandle: true.into(),
        };
        let (out_r, out_w) = make_pipe(&sa)?;
        let (err_r, err_w) = make_pipe(&sa)?;
        let (in_r, in_w) = make_stdin_pipe(&sa)?;

        // 继承面限定:bInheritHandles=TRUE 时子进程默认继承父进程全部可继承句柄
        // (journal/sidstore 若可继承即成 DACL 旁路)。HANDLE_LIST 属性把继承
        // 收敛到恰好三根 stdio 管道端点。
        let attr_count = 1u32;
        let mut size = 0usize;
        // 探测所需字节数(必须用与真实调用相同的属性数,否则二次初始化报 0x7A)
        let _ = InitializeProcThreadAttributeList(
            LPPROC_THREAD_ATTRIBUTE_LIST(std::ptr::null_mut()),
            attr_count,
            0,
            &mut size,
        );
        if size == 0 {
            return Err(windows::core::Error::from_hresult(
                windows::core::HRESULT::from_win32(
                    windows::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER.0,
                ),
            ));
        }
        let mut attr_buf = vec![0u8; size];
        let attr = LPPROC_THREAD_ATTRIBUTE_LIST(attr_buf.as_mut_ptr() as *mut core::ffi::c_void);
        InitializeProcThreadAttributeList(attr, attr_count, 0, &mut size)?;
        let inherit_handles: [HANDLE; 3] = [in_r, out_w, err_w];
        UpdateProcThreadAttribute(
            attr,
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            Some(inherit_handles.as_ptr() as *const core::ffi::c_void),
            std::mem::size_of::<HANDLE>() * inherit_handles.len(),
            None,
            None,
        )?;
        let env_block = build_env_block(env);
        let mut cmd_w: Vec<u16> = cmdline.encode_utf16().chain([0]).collect();
        let cwd_w = wide(cwd);

        let mut si = STARTUPINFOEXW::default();
        si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        si.StartupInfo.hStdInput = in_r;
        si.StartupInfo.hStdOutput = out_w;
        si.StartupInfo.hStdError = err_w;
        si.lpAttributeList = attr;
        let desktop_w: Option<Vec<u16>> = desktop.map(|d| d.encode_utf16().chain([0]).collect());
        if let Some(dw) = &desktop_w {
            si.StartupInfo.lpDesktop = PWSTR(dw.as_ptr() as *mut u16);
        }

        let mut pi = PROCESS_INFORMATION::default();
        let startupinfo_ptr =
            &mut si as *mut STARTUPINFOEXW as *const windows::Win32::System::Threading::STARTUPINFOW;
        // 两种后端统一走 CreateProcessAsUserW(受限令牌 / lowbox 令牌,
        // 均为进程主令牌的直接子令牌 → 免 SeAssignPrimaryTokenPrivilege)
        let token = match isolation {
            Isolation::RestrictedToken { token } | Isolation::AppContainer { token } => *token,
        };
        let created = CreateProcessAsUserW(
            token,
            PCWSTR::null(),
            PWSTR(cmd_w.as_mut_ptr()),
            None,
            None,
            true,
            CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT,
            Some(env_block.as_ptr() as *const core::ffi::c_void),
            PCWSTR(cwd_w.as_ptr()),
            startupinfo_ptr,
            &mut pi,
        )
        .map_err(|e| ctx(e, "CreateProcessAsUserW"));

        // 无论成败都要关闭我方写端副本,否则读端等不到 EOF
        let _ = CloseHandle(out_w);
        let _ = CloseHandle(err_w);
        let _ = CloseHandle(in_r);
        let _ = CloseHandle(in_w);
        DeleteProcThreadAttributeList(attr);
        created?;

        AssignProcessToJobObject(job, pi.hProcess).map_err(|e| ctx(e, "AssignProcessToJobObject"))?;
        ResumeThread(pi.hThread);

        Ok(Child {
            pid: pi.dwProcessId,
            process: pi.hProcess,
            thread: pi.hThread,
            job,
            stdout: Some(out_r),
            stderr: Some(err_r),
            waited: false,
        })
    }
}

fn ctx(e: windows::core::Error, step: &str) -> windows::core::Error {
    windows::core::Error::new(e.code(), format!("{step}: {e}"))
}

fn make_pipe(sa: &SECURITY_ATTRIBUTES) -> Result<(OwnedHandle, HANDLE)> {
    unsafe {
        let mut r = HANDLE::default();
        let mut w = HANDLE::default();
        CreatePipe(&mut r, &mut w, Some(sa), 0)?;
        SetHandleInformation(r, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0))?;
        Ok((OwnedHandle::from_raw_handle(r.0 as _), w))
    }
}

/// stdin 管道:读端保持继承位(随 bInheritHandles 进入子进程),
/// 写端归父并在 spawn 后立即关闭 —— 子进程读 stdin 即 EOF,而非无效句柄。
fn make_stdin_pipe(sa: &SECURITY_ATTRIBUTES) -> Result<(HANDLE, HANDLE)> {
    unsafe {
        let mut r = HANDLE::default();
        let mut w = HANDLE::default();
        CreatePipe(&mut r, &mut w, Some(sa), 0)?;
        SetHandleInformation(w, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0))?;
        Ok((r, w))
    }
}
