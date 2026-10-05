//! M2(三)RedirectPlane 接线回归(sbx_win::redirect;场景依据 HANDOFF v3)。
//!
//! 无 ProjFS 环境(Client-ProjFS 未启用)自动跳过,不算失败。
//! 手动跑:cargo test -p sbx-win --test redirect -- --nocapture

use sbx_win::projfs::Change;
use sbx_win::redirect::{self, split_plan};
use sbx_win::{
    desktop, env, policy::SbxPolicy, spawn::{self, Isolation},
};
use std::path::{Path, PathBuf};
use std::time::Duration;

fn skip_unless_available() -> bool {
    if sbx_win::projfs::available() {
        return true;
    }
    eprintln!(
        "skip: ProjFS 不可用(Client-ProjFS 可选功能未启用)。\
一次性启用(需 admin):DISM /Online /Enable-Feature /FeatureName:Client-ProjFS /NoRestart"
    );
    false
}

/// 测试工作区布局(store 侧;`.git` = deny carveout 候选)。
fn fresh_workspace(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
    let base = std::env::temp_dir().join(format!("sbx-redirect-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let ws = base.join("ws");
    let view = base.join("view");
    std::fs::create_dir_all(ws.join(".git")).unwrap();
    std::fs::write(ws.join(".git").join("config"), "gitcfg").unwrap();
    std::fs::create_dir_all(ws.join("target")).unwrap();
    std::fs::write(ws.join("README.md"), "readme-v1").unwrap();
    std::fs::write(ws.join("secret.txt"), "untouched").unwrap();
    (base, ws, view)
}

fn policy_for(ws: &Path) -> SbxPolicy {
    // JSON 内路径统一正斜杠(仓内解析全接受;反斜杠需转义)
    let fwd = |p: PathBuf| p.display().to_string().replace('\\', "/");
    serde_json::from_str(&format!(
        r#"{{
            "writable_roots": ["{}"],
            "writable_files": ["{}"],
            "deny_write_paths": ["{}"],
            "redirect": true
        }}"#,
        fwd(ws.join("target")),
        fwd(ws.join("README.md")),
        fwd(ws.join(".git")),
    ))
    .unwrap()
}

/// 沙箱子进程(受限令牌,cwd = 视图根)在视图内跑一条 cmd 命令。
/// `cap` = 与 prepare_turn 的视图 op 同源的 capability SID(逐目标授权,
/// 根上无 cap ACE——protected 重建后根 DACL 只含 user ACE)。
fn run_in_view(view: &Path, cap: &[u8], inner: &str) -> sbx_win::spawn::Exit {
    let tok =
        sbx_win::token::create_restricted_token(&[cap.to_vec(), sbx_win::sid::everyone_sid()])
            .unwrap();
    // 参数是 SID 文本(嵌入桌面 SDDL),不是桌面名
    let desk =
        desktop::create_private_desktop(&sbx_win::sid::sid_to_string(cap)).unwrap();
    let pairs = env::minimal_child_env(&view.join(".sbx-tmp"), &view.join(".sbx-home"));
    let cmdline = format!("\"{}\" /c {}", std::env::var("ComSpec").unwrap(), inner);
    let child = spawn::spawn(
        &Isolation::RestrictedToken { token: tok.handle() },
        &cmdline,
        view,
        &pairs,
        Some(desk.name()),
    )
    .expect("沙箱 spawn 失败");
    child.wait().unwrap()
}

fn has_change(changes: &[Change], want: &Change) -> bool {
    changes.iter().any(|c| c == want)
}

/// 大小写不敏感查找(diff 的 rel 来自文件系统枚举,大小写以卷上为准)。
fn contains_ci(changes: &[Change], kind: &str, path: &str) -> bool {
    let p = path.to_lowercase();
    changes.iter().any(|c| match c {
        Change::New(r) if kind == "new" => r.to_lowercase() == p,
        Change::Modified(r) if kind == "modified" => r.to_lowercase() == p,
        Change::Deleted(r) if kind == "deleted" => r.to_lowercase() == p,
        _ => false,
    })
}

/// 失败定位探针(手动跑):逐条隔离视图 op 的 ACE 应用。
#[test]
fn diag_view_ops_isolated() {
    if !skip_unless_available() {
        return;
    }
    let (base, ws, view) = fresh_workspace("diag");
    let cap = sbx_win::sid::capability_sid().unwrap();
    let split = split_plan(&policy_for(&ws), &cap, &ws, &view).unwrap();
    let turn = redirect::prepare_turn(&ws, &view, &[]).expect("prepare_turn(无 ops)");
    for v in &split.view {
        let r = turn.debug_apply_view_op(v);
        eprintln!("apply rel={:?} mode={:?} -> {:?}", v.rel, v.op.mode, r);
    }
}

#[test]
fn redirect_turn_merge_end_to_end() {
    if !skip_unless_available() {
        return;
    }
    let (base, ws, view) = fresh_workspace("merge");
    let cap = sbx_win::sid::capability_sid().unwrap();
    let split = split_plan(&policy_for(&ws), &cap, &ws, &view).expect("split_plan 失败");
    let turn = redirect::prepare_turn(&ws, &view, &split.view).expect("prepare_turn 失败");

    // 授权写(目录 + 逐文件)与未授权写(无名文件 / deny carveout / 按真实
    // 路径直捣 store)一次跑完
    let secret_real = ws.join("secret.txt");
    let e = run_in_view(
        turn.root(),
        &cap,
        &format!(
            "echo built > target\\out.txt & echo new > README.md & echo x > secret.txt & echo x > .git\\hack & echo x > {}",
            secret_real.display()
        ),
    );
    eprintln!("child exit={} stderr={}", e.code, String::from_utf8_lossy(&e.stderr));
    std::thread::sleep(Duration::from_millis(100));

    let changes = turn.diff().unwrap();
    eprintln!("changes: {changes:?}");

    // 授权写全部落在视图(store 未动)
    assert!(
        has_change(&changes, &Change::New("target\\out.txt".into())),
        "diff 缺 target\\out.txt: {changes:?}"
    );
    assert!(
        contains_ci(&changes, "modified", "README.md"),
        "diff 缺 README.md: {changes:?}"
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("README.md")).unwrap(),
        "readme-v1",
        "store 的 README.md 被穿透"
    );
    assert!(!ws.join("target").join("out.txt").exists(), "store 的 target 被穿透");

    // 未授权写被内核拒绝:secret 无痕迹(视图与 store)、.git deny 生效
    assert!(
        !changes
            .iter()
            .any(|c| matches!(c, Change::New(r) if r.to_lowercase().contains("secret"))),
        "未授权文件写入进了视图: {changes:?}"
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("secret.txt")).unwrap(),
        "untouched",
        "按真实路径的直捣写入穿透到了 store"
    );
    assert!(!view.join(".git").join("hack").exists() && !contains_ci(&changes, "new", r".git\hack"), "deny carveout(.git)被写穿");

    // merge 落回 store(turn 边界审批流)
    let report = turn.merge().unwrap();
    eprintln!("merge: {report:?}");
    assert!(
        std::fs::read_to_string(ws.join("target").join("out.txt")).unwrap().contains("built"),
        "merge 未落盘 target\\out.txt"
    );
    assert!(
        std::fs::read_to_string(ws.join("README.md"))
            .unwrap()
            .starts_with("new"),
        "merge 未落盘 README.md"
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("secret.txt")).unwrap(),
        "untouched",
        "merge 把未授权内容带进了 store"
    );
    assert!(turn.diff().unwrap().is_empty(), "merge 后 diff 未归零");

    drop(turn);
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn redirect_turn_discard_restores_millisecond_semantics() {
    if !skip_unless_available() {
        return;
    }
    let (base, ws, view) = fresh_workspace("discard");
    let cap = sbx_win::sid::capability_sid().unwrap();
    let split = split_plan(&policy_for(&ws), &cap, &ws, &view).unwrap();
    let turn = redirect::prepare_turn(&ws, &view, &split.view).unwrap();

    run_in_view(
        turn.root(),
        &cap,
        "echo changed > README.md & echo junk > target\\junk.txt & del secret.txt",
    );
    std::thread::sleep(Duration::from_millis(100));
    let changes = turn.diff().unwrap();
    assert!(
        !changes.is_empty(),
        "discard 前视图应持有变更(改写/新建/删除): {changes:?}"
    );

    let report = turn.discard().unwrap();
    eprintln!("discard: {report:?}");

    // store 全程零接触
    assert_eq!(
        std::fs::read_to_string(ws.join("README.md")).unwrap(),
        "readme-v1",
        "discard 前 store 已被穿透"
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("secret.txt")).unwrap(),
        "untouched",
        "视图内删除穿透到了 store"
    );
    // 视图恢复到与 store 一致:改写重投影、新建消失、删除的 tombstone 撤销
    assert_eq!(
        std::fs::read_to_string(turn.root().join("README.md")).unwrap(),
        "readme-v1",
        "discard 后视图未还原改写"
    );
    assert!(
        !turn.root().join("target").join("junk.txt").exists(),
        "discard 后新建文件仍在"
    );
    assert!(
        turn.root().join("secret.txt").exists(),
        "discard 后被删文件未重新投影"
    );
    assert!(turn.diff().unwrap().is_empty(), "discard 后 diff 未归零");

    drop(turn);
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn redirect_notifications_flow_to_parent() {
    if !skip_unless_available() {
        return;
    }
    let (base, ws, view) = fresh_workspace("notif");
    let cap = sbx_win::sid::capability_sid().unwrap();
    let split = split_plan(&policy_for(&ws), &cap, &ws, &view).unwrap();
    let turn = redirect::prepare_turn(&ws, &view, &split.view).unwrap();

    // 子进程:改写授权文件 + 删除授权文件(通知观察流:v1 纯记录)
    run_in_view(turn.root(), &cap, "echo x > README.md & del secret.txt");
    std::thread::sleep(Duration::from_millis(200));

    let notes = turn.take_notifications();
    eprintln!("notifications: {notes:?}");
    assert!(
        notes
            .iter()
            .any(|n| matches!(n, sbx_win::projfs::Notification::Overwritten { path } if path.eq_ignore_ascii_case("README.md"))),
        "缺 README.md 覆盖写通知: {notes:?}"
    );
    assert!(
        notes
            .iter()
            .any(|n| matches!(n, sbx_win::projfs::Notification::PreDelete { path } if path == "secret.txt")),
        "缺 secret.txt pre-delete 通知: {notes:?}"
    );

    // 取走后清空(幂等)
    assert!(turn.take_notifications().is_empty(), "take_notifications 后未清空");

    drop(turn);
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn redirect_merge_deletions_require_explicit_confirmation() {
    if !skip_unless_available() {
        return;
    }
    let (base, ws, view) = fresh_workspace("delmerge");
    let cap = sbx_win::sid::capability_sid().unwrap();
    let split = split_plan(&policy_for(&ws), &cap, &ws, &view).unwrap();
    let turn = redirect::prepare_turn(&ws, &view, &split.view).unwrap();

    // 子进程删除未授权文件(DELETE 绕过写限制,ADR-0004)——只进视图名单
    run_in_view(turn.root(), &cap, "del secret.txt");
    std::thread::sleep(Duration::from_millis(100));
    let changes = turn.diff().unwrap();
    assert!(
        contains_ci(&changes, "deleted", "secret.txt"),
        "视图内删除应进 diff: {changes:?}"
    );

    // 默认 merge:Deleted 项被跳过(需显式确认),store 文件仍在;
    // 视图侧 tombstone 被撤销 → diff 归零,文件重新投影
    let report = turn.merge().unwrap();
    assert_eq!(report.skipped_deletions, 1, "默认 merge 应跳过删除: {report:?}");
    assert_eq!(report.deleted, 0, "默认 merge 不应执行删除: {report:?}");
    assert!(
        ws.join("secret.txt").exists(),
        "默认 merge 后 store 文件被删(ADR-0004 被违反)"
    );
    assert!(turn.diff().unwrap().is_empty(), "merge 后 diff 未归零");
    assert!(
        turn.root().join("secret.txt").exists(),
        "被删文件应重新投影"
    );

    // 显式确认后的 merge(AllowDeletions):真实删除生效
    run_in_view(turn.root(), &cap, "del secret.txt");
    std::thread::sleep(Duration::from_millis(100));
    let report2 = turn
        .merge_with(sbx_win::projfs::MergePolicy::AllowDeletions)
        .unwrap();
    assert_eq!(report2.deleted, 1, "确认后的 merge 应执行删除: {report2:?}");
    assert!(
        !ws.join("secret.txt").exists(),
        "确认后的删除应落回 store"
    );
    assert!(turn.diff().unwrap().is_empty(), "确认 merge 后 diff 未归零");

    drop(turn);
    let _ = std::fs::remove_dir_all(&base);
}
