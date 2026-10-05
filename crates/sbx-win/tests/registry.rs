//! 注册表写授权验证(writable_registry_keys;ADR-0002:cap-ACE 机制推广到
//! SE_REGISTRY_OBJECT,未授权注册表写本就 fail-closed)。
//!
//! 断言链:apply 预创建授权键 + 挂 cap-ACE → 受限令牌 reg add 成功 →
//! 未授权键(同父新键)被拒 → 按 SDDL 快照恢复 → 授权操作也被拒(授权即撤销)。

use sbx_win::{
    acl,
    desktop,
    env,
    events,
    policy::{self, AceMode, AceTarget, IsolationKind, NetworkPolicy, Precreate, SbxPolicy, build_ace_plan},
    sid,
    spawn::{self, Isolation},
    token,
};
use std::path::Path;

fn unique_key(tag: &str) -> String {
    format!(
        "HKCU\\Software\\sbx-regtest-{}-{}-{}",
        tag,
        std::process::id(),
        events::now_millis()
    )
}

fn reg_path(key: &str) -> &Path {
    Path::new(key)
}

#[test]
fn registry_writes_follow_policy_and_cleanup_restores() {
    let key = unique_key("main");
    let base = std::env::temp_dir().join(format!(
        "sbx-reg-{}-{}",
        std::process::id(),
        events::now_millis()
    ));
    let scratch = base.join("scratch");
    std::fs::create_dir_all(scratch.join("tmp")).unwrap();
    std::fs::create_dir_all(scratch.join("home")).unwrap();

    let cap = sid::capability_sid().unwrap();
    let policy = SbxPolicy {
        writable_roots: vec![],
        writable_files: vec![],
        deny_write_paths: vec![],
        network: NetworkPolicy::Deny,
        isolation: IsolationKind::Token,
        readable_roots: vec![],
        capabilities: vec![],
        writable_registry_keys: vec![key.clone()],
        redirect: false,
    };
    let mut plan = build_ace_plan(&policy, &cap);
    plan.push(policy::AceOp {
        path: scratch.clone(),
        mode: AceMode::Allow,
        sid: cap.clone(),
        inherit_tree: true,
        precreate: Precreate::None,
        full_mask: false,
        kind: AceTarget::File,
    });
    let snaps = acl::apply_ops(&plan, &events::NullSink).unwrap();
    assert_eq!(snaps.len(), plan.len());
    assert!(snaps[0].is_some(), "注册表键应有可快照 DACL(apply 已预创建)");

    // 授权键上 cap-ACE 就位(幂等复查也走同一探测路径)
    assert!(
        acl::has_ace_for(reg_path(&key), AceTarget::Registry, &cap, AceMode::Allow).unwrap(),
        "注册表键未见 cap-ACE"
    );

    let tok = token::create_restricted_token(&[cap.clone(), sid::everyone_sid()]).unwrap();
    let desk = desktop::create_private_desktop(&sid::sid_to_string(&cap)).unwrap();
    let pairs = env::minimal_child_env(&scratch.join("tmp"), &scratch.join("home"));
    let run = |inner: String| -> spawn::Exit {
        let child = spawn::spawn(
            &Isolation::RestrictedToken { token: tok.handle() },
            &format!("\"{}\" /c {}", std::env::var("ComSpec").unwrap(), inner),
            &scratch,
            &pairs,
            Some(desk.name()),
        )
        .unwrap();
        child.wait().unwrap()
    };

    // 授权写:reg add 成功(reg.exe 以受限令牌运行,KEY_SET_VALUE 落在授权键)
    let e = run(format!("reg add \"{}\" /v probe /t REG_SZ /d hello /f", key));
    assert!(
        e.code == 0,
        "授权注册表写失败 exit={} stderr={}",
        e.code,
        String::from_utf8_lossy(&e.stderr)
    );

    // 未授权写:同父下的新键(reg add 需在 HKCU\Software 下建子键)→ 拒
    let other = unique_key("other");
    let e = run(format!("reg add \"{}\" /v probe /t REG_SZ /d x /f", other));
    assert!(e.code != 0, "未授权注册表写竟成功");

    // cleanup 语义:按 SDDL 快照恢复 → cap ACE 消失,此后连授权操作也被拒
    acl::restore_sddl_for(AceTarget::Registry, reg_path(&key), snaps[0].as_ref().unwrap()).unwrap();
    assert!(
        !acl::has_ace_for(reg_path(&key), AceTarget::Registry, &cap, AceMode::Allow).unwrap(),
        "恢复后注册表键仍见 cap-ACE"
    );
    let e = run(format!("reg delete \"{}\" /f", key));
    assert!(e.code != 0, "恢复后注册表删除竟成功");

    // 环境卫生:全令牌删除测试键(残留键对其他用户无意义)
    let _ = std::process::Command::new("reg").args(["delete", &key, "/f"]).output();
    let _ = std::fs::remove_dir_all(&base);
}
