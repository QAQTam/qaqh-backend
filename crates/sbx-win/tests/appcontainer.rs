//! AppContainer 后端验收(ADR-0002)。
//!
//! 与 TokenPlane(spike.rs)的关键语义差异:
//! - 读也过 AC-SID DACL 检查 → readable_roots 之外的读被拒(读隔离)
//! - 空 capability = 网络内核级拒绝(此处不做联网测试,免环境抖动)
//! - 同用户令牌模拟链在 OpenProcessToken 一步即断(对比 escape_probe.rs 的背景)
//!
//! 需非提权环境(CreateAppContainerProfile 写 HKCU,免提权)。

use sbx_win::{
    acl,
    appcontainer,
    desktop,
    env,
    events,
    policy::{AceMode, AceOp, IsolationKind, NetworkPolicy, Precreate, SbxPolicy, build_ace_plan},
    spawn::{self, Isolation},
    token,
};
use std::path::PathBuf;

fn q(p: &std::path::Path) -> String {
    format!("\"{}\"", p.display())
}

#[test]
fn ac_writes_follow_policy_and_reads_are_isolated() {
    let name = format!("sbxtest{}{}", "acc", appcontainer::random_suffix());
    let profile = appcontainer::ensure_profile(&name).unwrap();
    let base = std::env::temp_dir().join(format!(
        "sbx-ac-main-{}-{}",
        std::process::id(),
        events::now_millis()
    ));
    let scratch = base.join("scratch");
    std::fs::create_dir_all(scratch.join("tmp")).unwrap();
    std::fs::create_dir_all(scratch.join("home")).unwrap();
    let desk = desktop::create_private_desktop(&profile.sid_text).unwrap();

    // 策略:scratch 可写;allowed.txt 可写;readable 子树可读;其余默认拒绝
    let ws = base.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let allowed = ws.join("fresh.txt");
    let existing = ws.join("existing.txt");
    let protected = ws.join("protected.txt");
    std::fs::write(&existing, "old").unwrap();
    std::fs::write(&protected, "old").unwrap();
    let sdk = base.join("sdk");
    std::fs::create_dir_all(&sdk).unwrap();
    let sdk_file = sdk.join("lib.txt");
    std::fs::write(&sdk_file, "sdk-content").unwrap();

    let policy = SbxPolicy {
        writable_roots: vec![],
        writable_files: vec![allowed.clone()],
        deny_write_paths: vec![],
        network: NetworkPolicy::Deny,
        isolation: IsolationKind::AppContainer,
        readable_roots: vec![sdk.clone()],
        capabilities: vec![],
        writable_registry_keys: vec![],
        redirect: false,
    };
    let mut plan = build_ace_plan(&policy, &profile.sid);
    plan.push(AceOp {
        path: scratch.clone(),
        mode: AceMode::Allow,
        sid: profile.sid.clone(),
        inherit_tree: true,
        precreate: Precreate::None,
        full_mask: false,
        kind: sbx_win::policy::AceTarget::File,
    });
    let _snaps = acl::apply_ops(&plan, &events::NullSink).unwrap();

    let lowbox = token::create_lowbox_token(&profile.sid).unwrap();
    let pairs = env::minimal_child_env(&scratch.join("tmp"), &scratch.join("home"));
    let run = |inner: String| -> spawn::Exit {
        let child = spawn::spawn(
            &Isolation::AppContainer { token: lowbox.handle() },
            &format!("\"{}\" /c {}", std::env::var("ComSpec").unwrap(), inner),
            &scratch,
            &pairs,
            Some(desk.name()),
        )
        .unwrap();
        child.wait().unwrap()
    };

    // a) scratch 可写
    let t = scratch.join("tmp").join("ok.txt");
    let e = run(format!("echo y > {}", q(&t)));
    assert!(e.code == 0 && t.exists(), "scratch 写失败 exit={} stderr={}", e.code, String::from_utf8_lossy(&e.stderr));

    // b) writable_files 授权写:目标不存在 → 模拟预创建(Low 标签)→ 可写
    let e = run(format!("echo x >> {}", q(&allowed)));
    assert!(e.code == 0, "授权文件(模拟预创建)写失败 exit={} stderr={}", e.code, String::from_utf8_lossy(&e.stderr));
    assert!(std::fs::read_to_string(&allowed).unwrap().contains("x"));

    // b2) 已存在文件:v0.2 限制 —— Low 标签 unelevated 不可写,改写被拒(M2 overlay 解)
    let e = run(format!("echo x >> {}", q(&existing)));
    assert!(e.code != 0, "已存在文件的改写竟成功(标签机制失效?)");
    assert_eq!(std::fs::read_to_string(&existing).unwrap(), "old");

    // c) 未授权写 → 拒(标准检查过(user),AC 检查无 ACE → 拒)
    let e = run(format!("echo x > {}", q(&ws.join("other.txt"))));
    assert!(e.code != 0, "未授权写竟成功");
    assert!(!ws.join("other.txt").exists());

    // d) 读隔离:readable_roots 之外的读被拒(与 TokenPlane 的关键差异)
    let e = run(format!("type {}", q(&protected)));
    assert!(e.code != 0, "readable_roots 之外的读竟成功 = 读隔离失效");
    assert_eq!(std::fs::read_to_string(&protected).unwrap(), "old");

    // e) readable_roots 内的读放行
    let e = run(format!("type {}", q(&sdk_file)));
    assert!(
        e.code == 0 && String::from_utf8_lossy(&e.stdout).contains("sdk-content"),
        "readable_roots 读失败 exit={} stdout={}",
        e.code,
        String::from_utf8_lossy(&e.stdout)
    );

    let _ = appcontainer::delete_profile(&name);
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn ac_impersonation_chain_breaks_at_token_open() {
    // 决定性验证(ADR-0002 §实证):AppContainer 下,探针的
    // OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION) 对同用户进程即失败
    // (进程/令牌对象 DACL 无 AC-SID ACE)→ 拿不到任何可模拟令牌 → 链断。
    // 这是 escape_probe.rs 在 TokenPlane 上"靠环境防御"做不到的结构保证。
    let name = format!("sbxtest{}{}", "esc", appcontainer::random_suffix());
    let profile = appcontainer::ensure_profile(&name).unwrap();
    let base = std::env::temp_dir().join(format!(
        "sbx-ac-esc-{}-{}",
        std::process::id(),
        events::now_millis()
    ));
    let scratch = base.join("scratch");
    std::fs::create_dir_all(scratch.join("tmp")).unwrap();
    std::fs::create_dir_all(scratch.join("home")).unwrap();
    let desk = desktop::create_private_desktop(&profile.sid_text).unwrap();
    let lowbox = token::create_lowbox_token(&profile.sid).unwrap();
    let plan = [AceOp {
        path: scratch.clone(),
        mode: AceMode::Allow,
        sid: profile.sid.clone(),
        inherit_tree: true,
        precreate: Precreate::None,
        full_mask: false,
        kind: sbx_win::policy::AceTarget::File,
    }];
    let _ = acl::apply_ops(&plan, &events::NullSink).unwrap();

    // 编译探针(与 escape_probe 共用源)
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/probe-src/probe.rs");
    let probe_exe = base.join("sbx-escape-probe.exe");
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let compiled = std::process::Command::new(&rustc)
        .args(["-O"])
        .arg(&src)
        .args(["-o"])
        .arg(&probe_exe)
        .output()
        .expect("spawn rustc");
    assert!(compiled.status.success(), "probe compile failed: {}", String::from_utf8_lossy(&compiled.stderr));

    let c1 = base.join("canary1.txt");
    let c2 = base.join("canary2.txt");
    let c3 = base.join("canary3.txt");
    let pairs = env::minimal_child_env(&scratch.join("tmp"), &scratch.join("home"));
    let cmdline = format!(
        "\"{}\" \"{}\" \"{}\" \"{}\" explorer.exe",
        probe_exe.display(),
        c1.display(),
        c2.display(),
        c3.display()
    );
    let child = spawn::spawn(
        &Isolation::AppContainer { token: lowbox.handle() },
        &cmdline,
        &scratch,
        &pairs,
        Some(desk.name()),
    )
    .unwrap();
    let exit = child.wait().unwrap();
    eprintln!("probe exit: {}", exit.code);
    eprintln!("probe stdout:\n{}", String::from_utf8_lossy(&exit.stdout));

    // 硬断言:canary 零落盘;链路断点允许在拿令牌(理想)或写仍被拒(实际:
    // 沙箱内几乎看不到任何同用户进程,探针只能模拟到自己的进程条目)
    let out = String::from_utf8_lossy(&exit.stdout);
    assert!(
        out.contains("no duplicateable same-user token")
            || out.contains("write still blocked")
            || out.contains("RESULT: FAIL"),
        "探针既未断链也未报阻断,实际输出:{out}"
    );
    assert!(!c1.exists() && !c2.exists() && !c3.exists(), "canary 落盘 = 逃逸");

    let _ = appcontainer::delete_profile(&name);
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn lowbox_token_carries_granted_capabilities() {
    // capabilities 策略化的内核级断言:授权的能力 SID 进入 TokenCapabilities
    // (注意:capability 不在 TokenGroups 里),空 capability 令牌必须没有网络
    // 能力(断网的令牌级证据,ADR-0002)。
    let name = format!("sbxtest{}{}", "cap", appcontainer::random_suffix());
    let profile = appcontainer::ensure_profile(&name).unwrap();
    let ic = appcontainer::capability_sid_from_name("internetClient").unwrap();

    let with_cap =
        token::create_lowbox_token_with_capabilities(&profile.sid, &[ic.clone()]).unwrap();
    let caps = token::token_capabilities(with_cap.handle()).unwrap();
    assert!(
        caps.iter().any(|(sid, _)| *sid == ic),
        "授权的 internetClient(S-1-15-3-1)不在 TokenCapabilities 中"
    );
    assert!(
        caps.iter().any(|(sid, attr)| *sid == ic && attr & 0x4 != 0),
        "internetClient 能力未处于 ENABLED 状态"
    );

    let plain = token::create_lowbox_token(&profile.sid).unwrap();
    let plain_caps = token::token_capabilities(plain.handle()).unwrap();
    assert!(
        !plain_caps.iter().any(|(sid, _)| *sid == ic),
        "空 capability 令牌竟含 internetClient = 内核断网失效"
    );
    // 包 SID 不进 TokenGroups/TokenCapabilities,走 TokenAppContainerSid 查询
    let pkg = token::token_app_container_sid(plain.handle()).unwrap();
    assert_eq!(
        pkg, profile.sid,
        "TokenAppContainerSid 与 profile SID 不一致 = AC 检查锚点缺失"
    );

    let _ = appcontainer::delete_profile(&name);
}
