//! 验收场景(spike = ADR-0001 的裁决实验)。
//!
//! 四组检查,对应 spec A1/A2 的两个授权形态、读透传与 M2 重定向:
//! - shape_a:目录级 allow(整棵子树)+ `.git` deny carveout —— qaqh 默认策略形态
//! - shape_b:逐文件 allow(writable_files)—— write_paths 审批形态
//! - shape_c:scratch 目录可写(环境重指的前提)
//! - shape_d:redirect turn(M2,ADR-0003)—— 授权写被视图捕获、未授权写
//!   内核拒绝、merge 落回 store(ProjFS 不可用 → d0 失败并给出启用指引)
//!
//! 全部通过 = cap-SID 锚点成立;失败原因指向 DACL 追加被拒 = 回退 Low-IL Plan B。

use crate::acl;
use crate::env::minimal_child_env;
use crate::events::NullSink;
use crate::policy::{AceMode, AceOp, NetworkPolicy, Precreate, SbxPolicy, build_ace_plan};
use crate::sid;
use crate::spawn;
use crate::token;
use std::io;
use std::path::Path;

pub struct CheckResult {
    pub name: String,
    pub passed: bool,
    pub detail: String,
}

pub struct Report {
    pub elevated: bool,
    pub cap_sid: String,
    pub checks: Vec<CheckResult>,
}

impl Report {
    pub fn all_passed(&self) -> bool {
        self.checks.iter().all(|c| c.passed)
    }
}

fn comspec() -> String {
    std::env::var("ComSpec").unwrap_or_else(|_| "C:\\Windows\\system32\\cmd.exe".into())
}

fn cmd_line(inner: &str) -> String {
    format!("\"{}\" /c {}", comspec(), inner)
}

fn quoted(p: &Path) -> String {
    format!("\"{}\"", p.display())
}

fn check(name: &str, cond: bool, detail: String) -> CheckResult {
    CheckResult {
        name: name.to_string(),
        passed: cond,
        detail,
    }
}

/// 在 `base` 下运行全部验收检查。调用方负责 base 不存在/可创建。
pub fn run_all(base: &Path) -> io::Result<Report> {
    let sink = NullSink;
    let cap = sid::capability_sid().map_err(io::Error::other)?;
    let cap_text = sid::sid_to_string(&cap);
    let everyone = sid::everyone_sid();

    let scratch = base.join("scratch");
    std::fs::create_dir_all(scratch.join("tmp"))?;
    std::fs::create_dir_all(scratch.join("home"))?;

    // shape A:allow ws 树 + deny .git 树
    let ws_a = base.join("ws_a");
    let git = ws_a.join(".git");
    std::fs::create_dir_all(&git)?;
    std::fs::write(git.join("config"), "gitcfg")?;
    let plan_a: Vec<AceOp> = {
        let policy = SbxPolicy {
            writable_roots: vec![ws_a.clone()],
            writable_files: vec![],
            deny_write_paths: vec![git.clone()],
            network: crate::policy::NetworkPolicy::Deny,
            isolation: crate::policy::IsolationKind::Token,
            readable_roots: vec![],
            capabilities: vec![],
            writable_registry_keys: vec![],
            redirect: false,
        };
        build_ace_plan(&policy, &cap)
    };
    // scratch 可写(A3 前提)
    let mut plan_a = plan_a;
    plan_a.push(AceOp {
        path: scratch.clone(),
        mode: AceMode::Allow,
        sid: cap.clone(),
        inherit_tree: true,
        precreate: Precreate::None,
        full_mask: false,
        kind: crate::policy::AceTarget::File,
    });
    let _ = acl::apply_ops(&plan_a, &sink)?;

    // shape B:逐文件 allow
    let ws_b = base.join("ws_b");
    std::fs::create_dir_all(&ws_b)?;
    let allowed = ws_b.join("allowed.txt");
    let protected = ws_b.join("protected.txt");
    std::fs::write(&allowed, "old")?;
    std::fs::write(&protected, "old")?;
    // b4:writable_files 目标不存在 → 预创建后放行(codex 预创建语义)
    let fresh = ws_b.join("fresh.txt");
    let plan_b: Vec<AceOp> = {
        let policy = SbxPolicy {
            writable_roots: vec![],
            writable_files: vec![allowed.clone(), fresh.clone()],
            deny_write_paths: vec![],
            network: NetworkPolicy::Deny,
            isolation: crate::policy::IsolationKind::Token,
            readable_roots: vec![],
            capabilities: vec![],
            writable_registry_keys: vec![],
            redirect: false,
        };
        build_ace_plan(&policy, &cap)
    };
    let _ = acl::apply_ops(&plan_b, &sink)?;

    let tok = token::create_restricted_token(&[cap.clone(), everyone])
        .map_err(|e| io::Error::other(format!("restricted token: {e}")))?;

    let desktop = crate::desktop::create_private_desktop(&cap_text)
        .map_err(|e| io::Error::other(format!("private desktop: {e}")))?;

    let env_pairs = minimal_child_env(&scratch.join("tmp"), &scratch.join("home"));
    let run_in = |cwd: &Path, inner: String| -> io::Result<spawn::Exit> {
        let child = spawn::spawn(
            &spawn::Isolation::RestrictedToken { token: tok.handle() },
            &cmd_line(&inner),
            cwd,
            &env_pairs,
            Some(desktop.name()),
        )
        .map_err(|e| io::Error::other(format!("spawn: {e}")))?;
        child.wait()
    };
    let run = |inner: String| -> io::Result<spawn::Exit> { run_in(&scratch, inner) };

    let mut checks = Vec::new();

    // shape A
    let ok_file = ws_a.join("ok.txt");
    let e = run(format!("echo x > {}", quoted(&ok_file)))?;
    checks.push(check(
        "a1_root_allow_write_new_file",
        e.code == 0 && ok_file.exists(),
        format!("exit={}", e.code),
    ));

    let git_config = git.join("config");
    let e = run(format!("echo x > {}", quoted(&git_config)))?;
    checks.push(check(
        "a2_git_deny_write_denied",
        e.code != 0,
        format!("exit={} stderr={}", e.code, String::from_utf8_lossy(&e.stderr).trim()),
    ));

    let e = run(format!("type {}", quoted(&git_config)))?;
    checks.push(check(
        "a3_git_read_passthrough",
        e.code == 0 && String::from_utf8_lossy(&e.stdout).contains("gitcfg"),
        format!("exit={} stdout={}", e.code, String::from_utf8_lossy(&e.stdout).trim()),
    ));

    // shape B
    let e = run(format!("echo x >> {}", quoted(&allowed)))?;
    let content = std::fs::read_to_string(&allowed)?;
    checks.push(check(
        "b1_writable_file_allowed",
        e.code == 0 && content.contains("x"),
        format!("exit={} content={content:?}", e.code),
    ));

    let e = run(format!("echo x > {}", quoted(&protected)))?;
    let content = std::fs::read_to_string(&protected)?;
    checks.push(check(
        "b2_unlisted_file_denied",
        e.code != 0 && content == "old",
        format!("exit={} content={content:?}", e.code),
    ));

    let e = run(format!("type {}", quoted(&protected)))?;
    checks.push(check(
        "b3_read_passthrough",
        e.code == 0 && String::from_utf8_lossy(&e.stdout).contains("old"),
        format!("exit={}", e.code),
    ));

    let e = run(format!("echo new > {}", quoted(&fresh)))?;
    checks.push(check(
        "b4_writable_file_precreated",
        e.code == 0 && fresh.exists(),
        format!("exit={}", e.code),
    ));

    // scratch
    let t = scratch.join("tmp").join("probe.txt");
    let e = run(format!("echo y > {}", quoted(&t)))?;
    checks.push(check(
        "c1_scratch_writable",
        e.code == 0 && t.exists(),
        format!("exit={}", e.code),
    ));

    // shape D:redirect turn(M2;ProjFS 不可用 → d0 失败 + 启用指引,后续跳过)
    let projfs_ok = crate::projfs::available();
    checks.push(check(
        "d0_projfs_available",
        projfs_ok,
        if projfs_ok {
            "ok".into()
        } else {
            "一次性启用(需 admin):DISM /Online /Enable-Feature /FeatureName:Client-ProjFS /NoRestart".into()
        },
    ));
    if projfs_ok {
        let ws_d = base.join("ws_d");
        let target_d = ws_d.join("target");
        let git_d = ws_d.join(".git");
        std::fs::create_dir_all(&target_d)?;
        std::fs::create_dir_all(&git_d)?;
        std::fs::write(git_d.join("config"), "gitcfg")?;
        std::fs::write(ws_d.join("README.md"), "readme-v1")?;
        std::fs::write(ws_d.join("secret.txt"), "untouched")?;
        let view_d = base.join("view_d");
        let policy_d = SbxPolicy {
            writable_roots: vec![target_d.clone()],
            writable_files: vec![ws_d.join("README.md")],
            deny_write_paths: vec![git_d.clone()],
            network: NetworkPolicy::Deny,
            isolation: crate::policy::IsolationKind::Token,
            readable_roots: vec![],
            capabilities: vec![],
            writable_registry_keys: vec![],
            redirect: true,
        };
        let split_d = crate::redirect::split_plan(&policy_d, &cap, &ws_d, &view_d)?;
        let turn_d = crate::redirect::prepare_turn(&ws_d, &view_d, &split_d.view)?;
        // 授权写(目录 root + 逐文件)+ 未授权写(无名文件 / deny carveout /
        // 按真实路径直捣 store)一次跑完
        let e = run_in(
            turn_d.root(),
            format!(
                "echo built > target\\out.txt & echo new > README.md & echo x > secret.txt & echo x > .git\\hack & echo x > {}",
                quoted(&ws_d.join("secret.txt"))
            ),
        )?;
        let changes = turn_d.diff()?;

        // d1:授权写全部落在视图(upper),store 未动
        let new_out = changes
            .iter()
            .any(|c| matches!(c, crate::projfs::Change::New(r) if r == "target\\out.txt"));
        let mod_readme = changes
            .iter()
            .any(|c| matches!(c, crate::projfs::Change::Modified(r) if r.eq_ignore_ascii_case("README.md")));
        let store_clean = !target_d.join("out.txt").exists()
            && std::fs::read_to_string(ws_d.join("README.md"))?.starts_with("readme-v1");
        checks.push(check(
            "d1_authorized_writes_captured_in_view",
            new_out && mod_readme && store_clean,
            format!("exit={} changes={changes:?}", e.code),
        ));

        // d2:未授权写被内核拒绝(无 diff 记录,store/视图均无痕迹)
        let secret_clean = std::fs::read_to_string(ws_d.join("secret.txt"))? == "untouched"
            && !changes
                .iter()
                .any(|c| matches!(c, crate::projfs::Change::New(r) if r.contains("secret")));
        let git_clean = !git_d.join("hack").exists();
        checks.push(check(
            "d2_unauthorized_writes_denied",
            secret_clean && git_clean,
            format!("changes={changes:?}"),
        ));

        // d3:merge 落回 store(turn 边界审批流)
        turn_d.merge()?;
        let merged = std::fs::read_to_string(target_d.join("out.txt"))?.contains("built")
            && std::fs::read_to_string(ws_d.join("README.md"))?.starts_with("new")
            && std::fs::read_to_string(ws_d.join("secret.txt"))? == "untouched";
        checks.push(check("d3_merge_applies_to_store", merged, format!("exit={}", e.code)));
    }

    Ok(Report {
        elevated: token::is_elevated(),
        cap_sid: cap_text,
        checks,
    })
}
