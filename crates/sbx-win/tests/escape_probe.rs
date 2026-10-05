//! 逃逸警报(ignored,单独运行):同用户令牌模拟链在沙箱内必须不能产出未授权写入。
//!
//! 背景(2026-10-04 评审实证):WRITE_RESTRICTED 令牌只过滤写类访问,
//! OpenProcess / OpenProcessToken(TOKEN_DUPLICATE) / DuplicateTokenEx 全链
//! 在沙箱内畅通;终局 SetThreadToken(外部完整令牌) 在本机被环境防御
//! (疑似 Defender 行为引擎)击杀 —— 不是本沙箱的防线。环境防御缺席的机器
//! (裸 CI / 被做掉杀软的终端)上,canary 落盘 = 逃逸成立。
//!
//! 通过条件(任一,硬断言都是 canary 不存在):
//!   a) 探针被环境防御终止(退出码非零) —— 环境防御在位
//!   b) 探针走完但 canary 全部未出现 —— 沙箱/令牌/环境拒绝了该链
//!   c) canary3(负对照)落盘 = DACL 平面本身失效 —— 另一条硬断言,直接失败
//!
//! 失败 = 令牌模拟逃逸成立 —— **立即处置**(参见 ADR-0002:AppContainer 提前)。
//! 运行:cargo test -p sbx-win --test escape_probe -- --ignored --nocapture

use sbx_win::{acl, desktop, env, events, sid, spawn, token};
use std::path::PathBuf;

#[test]
#[ignore = "逃逸警报:环境防御缺席的机器上设计为失败,单独运行(CI escape-alarm job)"]
fn escape_probe_must_not_write_outside_writable_roots() {
    let base = std::env::temp_dir().join(format!(
        "sbx-escape-probe-{}-{}",
        std::process::id(),
        events::now_millis()
    ));
    std::fs::create_dir_all(base.join("scratch").join("tmp")).unwrap();
    std::fs::create_dir_all(base.join("scratch").join("home")).unwrap();

    // 编译探针(cargo test 环境内 rustc 必在 PATH)
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
    assert!(
        compiled.status.success(),
        "probe compile failed: {}",
        String::from_utf8_lossy(&compiled.stderr)
    );

    // 沙箱环境:与 hardening::setup 同构(scratch 带 cap ACE,私有桌面,受限令牌)
    let sid = sid::capability_sid().unwrap();
    acl::add_ace(&base.join("scratch"), &sid, sbx_win::policy::AceMode::Allow, true).unwrap();
    let tok = token::create_restricted_token(&[sid.clone(), sid::everyone_sid()]).unwrap();
    let desk = desktop::create_private_desktop(&sid::sid_to_string(&sid)).unwrap();

    // canary 全部放在无 cap ACE 的 base 下 —— 任何落盘都是逃逸
    let c1 = base.join("canary-impersonated-write.txt");
    let c2 = base.join("canary-full-token-child.txt");
    let c3 = base.join("canary-negative-control.txt");

    let pairs = env::minimal_child_env(&base.join("scratch/tmp"), &base.join("scratch/home"));
    let cmdline = format!(
        "\"{}\" \"{}\" \"{}\" \"{}\" explorer.exe",
        probe_exe.display(),
        c1.display(),
        c2.display(),
        c3.display(),
    );
    let child = spawn::spawn(&spawn::Isolation::RestrictedToken { token: tok.handle() }, &cmdline, &base.join("scratch"), &pairs, Some(desk.name()))
        .expect("spawn probe");
    let exit = child.wait().unwrap();

    eprintln!("probe exit code: {} (0x{:08X})", exit.code, exit.code);
    eprintln!("probe stdout:\n{}", String::from_utf8_lossy(&exit.stdout));
    eprintln!("probe stderr:\n{}", String::from_utf8_lossy(&exit.stderr));

    // 硬断言 1:负对照 —— 沙箱未模拟时,直写 canary3 必须被 DACL 拒
    assert!(
        !c3.exists(),
        "ESCAPE ALARM(平面失效): 未模拟状态下 canary3 落盘,DACL 写拒绝平面本身失效"
    );
    // 硬断言 2:模拟写 canary1 / 完整令牌子进程写 canary2 必须不发生
    assert!(
        !c1.exists() && !c2.exists(),
        "ESCAPE ALARM: 令牌模拟逃逸成立(canary1={}, canary2={}) —— 环境防御缺席,需立即推进 AppContainer(ADR-0002)",
        c1.exists(),
        c2.exists(),
    );

    let _ = std::fs::remove_dir_all(&base);
}
