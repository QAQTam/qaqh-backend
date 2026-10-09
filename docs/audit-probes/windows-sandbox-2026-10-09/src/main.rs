//! Diagnostic only: uses uniquely named profiles and temporary canaries.
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
};
use windows::Win32::Security::{FreeSid, SECURITY_CAPABILITIES};
use windows::Win32::System::Threading::*;
use windows::core::{PCWSTR, PWSTR};

unsafe fn standard_launch(sid: windows::Win32::Security::PSID) -> String {
    unsafe {
        let mut size = 0;
        let _ = InitializeProcThreadAttributeList(
            LPPROC_THREAD_ATTRIBUTE_LIST::default(),
            1,
            0,
            &mut size,
        );
        let mut buf = vec![0usize; size.div_ceil(std::mem::size_of::<usize>())];
        let attrs = LPPROC_THREAD_ATTRIBUTE_LIST(buf.as_mut_ptr().cast());
        InitializeProcThreadAttributeList(attrs, 1, 0, &mut size).unwrap();
        let caps = SECURITY_CAPABILITIES {
            AppContainerSid: sid,
            Capabilities: std::ptr::null_mut(),
            CapabilityCount: 0,
            Reserved: 0,
        };
        UpdateProcThreadAttribute(
            attrs,
            0,
            PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
            Some((&caps as *const SECURITY_CAPABILITIES).cast()),
            std::mem::size_of::<SECURITY_CAPABILITIES>(),
            None,
            None,
        )
        .unwrap();
        let mut si = STARTUPINFOEXW::default();
        si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        si.lpAttributeList = attrs;
        let exe: Vec<u16> = std::env::var("ComSpec")
            .unwrap()
            .encode_utf16()
            .chain([0])
            .collect();
        let mut cmd: Vec<u16> = "cmd.exe /d /c exit 0".encode_utf16().chain([0]).collect();
        let cwd: Vec<u16> = format!("{}\\System32", std::env::var("SystemRoot").unwrap())
            .encode_utf16()
            .chain([0])
            .collect();
        let mut pi = PROCESS_INFORMATION::default();
        let r = CreateProcessW(
            PCWSTR(exe.as_ptr()),
            PWSTR(cmd.as_mut_ptr()),
            None,
            None,
            false,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_NO_WINDOW,
            None,
            PCWSTR(cwd.as_ptr()),
            (&si as *const STARTUPINFOEXW).cast(),
            &mut pi,
        );
        DeleteProcThreadAttributeList(attrs);
        match r {
            Err(e) => format!("ERR 0x{:08X} {e}", e.code().0 as u32),
            Ok(()) => {
                let wait = WaitForSingleObject(pi.hProcess, 5000);
                let mut code = 0;
                let _ = GetExitCodeProcess(pi.hProcess, &mut code);
                if wait != windows::Win32::Foundation::WAIT_OBJECT_0 {
                    let _ = TerminateProcess(pi.hProcess, 1);
                }
                let _ = CloseHandle(pi.hThread);
                let _ = CloseHandle(pi.hProcess);
                format!("OK exit={code} wait={wait:?}")
            }
        }
    }
}

fn main() {
    println!("elevated={}", sbx_win::token::is_elevated());
    let name = format!("sbxaudit{}", sbx_win::appcontainer::random_suffix());
    let nw: Vec<u16> = name.encode_utf16().chain([0]).collect();
    let derived =
        unsafe { DeriveAppContainerSidFromAppContainerName(PCWSTR(nw.as_ptr())) }.unwrap();
    println!("derive nonexistent name: OK");
    println!("standard before profile: {}", unsafe {
        standard_launch(derived)
    });
    let profile = sbx_win::appcontainer::ensure_profile(&name).unwrap();
    println!("ensure_profile returned {}", profile.sid_text);
    println!("standard after ensure_profile: {}", unsafe {
        standard_launch(derived)
    });
    let created = unsafe {
        CreateAppContainerProfile(
            PCWSTR(nw.as_ptr()),
            PCWSTR(nw.as_ptr()),
            PCWSTR(nw.as_ptr()),
            None,
        )
    };
    println!(
        "actual CreateAppContainerProfile: {}",
        if created.is_ok() {
            "OK (ensure_profile had NOT created it)"
        } else {
            "ERR"
        }
    );
    println!("standard after actual create: {}", unsafe {
        standard_launch(derived)
    });
    unsafe {
        if let Ok(sid) = created {
            FreeSid(sid);
        }
        FreeSid(derived);
    }
    let ic = sbx_win::appcontainer::capability_sid_from_name("internetClient").unwrap();
    let ipc =
        sbx_win::appcontainer::capability_sid_from_name("privateNetworkClientServer").unwrap();
    let tok =
        sbx_win::token::create_lowbox_token_with_capabilities(&profile.sid, &[ic, ipc]).unwrap();
    unsafe {
        let mut len = 0;
        let _ = windows::Win32::Security::GetTokenInformation(
            tok.handle(),
            windows::Win32::Security::TokenCapabilities,
            None,
            0,
            &mut len,
        );
        let mut data = vec![0usize; (len as usize).div_ceil(std::mem::size_of::<usize>())];
        windows::Win32::Security::GetTokenInformation(
            tok.handle(),
            windows::Win32::Security::TokenCapabilities,
            Some(data.as_mut_ptr().cast()),
            len,
            &mut len,
        )
        .unwrap();
        let native = &*(data.as_ptr() as *const windows::Win32::Security::TOKEN_GROUPS);
        println!(
            "capabilities requested=2 native_count={} helper_count={}",
            native.GroupCount,
            sbx_win::token::token_capabilities(tok.handle())
                .unwrap()
                .len()
        );
    }
    drop(tok);
    let _ = sbx_win::appcontainer::delete_profile(&name);
    exec_probe();
    inherited_acl_probe();
}

fn exec_probe() {
    use qaqh_policy::SandboxBackend;
    use qaqh_process_tools::exec::handler::{ExecArgs, ExecTool};
    use qaqh_process_tools::tool_api::*;
    let base = std::env::temp_dir().join(format!(
        "qaqh-audit-{}-{}",
        std::process::id(),
        sbx_win::appcontainer::random_suffix()
    ));
    let ws = base.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    for backend in [
        SandboxBackend::Auto,
        SandboxBackend::WindowsToken,
        SandboxBackend::WindowsRedirect,
    ] {
        let outside = base.join(format!("{backend:?}.txt"));
        let mut spec = SandboxSpec::workspace_write(ws.clone());
        spec.backend = backend;
        let ctx = ToolCallContext {
            call_id: "audit".into(),
            session_id: "audit".into(),
            workspace_root: ws.clone(),
            mode: AgentMode::Code,
            permission_level: qaqh_process_tools::permission::PermissionLevel::SkipPermissions,
            sandbox: SandboxMode::Main,
            sandbox_spec: spec,
            exec_default_shell: Some("cmd".into()),
            timeout: std::time::Duration::from_secs(10),
            cancellation: CancellationToken::new(),
            progress: None,
            source: ToolCallSource::Model,
        };
        let args: ExecArgs = serde_json::from_value(serde_json::json!({"command": format!("echo audit > {}", outside.display()), "shell": "cmd", "timeout_secs": 10})).unwrap();
        let result = ExecTool.run(&ctx, args);
        match result {
            Ok(out) => {
                let value = serde_json::to_value(out).unwrap();
                println!(
                    "exec backend={backend:?} outside_written={} exit={} process_id={} output={:?}",
                    outside.exists(),
                    value["exit_code"],
                    value["process_id"],
                    value["output"]
                        .as_str()
                        .unwrap_or_default()
                        .chars()
                        .take(120)
                        .collect::<String>()
                );
            }
            Err(err) => println!(
                "exec backend={backend:?} outside_written={} error={err:?}",
                outside.exists()
            ),
        }
    }
    // All targets are under this uniquely named temporary directory.
    std::fs::remove_dir_all(base).unwrap();
}

fn inherited_acl_probe() {
    let base = std::env::temp_dir().join(format!(
        "qaqh-acl-audit-{}-{}",
        std::process::id(),
        sbx_win::appcontainer::random_suffix()
    ));
    let store = base.join("store");
    let scratch = base.join("scratch");
    let root = scratch.join("view");
    std::fs::create_dir_all(&store).unwrap();
    std::fs::create_dir_all(&scratch).unwrap();
    let cap = sbx_win::sid::capability_sid().unwrap();
    sbx_win::acl::add_ace(&scratch, &cap, sbx_win::policy::AceMode::Allow, true).unwrap();
    let turn = sbx_win::redirect::prepare_turn(&store, &root, &[]).unwrap();
    let token =
        sbx_win::token::create_restricted_token(&[cap.clone(), sbx_win::sid::everyone_sid()])
            .unwrap();
    let desktop =
        sbx_win::desktop::create_private_desktop(&sbx_win::sid::sid_to_string(&cap)).unwrap();
    let env = sbx_win::env::minimal_child_env(&scratch, &scratch);
    let child = sbx_win::spawn::spawn(
        &sbx_win::spawn::Isolation::RestrictedToken {
            token: token.handle(),
        },
        &format!(
            "\"{}\" /d /c echo audit > unauthorized.txt",
            std::env::var("ComSpec").unwrap()
        ),
        &root,
        &env,
        Some(desktop.name()),
    )
    .unwrap();
    let exit = child.wait().unwrap();
    println!(
        "scratch inherited cap + protected view: unauthorized_written={} exit={} stderr={}",
        root.join("unauthorized.txt").exists(),
        exit.code,
        String::from_utf8_lossy(&exit.stderr)
    );
    let verbose = "for /L %i in (1,1,2000) do @echo 012345678901234567890123456789012345678901234567890123456789";
    let started = std::time::Instant::now();
    let baseline = std::process::Command::new(std::env::var("ComSpec").unwrap())
        .args(["/d", "/c", verbose])
        .output()
        .unwrap();
    println!(
        "verbose baseline exit={} bytes={} elapsed_ms={}",
        baseline.status.code().unwrap_or(-1),
        baseline.stdout.len(),
        started.elapsed().as_millis()
    );
    let mut child = sbx_win::spawn::spawn(
        &sbx_win::spawn::Isolation::RestrictedToken {
            token: token.handle(),
        },
        &format!("\"{}\" /d /c {verbose}", std::env::var("ComSpec").unwrap()),
        &root,
        &env,
        Some(desktop.name()),
    )
    .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    while child.try_wait().unwrap().is_none() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let stalled = child.try_wait().unwrap().is_none();
    if stalled {
        child.kill_tree();
    }
    let exit = child.wait().unwrap();
    println!(
        "verbose sbx stalled_before_wait={stalled} captured_bytes={} exit={}",
        exit.stdout.len(),
        exit.code
    );
    turn.discard().unwrap();
    drop(turn);
    drop(token);
    drop(desktop);
    let _ = std::fs::remove_dir(&root);
    if let Err(err) = std::fs::remove_dir_all(&base) {
        println!("cleanup note: {}: {err}", base.display());
    }
}
