//! 原生二进制内核级写保护验证器（`sbx verify` / `sbx probe-worker` / `sbx probe-grandchild`）。
//!
//! 目的：用纯编译期原生 `.exe` 直接调用 Win32 / `std::fs` 系统调用（零 shell 文本解析、
//! 零进程名/命令行黑名单），对比「裸跑基线」与「QAQHarness Windows 沙箱」的真实内核行为：
//! - 进程令牌自检：`IsTokenRestricted`、`TokenElevation`、`TokenRestrictedSids`、
//!   特权裁剪计数、私有桌面名、`IsProcessInJob`；
//! - 文件系统内核写隔离：工作区内允许写、工作区外 `os error 5 (Access is denied)`、
//!   `.git/hooks` 与 `.qaqh/skills` 显式 Deny ACE 写拒 + `.git/config` 读透传；
//! - 孙进程继承：原生二进制再 spawn 自身子进程写工作区外，证明受限令牌与 Job Object 随进程树继承；
//! - 注册表写隔离：`RegCreateKeyExW(HKCU\Software\...)` 被内核拒绝（error 5）；
//! - Job UI 隔离：`OpenClipboard` 被 Job UILimit 拒绝；
//! - ProjFS RedirectPlane（若宿主机启用 `Client-ProjFS`）：虚拟视图写隔离 + merge 前真实工作区零污染 +
//!   `ConfirmDeletions` 删除拦截。

use sbx_win::{
    acl, desktop, env, events,
    policy::{AceMode, AceOp, AceTarget, IsolationKind, NetworkPolicy, Precreate, SbxPolicy, build_ace_plan},
    projfs, redirect, sid,
    spawn::{self, Isolation},
    token,
};
use serde_json::{Value, json};
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::time::Instant;

// ── Win32 原生内核查询绑定（直接读内核对象状态，不依赖任何上层抽象） ──

type Handle = *mut c_void;
type Bool = i32;
type Dword = u32;

const TOKEN_QUERY: Dword = 0x0008;
const TOKEN_PRIVILEGES_CLASS: Dword = 3;
const TOKEN_RESTRICTED_SIDS_CLASS: Dword = 11;
const TOKEN_ELEVATION_CLASS: Dword = 20;
const SE_PRIVILEGE_ENABLED: Dword = 0x0000_0002;
const UOI_NAME: Dword = 2;
const HKEY_CURRENT_USER: isize = 0x8000_0001_u32 as isize;
const KEY_WRITE: Dword = 0x0002_0006;
const ERROR_SUCCESS: Dword = 0;

#[repr(C)]
struct Luid {
    low_part: Dword,
    high_part: i32,
}

#[repr(C)]
struct LuidAndAttributes {
    luid: Luid,
    attributes: Dword,
}

#[repr(C)]
struct SidAndAttributes {
    sid: *mut c_void,
    attributes: Dword,
}

#[repr(C)]
struct TokenElevation {
    token_is_elevated: Dword,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentProcess() -> Handle;
    fn GetCurrentThreadId() -> Dword;
    fn CloseHandle(h: Handle) -> Bool;
    fn IsProcessInJob(process: Handle, job: Handle, result: *mut Bool) -> Bool;
    fn LocalFree(hmem: Handle) -> Handle;
}

#[link(name = "advapi32")]
unsafe extern "system" {
    fn OpenProcessToken(process: Handle, access: Dword, token: *mut Handle) -> Bool;
    fn IsTokenRestricted(token: Handle) -> Bool;
    fn GetTokenInformation(
        token: Handle,
        info_class: Dword,
        info: *mut c_void,
        info_len: Dword,
        return_len: *mut Dword,
    ) -> Bool;
    fn ConvertSidToStringSidW(sid: *mut c_void, string_sid: *mut *mut u16) -> Bool;
    fn RegCreateKeyExW(
        hkey: isize,
        subkey: *const u16,
        reserved: Dword,
        class: *const u16,
        options: Dword,
        sam_desired: Dword,
        security_attrs: *const c_void,
        result: *mut isize,
        disposition: *mut Dword,
    ) -> Dword;
    fn RegCloseKey(hkey: isize) -> Dword;
    fn RegDeleteKeyW(hkey: isize, subkey: *const u16) -> Dword;
}

#[link(name = "user32")]
unsafe extern "system" {
    fn GetThreadDesktop(thread_id: Dword) -> Handle;
    fn GetUserObjectInformationW(
        obj: Handle,
        index: Dword,
        info: *mut c_void,
        len: Dword,
        needed: *mut Dword,
    ) -> Bool;
    fn OpenClipboard(hwnd: Handle) -> Bool;
    fn CloseClipboard() -> Bool;
}

fn wide_z(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

fn inspect_kernel_token() -> Value {
    let mut is_restricted = false;
    let mut is_elevated = false;
    let mut enabled_privileges = 0u32;
    let mut total_privileges = 0u32;
    let mut restricting_sids: Vec<String> = Vec::new();
    let mut in_job = false;
    let mut desktop_name = String::from("<unknown>");

    unsafe {
        let mut h_tok: Handle = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut h_tok) != 0 {
            is_restricted = IsTokenRestricted(h_tok) != 0;

            // Elevation
            let mut elev = TokenElevation {
                token_is_elevated: 0,
            };
            let mut ret_len = 0u32;
            if GetTokenInformation(
                h_tok,
                TOKEN_ELEVATION_CLASS,
                (&raw mut elev).cast::<c_void>(),
                std::mem::size_of::<TokenElevation>() as Dword,
                &mut ret_len,
            ) != 0
            {
                is_elevated = elev.token_is_elevated != 0;
            }

            // Privileges
            let mut priv_len = 0u32;
            GetTokenInformation(
                h_tok,
                TOKEN_PRIVILEGES_CLASS,
                std::ptr::null_mut(),
                0,
                &mut priv_len,
            );
            if priv_len > 0 {
                let mut buf = vec![0u8; priv_len as usize];
                if GetTokenInformation(
                    h_tok,
                    TOKEN_PRIVILEGES_CLASS,
                    buf.as_mut_ptr().cast::<c_void>(),
                    priv_len,
                    &mut priv_len,
                ) != 0
                {
                    let count = u32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]);
                    total_privileges = count;
                    let base = buf.as_ptr().add(4).cast::<LuidAndAttributes>();
                    for i in 0..count as usize {
                        let entry = std::ptr::read_unaligned(base.add(i));
                        if (entry.attributes & SE_PRIVILEGE_ENABLED) != 0 {
                            enabled_privileges += 1;
                        }
                    }
                }
            }

            // Restricting SIDs
            let mut rsid_len = 0u32;
            GetTokenInformation(
                h_tok,
                TOKEN_RESTRICTED_SIDS_CLASS,
                std::ptr::null_mut(),
                0,
                &mut rsid_len,
            );
            if rsid_len > 0 {
                let mut buf = vec![0u8; rsid_len as usize];
                if GetTokenInformation(
                    h_tok,
                    TOKEN_RESTRICTED_SIDS_CLASS,
                    buf.as_mut_ptr().cast::<c_void>(),
                    rsid_len,
                    &mut rsid_len,
                ) != 0
                {
                    let count = u32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]);
                    let groups_offset = std::mem::align_of::<SidAndAttributes>();
                    let base = buf.as_ptr().add(groups_offset).cast::<SidAndAttributes>();
                    for i in 0..count as usize {
                        let entry = std::ptr::read_unaligned(base.add(i));
                        let mut str_ptr: *mut u16 = std::ptr::null_mut();
                        if ConvertSidToStringSidW(entry.sid, &mut str_ptr) != 0
                            && !str_ptr.is_null()
                        {
                            let mut n = 0usize;
                            while *str_ptr.add(n) != 0 {
                                n += 1;
                            }
                            let slice = std::slice::from_raw_parts(str_ptr, n);
                            restricting_sids.push(String::from_utf16_lossy(slice));
                            LocalFree(str_ptr.cast::<c_void>());
                        }
                    }
                }
            }

            CloseHandle(h_tok);
        }

        // Job Object membership
        let mut job_flag: Bool = 0;
        if IsProcessInJob(GetCurrentProcess(), std::ptr::null_mut(), &mut job_flag) != 0 {
            in_job = job_flag != 0;
        }

        // Private Desktop name
        let h_desk = GetThreadDesktop(GetCurrentThreadId());
        if !h_desk.is_null() {
            let mut needed = 0u32;
            GetUserObjectInformationW(h_desk, UOI_NAME, std::ptr::null_mut(), 0, &mut needed);
            if needed >= 2 {
                let mut buf = vec![0u16; (needed as usize).div_ceil(2)];
                if GetUserObjectInformationW(
                    h_desk,
                    UOI_NAME,
                    buf.as_mut_ptr().cast::<c_void>(),
                    needed,
                    &mut needed,
                ) != 0
                {
                    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
                    desktop_name = String::from_utf16_lossy(&buf[..len]);
                }
            }
        }
    }

    json!({
        "is_token_restricted": is_restricted,
        "is_elevated": is_elevated,
        "restricting_sids": restricting_sids,
        "enabled_privileges": enabled_privileges,
        "total_privileges": total_privileges,
        "in_job_object": in_job,
        "desktop_name": desktop_name,
    })
}

fn probe_write_file(path: &Path, content: &str) -> Value {
    match std::fs::write(path, content) {
        Ok(()) => json!({ "allowed": true, "os_error": null, "detail": "write succeeded" }),
        Err(err) => json!({
            "allowed": false,
            "os_error": err.raw_os_error(),
            "detail": err.to_string(),
        }),
    }
}

fn probe_read_file(path: &Path, expected_substr: &str) -> Value {
    match std::fs::read_to_string(path) {
        Ok(content) => json!({
            "allowed": content.contains(expected_substr),
            "os_error": null,
            "detail": format!("read {} bytes", content.len()),
        }),
        Err(err) => json!({
            "allowed": false,
            "os_error": err.raw_os_error(),
            "detail": err.to_string(),
        }),
    }
}

fn probe_registry_write(subkey: &str) -> Value {
    let sub_w = wide_z(subkey);
    let mut h_out: isize = 0;
    let mut disp: Dword = 0;
    let code = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            sub_w.as_ptr(),
            0,
            std::ptr::null(),
            0,
            KEY_WRITE,
            std::ptr::null(),
            &mut h_out,
            &mut disp,
        )
    };
    if code == ERROR_SUCCESS {
        unsafe {
            RegCloseKey(h_out);
            RegDeleteKeyW(HKEY_CURRENT_USER, sub_w.as_ptr());
        }
        json!({ "allowed": true, "os_error": 0, "detail": "HKCU key created & cleaned" })
    } else {
        json!({
            "allowed": false,
            "os_error": code,
            "detail": format!("Win32 error {code}"),
        })
    }
}

fn probe_clipboard() -> Value {
    let ok = unsafe { OpenClipboard(std::ptr::null_mut()) != 0 };
    if ok {
        unsafe {
            CloseClipboard();
        }
        json!({ "allowed": true, "detail": "OpenClipboard succeeded" })
    } else {
        json!({ "allowed": false, "detail": "OpenClipboard denied (Job UILimit)" })
    }
}

fn probe_grandchild_write(target: &Path) -> Value {
    let Ok(exe) = std::env::current_exe() else {
        return json!({ "allowed": false, "os_error": null, "detail": "current_exe unavailable" });
    };
    match std::process::Command::new(exe)
        .arg("probe-grandchild")
        .arg(target)
        .output()
    {
        Ok(out) => {
            let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
            serde_json::from_str(&text).unwrap_or_else(|_| {
                json!({
                    "allowed": out.status.success(),
                    "os_error": out.status.code(),
                    "detail": format!("exit={:?} stdout={text}", out.status.code()),
                })
            })
        }
        Err(err) => json!({
            "allowed": false,
            "os_error": err.raw_os_error(),
            "detail": format!("spawn grandchild failed: {err}"),
        }),
    }
}

/// 孙进程入口：`sbx probe-grandchild <target-file>`。
pub fn cmd_probe_grandchild(args: &[String]) -> i32 {
    let Some(target) = args.first() else {
        eprintln!("probe-grandchild: missing target path");
        return 2;
    };
    let result = probe_write_file(Path::new(target), "grandchild-escape");
    println!("{result}");
    if result["allowed"].as_bool().unwrap_or(false) {
        0
    } else {
        1
    }
}

/// 探针子进程入口：`sbx probe-worker --ws <DIR> --outside <DIR> --reg-key <SUBKEY> [--delete-file <FILE>]`。
/// 也可以直接在 QAQHarness 的 `exec_command` 里调用本命令，独立验证线上守护进程的真实沙箱边界。
pub fn cmd_probe_worker(args: &[String]) -> i32 {
    let mut ws: Option<PathBuf> = None;
    let mut outside: Option<PathBuf> = None;
    let mut reg_key = String::from("Software\\qaqh-sbx-verify-probe");
    let mut delete_file: Option<PathBuf> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--ws" => {
                ws = args.get(i + 1).map(PathBuf::from);
                i += 2;
            }
            "--outside" => {
                outside = args.get(i + 1).map(PathBuf::from);
                i += 2;
            }
            "--reg-key" => {
                if let Some(v) = args.get(i + 1) {
                    reg_key = v.clone();
                }
                i += 2;
            }
            "--delete-file" => {
                delete_file = args.get(i + 1).map(PathBuf::from);
                i += 2;
            }
            other => {
                eprintln!("probe-worker: unknown flag {other}");
                return 2;
            }
        }
    }

    let ws = ws.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let outside = outside.unwrap_or_else(|| std::env::temp_dir().join("qaqh-sbx-outside-probe"));

    let kernel = inspect_kernel_token();
    let ws_write = probe_write_file(&ws.join("probe_allowed.txt"), "authorized-write-ok");
    let outside_write = probe_write_file(&outside.join("probe_escape.txt"), "unauthorized-escape");
    let git_hooks_write = probe_write_file(
        &ws.join(".git").join("hooks").join("pre-commit"),
        "#!/bin/sh\necho pwned\n",
    );
    let skills_write = probe_write_file(
        &ws.join(".qaqh").join("skills").join("injected.md"),
        "# injected skill",
    );
    let git_config_read = probe_read_file(&ws.join(".git").join("config"), "gitcfg");
    let grandchild_write = probe_grandchild_write(&outside.join("probe_grandchild.txt"));
    let reg_write = probe_registry_write(&reg_key);
    let clipboard = probe_clipboard();

    let deleted_target = delete_file.map(|p| match std::fs::remove_file(&p) {
        Ok(()) => json!({ "allowed": true, "detail": "removed in current view" }),
        Err(err) => json!({ "allowed": false, "os_error": err.raw_os_error(), "detail": err.to_string() }),
    });

    let report = json!({
        "pid": std::process::id(),
        "kernel": kernel,
        "env": {
            "TEMP": std::env::var("TEMP").ok(),
            "TMP": std::env::var("TMP").ok(),
            "USERPROFILE": std::env::var("USERPROFILE").ok(),
            "HOME": std::env::var("HOME").ok(),
        },
        "probes": {
            "write_authorized_workspace": ws_write,
            "write_outside_workspace": outside_write,
            "write_git_hooks_carveout": git_hooks_write,
            "write_skills_carveout": skills_write,
            "read_git_config_passthrough": git_config_read,
            "grandchild_write_outside": grandchild_write,
            "write_registry_unauthorized": reg_write,
            "clipboard_open": clipboard,
            "delete_precious_file": deleted_target,
        }
    });

    println!("{report}");
    0
}

fn prepare_fixture(root: &Path) -> std::io::Result<(PathBuf, PathBuf)> {
    let ws = root.join("ws");
    let outside = root.join("outside");
    std::fs::create_dir_all(ws.join(".git").join("hooks"))?;
    std::fs::create_dir_all(ws.join(".qaqh").join("skills"))?;
    std::fs::create_dir_all(&outside)?;
    std::fs::write(ws.join(".git").join("config"), "[core]\n\tmarker = gitcfg\n")?;
    std::fs::write(ws.join("precious.txt"), "do-not-delete")?;
    std::fs::write(outside.join("existing.txt"), "untouched")?;
    Ok((ws, outside))
}

fn quote(p: &Path) -> String {
    format!("\"{}\"", p.display())
}

fn parse_worker_json(bytes: &[u8]) -> Result<Value, String> {
    let text = String::from_utf8_lossy(bytes);
    serde_json::from_str(text.trim()).map_err(|e| format!("parse worker json: {e} (raw={text:?})"))
}

fn fmt_probe(v: &Value) -> String {
    let allowed = v["allowed"].as_bool().unwrap_or(false);
    if allowed {
        "ALLOW".to_string()
    } else if let Some(code) = v["os_error"].as_i64() {
        format!("DENY (Win32 err={code})")
    } else {
        format!("DENY ({})", v["detail"].as_str().unwrap_or("blocked"))
    }
}

/// 一键对比验证：`sbx verify [--json]`。
pub fn cmd_verify(args: &[String]) -> i32 {
    let json_only = args.iter().any(|a| a == "--json");
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("verify: cannot locate self binary: {e}");
            return 1;
        }
    };

    let base = std::env::temp_dir().join(format!(
        "qaqh-sbx-verify-{}-{}",
        std::process::id(),
        events::now_millis()
    ));
    let reg_subkey = format!("Software\\qaqh-sbx-verify-{}", std::process::id());

    // ── 1. 对照组（无沙箱裸跑） ──
    let base_ctrl = base.join("baseline");
    let (ws_ctrl, out_ctrl) = match prepare_fixture(&base_ctrl) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("verify: fixture failed: {e}");
            return 1;
        }
    };
    let t_ctrl = Instant::now();
    let ctrl_out = match std::process::Command::new(&exe)
        .args([
            "probe-worker",
            "--ws",
            &ws_ctrl.to_string_lossy(),
            "--outside",
            &out_ctrl.to_string_lossy(),
            "--reg-key",
            &reg_subkey,
        ])
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&base);
            eprintln!("verify: baseline spawn failed: {e}");
            return 1;
        }
    };
    let ctrl_ms = t_ctrl.elapsed().as_millis() as u64;
    let baseline = match parse_worker_json(&ctrl_out.stdout) {
        Ok(v) => v,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&base);
            eprintln!("verify: baseline {e}");
            return 1;
        }
    };

    // ── 2. 实验组 A：QAQHarness Windows 沙箱 TokenPlane ──
    let base_tok = base.join("token_plane");
    let (ws_tok, out_tok) = match prepare_fixture(&base_tok) {
        Ok(v) => v,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&base);
            eprintln!("verify: token fixture failed: {e}");
            return 1;
        }
    };
    let scratch_tok = base_tok.join("scratch");
    let _ = std::fs::create_dir_all(scratch_tok.join("tmp"));
    let _ = std::fs::create_dir_all(scratch_tok.join("home"));

    let t_tok = Instant::now();
    let cap = match sid::capability_sid() {
        Ok(c) => c,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&base);
            eprintln!("verify: capability_sid failed: {e}");
            return 1;
        }
    };
    let cap_text = sid::sid_to_string(&cap);
    let policy_tok = SbxPolicy {
        writable_roots: vec![ws_tok.clone()],
        writable_files: vec![],
        deny_write_paths: vec![
            ws_tok.join(".git").join("hooks"),
            ws_tok.join(".qaqh").join("skills"),
        ],
        network: NetworkPolicy::Deny,
        isolation: IsolationKind::Token,
        readable_roots: vec![],
        capabilities: vec![],
        writable_registry_keys: vec![],
        redirect: false,
    };
    let mut plan_tok = build_ace_plan(&policy_tok, &cap);
    plan_tok.push(AceOp {
        path: scratch_tok.clone(),
        mode: AceMode::Allow,
        sid: cap.clone(),
        inherit_tree: true,
        precreate: Precreate::None,
        full_mask: false,
        kind: AceTarget::File,
    });
    if let Err(e) = acl::apply_ops(&plan_tok, &events::NullSink) {
        let _ = std::fs::remove_dir_all(&base);
        eprintln!("verify: apply_ops failed: {e}");
        return 1;
    }
    let restricted_tok = match token::create_restricted_token(&[cap.clone(), sid::everyone_sid()]) {
        Ok(t) => t,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&base);
            eprintln!("verify: create_restricted_token failed: {e}");
            return 1;
        }
    };
    let priv_desk = match desktop::create_private_desktop(&cap_text) {
        Ok(d) => d,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&base);
            eprintln!("verify: create_private_desktop failed: {e}");
            return 1;
        }
    };
    let env_tok = env::minimal_child_env(&scratch_tok.join("tmp"), &scratch_tok.join("home"));
    let cmdline_tok = format!(
        "{} probe-worker --ws {} --outside {} --reg-key {}",
        quote(&exe),
        quote(&ws_tok),
        quote(&out_tok),
        reg_subkey
    );
    let tok_exit = match spawn::spawn(
        &Isolation::RestrictedToken {
            token: restricted_tok.handle(),
        },
        &cmdline_tok,
        &ws_tok,
        &env_tok,
        Some(priv_desk.name()),
    )
    .and_then(|c| c.wait().map_err(Into::into))
    {
        Ok(e) => e,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&base);
            eprintln!("verify: token spawn failed: {e}");
            return 1;
        }
    };
    let tok_ms = t_tok.elapsed().as_millis() as u64;
    let token_plane = match parse_worker_json(&tok_exit.stdout) {
        Ok(v) => v,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&base);
            eprintln!(
                "verify: token_plane exit={} (0x{:08X}) stderr={:?} {e}",
                tok_exit.code,
                tok_exit.code,
                String::from_utf8_lossy(&tok_exit.stderr)
            );
            return 1;
        }
    };

    // ── 3. 实验组 B：ProjFS RedirectPlane（若宿主机已启用 Client-ProjFS） ──
    let projfs_available = projfs::available();
    let redirect_plane = if projfs_available {
        let base_red = base.join("redirect_plane");
        if let Ok((ws_red, out_red)) = prepare_fixture(&base_red) {
            let scratch_red = base_red.join("scratch");
            let view_red = scratch_red.join("view");
            let _ = std::fs::create_dir_all(scratch_red.join("tmp"));
            let _ = std::fs::create_dir_all(scratch_red.join("home"));
            let t_red = Instant::now();
            let policy_red = SbxPolicy {
                writable_roots: vec![ws_red.clone()],
                writable_files: vec![],
                deny_write_paths: vec![
                    ws_red.join(".git").join("hooks"),
                    ws_red.join(".qaqh").join("skills"),
                ],
                network: NetworkPolicy::Deny,
                isolation: IsolationKind::Token,
                readable_roots: vec![],
                capabilities: vec![],
                writable_registry_keys: vec![],
                redirect: true,
            };
            let run_res = (|| -> Result<Value, String> {
                let split = redirect::split_plan(&policy_red, &cap, &ws_red, &view_red)
                    .map_err(|e| format!("split_plan: {e}"))?;
                let mut inplace = split.inplace;
                inplace.push(AceOp {
                    path: scratch_red.clone(),
                    mode: AceMode::Allow,
                    sid: cap.clone(),
                    inherit_tree: true,
                    precreate: Precreate::None,
                    full_mask: false,
                    kind: AceTarget::File,
                });
                acl::apply_ops(&inplace, &events::NullSink)
                    .map_err(|e| format!("apply_ops: {e}"))?;
                let turn = redirect::prepare_turn(&ws_red, &view_red, &split.view)
                    .map_err(|e| format!("prepare_turn: {e}"))?;
                let env_red =
                    env::minimal_child_env(&scratch_red.join("tmp"), &scratch_red.join("home"));
                let cmdline_red = format!(
                    "{} probe-worker --ws {} --outside {} --reg-key {} --delete-file {}",
                    quote(&exe),
                    quote(turn.root()),
                    quote(&out_red),
                    reg_subkey,
                    quote(&turn.root().join("precious.txt")),
                );
                let exit = spawn::spawn(
                    &Isolation::RestrictedToken {
                        token: restricted_tok.handle(),
                    },
                    &cmdline_red,
                    turn.root(),
                    &env_red,
                    Some(priv_desk.name()),
                )
                .map_err(|e| format!("spawn: {e}"))?
                .wait()
                .map_err(|e| format!("wait: {e}"))?;
                let worker = parse_worker_json(&exit.stdout)?;
                let store_clean_before_merge = !ws_red.join("probe_allowed.txt").exists()
                    && ws_red.join("precious.txt").exists();
                let merge_report = turn.merge().map_err(|e| format!("merge: {e}"))?;
                let store_applied_after_merge = ws_red.join("probe_allowed.txt").exists();
                let deletion_blocked_on_store = ws_red.join("precious.txt").exists();
                Ok(json!({
                    "available": true,
                    "duration_ms": t_red.elapsed().as_millis() as u64,
                    "store_clean_before_merge": store_clean_before_merge,
                    "store_applied_after_merge": store_applied_after_merge,
                    "deletion_blocked_on_store": deletion_blocked_on_store,
                    "merge_applied": merge_report.applied,
                    "merge_skipped_deletions": merge_report.skipped_deletions,
                    "worker": worker,
                }))
            })();
            match run_res {
                Ok(v) => v,
                Err(e) => json!({ "available": true, "error": e }),
            }
        } else {
            json!({ "available": true, "error": "fixture setup failed" })
        }
    } else {
        json!({
            "available": false,
            "remediation": "DISM /Online /Enable-Feature /FeatureName:Client-ProjFS /NoRestart"
        })
    };

    // 硬判定：全部关键内核写保护断言
    let tp = &token_plane["probes"];
    let tk = &token_plane["kernel"];
    let bp = &baseline["probes"];
    let all_pass = bp["write_outside_workspace"]["allowed"] == json!(true)
        && tk["is_token_restricted"] == json!(true)
        && tk["in_job_object"] == json!(true)
        && tp["write_authorized_workspace"]["allowed"] == json!(true)
        && tp["write_outside_workspace"]["allowed"] == json!(false)
        && tp["write_git_hooks_carveout"]["allowed"] == json!(false)
        && tp["write_skills_carveout"]["allowed"] == json!(false)
        && tp["read_git_config_passthrough"]["allowed"] == json!(true)
        && tp["grandchild_write_outside"]["allowed"] == json!(false)
        && tp["write_registry_unauthorized"]["allowed"] == json!(false)
        && tp["clipboard_open"]["allowed"] == json!(false);

    let full_json = json!({
        "verdict": if all_pass { "KERNEL_WRITE_ISOLATION_VERIFIED" } else { "FAILED" },
        "binary": exe.display().to_string(),
        "host_elevated": token::is_elevated(),
        "cap_sid": cap_text,
        "timing_ms": {
            "baseline": ctrl_ms,
            "token_plane_setup_and_run": tok_ms,
        },
        "baseline": baseline,
        "token_plane": token_plane,
        "redirect_plane": redirect_plane,
    });

    let _ = std::fs::remove_dir_all(&base);

    if json_only {
        println!(
            "{}",
            serde_json::to_string_pretty(&full_json).unwrap_or_default()
        );
    } else {
        println!("=== QAQHarness Windows Sandbox Kernel Verification ===");
        println!("Binary:        {}", exe.display());
        println!("Host Elevated: {}", token::is_elevated());
        println!("Cap SID:       {cap_text}");
        println!(
            "Timing:        baseline={}ms | token_plane(ACL+Token+Desktop+Job+Run)={}ms",
            ctrl_ms, tok_ms
        );
        println!();
        println!("--- Kernel Token & Containment Facts ---");
        println!(
            "  IsTokenRestricted : baseline={}  ->  sandbox={}",
            baseline["kernel"]["is_token_restricted"], tk["is_token_restricted"]
        );
        println!(
            "  EnabledPrivileges : baseline={}  ->  sandbox={}",
            baseline["kernel"]["enabled_privileges"], tk["enabled_privileges"]
        );
        println!(
            "  RestrictingSIDs   : baseline={}  ->  sandbox={}",
            baseline["kernel"]["restricting_sids"], tk["restricting_sids"]
        );
        println!(
            "  Desktop           : baseline={}  ->  sandbox={}",
            baseline["kernel"]["desktop_name"], tk["desktop_name"]
        );
        println!(
            "  InJobObject       : baseline={}  ->  sandbox={}",
            baseline["kernel"]["in_job_object"], tk["in_job_object"]
        );
        println!();
        println!("--- Raw OS Syscall Write-Protection Matrix (NO shell parser involved) ---");
        let rows = [
            ("Write authorized workspace", "write_authorized_workspace", "ALLOW"),
            ("Write OUTSIDE workspace", "write_outside_workspace", "DENY"),
            ("Write .git/hooks carveout", "write_git_hooks_carveout", "DENY"),
            ("Write .qaqh/skills carveout", "write_skills_carveout", "DENY"),
            ("Read .git/config passthrough", "read_git_config_passthrough", "ALLOW"),
            ("Grandchild .exe write outside", "grandchild_write_outside", "DENY"),
            ("Write HKCU Registry unauth", "write_registry_unauthorized", "DENY"),
            ("OpenClipboard (Job UILimit)", "clipboard_open", "DENY"),
        ];
        println!(
            "  {:<32} | {:<18} | {:<24} | {}",
            "Probe (Native Win32/FS)", "Baseline (No SBX)", "Sandboxed (TokenPlane)", "Expect"
        );
        println!("  {}", "-".repeat(88));
        for (label, key, expect) in rows {
            println!(
                "  {:<32} | {:<18} | {:<24} | {}",
                label,
                fmt_probe(&bp[key]),
                fmt_probe(&tp[key]),
                expect
            );
        }
        println!();
        println!("--- Env Scratch Isolation (Sandboxed) ---");
        println!("  TEMP        = {}", token_plane["env"]["TEMP"]);
        println!("  USERPROFILE = {}", token_plane["env"]["USERPROFILE"]);
        println!();
        println!("--- ProjFS RedirectPlane ---");
        if redirect_plane["available"] == json!(true) && redirect_plane.get("error").is_none() {
            println!(
                "  Status: AVAILABLE & VERIFIED ({}ms) | store_clean_before_merge={} | applied_after_merge={} | deletion_blocked={} (skipped={})",
                redirect_plane["duration_ms"],
                redirect_plane["store_clean_before_merge"],
                redirect_plane["store_applied_after_merge"],
                redirect_plane["deletion_blocked_on_store"],
                redirect_plane["merge_skipped_deletions"],
            );
        } else {
            println!("  Status: {}", redirect_plane);
        }
        println!();
        println!(
            "VERDICT: {}",
            if all_pass {
                "PASS — Real Windows Kernel Write Isolation Verified"
            } else {
                "FAIL — Sandbox Isolation Check Failed"
            }
        );
    }

    if all_pass { 0 } else { 1 }
}
