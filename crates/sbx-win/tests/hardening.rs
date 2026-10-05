//! 加固验证:幂等无堆积、ACE 撤销、Job 杀树(含孙进程)、8.3 短名绕过免疫。

use sbx_win::{acl, desktop, events, policy::AceOp, policy::AceTarget, policy::Precreate, policy::AceMode, sid, spawn, token};
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::time::Duration;

struct Env {
    sid: Vec<u8>,
    token: token::Token,
    desktop: desktop::Desktop,
    cwd: PathBuf,
}

/// 每个用例独立的临时沙箱环境(scratch 带 cap ACE,私有桌面,受限令牌)。
fn setup(tag: &str) -> (PathBuf, Env) {
    let base = std::env::temp_dir().join(format!(
        "sbx-hard-{tag}-{}-{}",
        std::process::id(),
        sbx_win::events::now_millis()
    ));
    let scratch = base.join("scratch");
    std::fs::create_dir_all(scratch.join("tmp")).unwrap();
    std::fs::create_dir_all(scratch.join("home")).unwrap();
    let sid = sid::capability_sid().unwrap();
    acl::add_ace(&scratch, &sid, AceMode::Allow, true).unwrap();
    let tok = token::create_restricted_token(&[sid.clone(), sid::everyone_sid()]).unwrap();
    let desk = desktop::create_private_desktop(&sid::sid_to_string(&sid)).unwrap();
    (
        base,
        Env {
            sid,
            token: tok,
            desktop: desk,
            cwd: scratch.clone(),
        },
    )
}

fn run(env: &Env, inner: &str) -> sbx_win::spawn::Exit {
    let scratch_tmp = env.cwd.join("tmp");
    let scratch_home = env.cwd.join("home");
    let pairs = sbx_win::env::minimal_child_env(&scratch_tmp, &scratch_home);
    let child = spawn::spawn(
        &spawn::Isolation::RestrictedToken { token: env.token.handle() },
        &format!("\"{}\" /c {}", std::env::var("ComSpec").unwrap(), inner),
        &env.cwd,
        &pairs,
        Some(env.desktop.name()),
    )
    .unwrap();
    child.wait().unwrap()
}

fn run_no_wait(env: &Env, inner: &str) -> sbx_win::spawn::Child {
    let scratch_tmp = env.cwd.join("tmp");
    let scratch_home = env.cwd.join("home");
    let pairs = sbx_win::env::minimal_child_env(&scratch_tmp, &scratch_home);
    spawn::spawn(
        &spawn::Isolation::RestrictedToken { token: env.token.handle() },
        &format!("\"{}\" /c {}", std::env::var("ComSpec").unwrap(), inner),
        &env.cwd,
        &pairs,
        Some(env.desktop.name()),
    )
    .unwrap()
}

#[test]
fn ace_apply_is_idempotent_and_revocable() {
    let (base, env) = setup("acl");
    let f = base.join("probe.txt");
    std::fs::write(&f, "x").unwrap();
    assert!(acl::add_ace(&f, &env.sid, AceMode::Allow, false).unwrap());
    assert!(!acl::add_ace(&f, &env.sid, AceMode::Allow, false).unwrap(), "第二次应为跳过");
    assert_eq!(acl::count_aces(&f, &env.sid, AceMode::Allow).unwrap(), 1, "不允许 ACE 堆积");
    assert!(acl::revoke_ace(&f, &env.sid).unwrap());
    assert_eq!(acl::count_aces(&f, &env.sid, AceMode::Allow).unwrap(), 0);
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn kill_tree_terminates_child() {
    let (base, env) = setup("kill");
    let mut child = run_no_wait(&env, "ping -n 30 127.0.0.1 > NUL");
    std::thread::sleep(Duration::from_millis(300));
    let start = std::time::Instant::now();
    child.kill_tree();
    // 收割应在数秒内返回(进程被 Job 终结),而不是等 30 秒。
    // Job 终结的退出码不保证非零(实测可为 0),只断言时限。
    let exit = child.wait().unwrap();
    assert!(start.elapsed() < Duration::from_secs(5), "kill_tree 后应立即退出");
    let _ = exit;
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn grandchild_in_job_dies_with_tree() {
    let (base, env) = setup("tree");
    let sentinel = base.join("sentinel.txt");
    let inner = format!(
        "start /b cmd /c \"ping -n 2 127.0.0.1 > NUL & echo done > {}\"",
        sentinel.display()
    );
    let mut child = run_no_wait(&env, &inner);
    std::thread::sleep(Duration::from_millis(500));
    child.kill_tree();
    // 给孙进程留足完成时间:若它逃出 Job,2 秒后哨兵必然出现
    std::thread::sleep(Duration::from_millis(2600));
    assert!(
        !sentinel.exists(),
        "孙进程在 kill_tree 后仍完成了写入 = 逃出了 Job(传播失败)"
    );
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn short_name_path_still_denied() {
    let (base, env) = setup("shortname");
    let ws = base.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let protected = ws.join("protected.txt");
    std::fs::write(&protected, "keep").unwrap();
    // 长路径无 cap ACE;8.3 短名指向同一文件对象,内核按对象 DACL 拒——形态免疫
    let mut buf = [0u16; 520];
    let wide: Vec<u16> = protected.as_os_str().encode_wide().chain([0]).collect();
    let n = unsafe {
        windows::Win32::Storage::FileSystem::GetShortPathNameW(
            windows::core::PCWSTR(wide.as_ptr()),
            Some(&mut buf),
        )
    };
    if n > 0 && n < 520 {
        let short = PathBuf::from(String::from_utf16_lossy(&buf[..n as usize]));
        if short != protected {
            let e = run(&env, &format!("echo hacked > {}", short.display()));
            let content = std::fs::read_to_string(&protected).unwrap();
            assert_ne!(e.code, 0, "经 8.3 短名写入不应绕过 DACL");
            assert_eq!(content, "keep");
        }
    }
    // 8.3 未启用的卷:跳过(内核按对象检查的属性与形态无关,不影响结论)
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn writes_via_restricted_token_use_the_policy() {
    let (base, env) = setup("policy");
    let ws = base.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let allowed = ws.join("a.txt");
    std::fs::write(&allowed, "old").unwrap();
    acl::add_ace(&allowed, &env.sid, AceMode::Allow, false).unwrap();
    let ok = run(&env, &format!("echo x >> {}", allowed.display()));
    assert_eq!(ok.code, 0);
    let denied = run(&env, &format!("echo x > {}", ws.join("other.txt").display()));
    assert_ne!(denied.code, 0);
    assert!(!ws.join("other.txt").exists(), "未授权新建不应发生");
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn scratch_layout_matches_m2_store_contract() {
    // §4.3:upper/tmp/home 布局是两个平面共享的 store 契约,M1 就要一次定死
    let (base, _env) = setup("layout");
    for d in ["tmp", "home"] {
        assert!(base.join("scratch").join(d).is_dir(), "{d} 目录缺失");
    }
    let _ = std::fs::remove_dir_all(&base);
}

/// junction 目标目录(免特权创建,任意环境可复现)
fn make_junction(link: &std::path::Path, target: &std::path::Path) -> bool {
    std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()
        .map(|o| o.status.success() && link.exists())
        .unwrap_or(false)
}

#[test]
fn junction_intermediate_component_is_rejected_before_ace() {
    let (base, env) = setup("junction");
    let ws = base.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    // 受害目录在 ws 外,无 cap ACE:攻击者把 junction 埋进 workspace,
    // 诱导父进程(完整令牌)把 ACE 加到 junction 目标上
    let victim_dir = base.join("victim");
    std::fs::create_dir_all(&victim_dir).unwrap();
    let victim_file = victim_dir.join("note.txt");
    std::fs::write(&victim_file, "SECRET").unwrap();

    let link = ws.join("link");
    if !make_junction(&link, &victim_dir) {
        eprintln!("skip: junction 创建失败");
        let _ = std::fs::remove_dir_all(&base);
        return;
    }

    let op = AceOp {
        path: link.join("note.txt"),
        mode: AceMode::Allow,
        sid: env.sid.clone(),
        inherit_tree: false,
        precreate: Precreate::File,
        full_mask: false,
        kind: AceTarget::File,
    };
    let applied = acl::apply_ops(&[op], &events::NullSink);
    assert!(applied.is_err(), "穿透 junction 的路径必须被拒绝");
    // 受害对象上绝不能出现 cap ACE,内容也不能被动过
    assert_eq!(acl::count_aces(&victim_dir, &env.sid, AceMode::Allow).unwrap(), 0);
    assert_eq!(acl::count_aces(&victim_file, &env.sid, AceMode::Allow).unwrap(), 0);
    assert_eq!(
        std::fs::read_to_string(&victim_file).unwrap(),
        "SECRET",
        "受害文件内容不得变化(precreate/ACE 均不得穿透 junction)"
    );
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn symlink_final_component_is_rejected_before_ace() {
    let (base, env) = setup("symlink");
    let ws = base.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let victim_dir = base.join("victim");
    std::fs::create_dir_all(&victim_dir).unwrap();
    let victim = victim_dir.join("authorized_keys");
    std::fs::write(&victim, "SECRET").unwrap();

    let planted = ws.join("config.toml");
    // 符号链接创建需要 SeCreateSymbolicLinkPrivilege/开发者模式;失败则跳过
    if std::os::windows::fs::symlink_file(&victim, &planted).is_err() {
        eprintln!("skip: symlink 创建失败(无特权且未开开发者模式)");
        let _ = std::fs::remove_dir_all(&base);
        return;
    }

    let op = AceOp {
        path: planted.clone(),
        mode: AceMode::Allow,
        sid: env.sid.clone(),
        inherit_tree: false,
        precreate: Precreate::None,
        full_mask: false,
        kind: AceTarget::File,
    };
    let applied = acl::apply_ops(&[op], &events::NullSink);
    assert!(applied.is_err(), "reparse point 目标必须被拒绝");
    assert_eq!(acl::count_aces(&victim, &env.sid, AceMode::Allow).unwrap(), 0);
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn sddl_snapshot_roundtrip_restores_dacl() {
    let (base, env) = setup("sddl");
    let f = base.join("f.txt");
    std::fs::write(&f, "x").unwrap();

    let before = acl::dacl_sddl(&f).unwrap();
    assert!(before.is_some(), "普通文件应有 DACL 可快照");

    let op = AceOp {
        path: f.clone(),
        mode: AceMode::Allow,
        sid: env.sid.clone(),
        inherit_tree: false,
        precreate: Precreate::None,
        full_mask: false,
        kind: AceTarget::File,
    };
    let snaps = acl::apply_ops(&[op.clone()], &events::NullSink).unwrap();
    assert_eq!(snaps.len(), 1);
    assert!(snaps[0].is_some(), "apply 应返回 apply 前的 SDDL 快照");
    assert_eq!(acl::count_aces(&f, &env.sid, AceMode::Allow).unwrap(), 1);

    // cleanup 语义:按快照整树恢复 —— cap ACE 消失,DACL 回到原始形态
    acl::restore_dacl_from_sddl(&f, snaps[0].as_ref().unwrap()).unwrap();
    assert_eq!(acl::count_aces(&f, &env.sid, AceMode::Allow).unwrap(), 0);
    assert_eq!(acl::dacl_sddl(&f).unwrap(), before, "恢复后 DACL 应与快照前一致");

    // 幂等复跑:apply(重新加 ACE)→ restore,再次回到原始形态
    let snaps2 = acl::apply_ops(std::slice::from_ref(&op), &events::NullSink).unwrap();
    acl::restore_dacl_from_sddl(&f, snaps2[0].as_ref().unwrap()).unwrap();
    assert_eq!(acl::count_aces(&f, &env.sid, AceMode::Allow).unwrap(), 0);
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn doctor_scan_finds_everyone_writable_paths() {
    let (base, _env) = setup("doctorscan");
    let clean = base.join("clean");
    let dirty = base.join("dirty");
    std::fs::create_dir_all(&clean).unwrap();
    std::fs::create_dir_all(&dirty).unwrap();
    // BUILTIN\Users 写 ACE(共享目录的典型形态;add_ace 接受任意 SID)
    let users = sid::parse_sid("S-1-5-32-545").unwrap();
    acl::add_ace(&dirty, &users, AceMode::Allow, true).unwrap();

    assert!(
        acl::wellknown_write_grants(&clean).unwrap().is_empty(),
        "用户私有目录不应有 well-known 写授权"
    );
    let grants = acl::wellknown_write_grants(&dirty).unwrap();
    assert!(grants.contains(&"BUILTIN\\Users"), "{grants:?}");

    let findings = acl::scan_everyone_writable(&base, 4, 1000).unwrap();
    assert!(findings.contains(&dirty), "应定位到 dirty:{findings:?}");
    assert!(!findings.contains(&clean), "clean 不应误报:{findings:?}");
    let _ = std::fs::remove_dir_all(&base);
}
