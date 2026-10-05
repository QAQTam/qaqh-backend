//! M2 RedirectPlane 回归测试(sbx_win::projfs 模块;场景依据 ADR-0003 spike)。
//!
//! 无 ProjFS 环境(Client-ProjFS 未启用)自动跳过,不算失败。
//! 手动跑:cargo test -p sbx-win --test projfs -- --nocapture

use sbx_win::projfs::{self, Change, MergePolicy, View};
use std::path::{Path, PathBuf};
use std::time::Duration;

fn skip_unless_available() -> bool {
    if projfs::available() {
        return true;
    }
    eprintln!(
        "skip: ProjFS 不可用(Client-ProjFS 可选功能未启用)。\
一次性启用(需 admin):DISM /Online /Enable-Feature /FeatureName:Client-ProjFS /NoRestart"
    );
    false
}

fn fresh_dirs(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
    let base = std::env::temp_dir().join(format!("sbx-projfs-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let store = base.join("store");
    let view = base.join("view");
    std::fs::create_dir_all(&store).unwrap();
    std::fs::create_dir_all(&view).unwrap();
    (base, store, view)
}

#[test]
fn projfs_view_read_through_and_upper_semantics() {
    if !skip_unless_available() {
        return;
    }
    let (base, store, view) = fresh_dirs("main");
    std::fs::write(store.join("hello.txt"), "hello-from-lower").unwrap();
    std::fs::create_dir_all(store.join("sub")).unwrap();
    std::fs::write(store.join("sub").join("inner.txt"), "nested-lower").unwrap();

    let v = View::start(&store, &view).expect("View::start(unelevated) 失败");

    // read-through(根 + 递归子目录)
    assert_eq!(
        std::fs::read(view.join("hello.txt")).unwrap(),
        b"hello-from-lower",
        "read-through 内容不符"
    );
    assert_eq!(
        std::fs::read(view.join("sub").join("inner.txt")).unwrap(),
        b"nested-lower",
        "递归子目录投影失败"
    );
    let names: Vec<String> = std::fs::read_dir(&view)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(names.contains(&"hello.txt".to_string()) && names.contains(&"sub".to_string()));

    // 新建(嵌套)= upper:store 零接触,diff 报 New
    std::fs::create_dir_all(view.join("newdir")).unwrap();
    std::fs::write(view.join("newdir").join("deep.txt"), "written-in-view").unwrap();
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        !store.join("newdir").exists(),
        "新建穿透到了 store(upper 语义不成立)"
    );

    // 改写占位 = copy-up:store 零穿透
    std::fs::write(view.join("hello.txt"), "overwritten-in-view").unwrap();
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(
        std::fs::read(store.join("hello.txt")).unwrap(),
        b"hello-from-lower",
        "改写穿透到了 store"
    );

    // diff:New + Modified,无 Deleted(store 未动)
    let changes = v.diff().unwrap();
    assert!(
        changes.contains(&Change::New("newdir\\deep.txt".into()))
            && changes.contains(&Change::Modified("hello.txt".into())),
        "diff 缺 New/Modified: {changes:?}"
    );

    // tombstone:视图内删除 → diff 报 Deleted,store 仍在
    std::fs::remove_file(view.join("sub").join("inner.txt")).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    assert!(changes_contains(&v, &Change::Deleted("sub\\inner.txt".into())));

    drop(v);
    let _ = std::fs::remove_dir_all(&base);
}

fn changes_contains(v: &View, want: &Change) -> bool {
    v.diff().unwrap().iter().any(|c| c == want)
}

#[test]
fn projfs_merge_applies_changes_to_store() {
    if !skip_unless_available() {
        return;
    }
    let (base, store, view) = fresh_dirs("merge");
    std::fs::write(store.join("keep.txt"), "keep-me").unwrap();
    std::fs::write(store.join("edit.txt"), "old-content").unwrap();

    let v = View::start(&store, &view).unwrap();
    std::fs::write(view.join("added.txt"), "added-in-view").unwrap();
    std::fs::write(view.join("edit.txt"), "new-content").unwrap();
    std::fs::remove_file(view.join("keep.txt")).unwrap();
    std::thread::sleep(Duration::from_millis(100));

    let report = v.merge_with(MergePolicy::AllowDeletions).unwrap();
    assert!(report.applied >= 2, "merge 报告异常: {report:?}");

    // store 侧生效
    assert_eq!(std::fs::read_to_string(store.join("added.txt")).unwrap(), "added-in-view");
    assert_eq!(std::fs::read_to_string(store.join("edit.txt")).unwrap(), "new-content");
    assert!(!store.join("keep.txt").exists(), "视图内删除未在 merge 时落回 store");

    // 合并后 diff 归零(视图侧已还原为按新 store 投影)
    assert!(v.diff().unwrap().is_empty(), "merge 后 diff 未归零");

    drop(v);
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn projfs_discard_restores_view_millisecond_semantics() {
    if !skip_unless_available() {
        return;
    }
    let (base, store, view) = fresh_dirs("discard");
    std::fs::write(store.join("doc.txt"), "original").unwrap();

    let v = View::start(&store, &view).unwrap();
    std::fs::write(view.join("doc.txt"), "modified-in-view").unwrap();
    std::fs::write(view.join("extra.txt"), "new-in-view").unwrap();
    std::fs::remove_file(view.join("doc.txt")).unwrap(); // 先改后删 → tombstone
    std::thread::sleep(Duration::from_millis(100));

    let report = v.discard().unwrap();
    eprintln!("discard report: {report:?}");

    // store 全程零接触
    assert_eq!(
        std::fs::read_to_string(store.join("doc.txt")).unwrap(),
        "original",
        "discard 前 store 已被穿透"
    );

    // 视图恢复到与 store 一致:改写文件重新投影为原始内容
    assert_eq!(
        std::fs::read_to_string(view.join("doc.txt")).unwrap(),
        "original",
        "discard 后视图未还原改写"
    );
    // 新建文件消失
    assert!(!view.join("extra.txt").exists(), "discard 后新建文件仍在");
    // diff 归零
    assert!(v.diff().unwrap().is_empty(), "discard 后 diff 未归零");

    drop(v);
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn projfs_sandboxed_child_reads_projection_and_writes_view() {
    if !skip_unless_available() {
        return;
    }
    use sbx_win::{acl, desktop, env, policy::AceMode, sid, spawn::{self, Isolation}, token};

    let (base, store, view) = fresh_dirs("sandbox");
    std::fs::write(store.join("child-read.txt"), "hello-from-lower-child").unwrap();

    let v = View::start(&store, &view).unwrap();

    let cap = sid::capability_sid().unwrap();
    // 视图根挂 cap-SID FULL 掩码(ADR-0002 事实 2);占位继承根 DACL
    acl::add_ace_with_mask(&view, &cap, AceMode::Allow, true, acl::FULL_MASK).unwrap();
    let tok = token::create_restricted_token(&[cap.clone(), sid::everyone_sid()]).unwrap();
    let desk = desktop::create_private_desktop(&sid::sid_to_string(&cap)).unwrap();
    let scratch_tmp = view.join(".sbx-tmp");
    let scratch_home = view.join(".sbx-home");
    std::fs::create_dir_all(&scratch_tmp).unwrap();
    std::fs::create_dir_all(&scratch_home).unwrap();
    let pairs = env::minimal_child_env(&scratch_tmp, &scratch_home);

    let cmdline = format!(
        "\"{}\" /c type child-read.txt > got.txt & echo sandboxed > child-new.txt",
        std::env::var("ComSpec").unwrap()
    );
    let child = spawn::spawn(
        &Isolation::RestrictedToken { token: tok.handle() },
        &cmdline,
        &view,
        &pairs,
        Some(desk.name()),
    )
    .expect("沙箱 spawn 失败");
    let exit = child.wait().unwrap();
    assert_eq!(exit.code, 0, "子进程命令失败 stderr={}", String::from_utf8_lossy(&exit.stderr));

    // 子进程读投影文件(水合由本测试进程服务)且写落在视图
    assert_eq!(
        std::fs::read(view.join("got.txt")).unwrap(),
        b"hello-from-lower-child",
        "子进程读投影文件失败(跨进程水合)"
    );
    assert!(view.join("child-new.txt").exists(), "子进程新建文件未落在视图");
    // 变更只存在于视图:diff 可见,store 干净
    assert!(!store.join("got.txt").exists(), "子进程变更穿透到了 store");
    let changes = v.diff().unwrap();
    assert!(
        changes.contains(&Change::New("got.txt".into())) && changes.iter().any(|c| matches!(c, Change::New(r) if r.starts_with("child-new"))),
        "diff 缺子进程变更: {changes:?}"
    );

    // merge 落回 store(turn 边界语义)
    v.merge().unwrap();
    assert_eq!(
        std::fs::read_to_string(store.join("got.txt")).unwrap(),
        "hello-from-lower-child"
    );

    drop(v);
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn projfs_start_rejects_non_empty_root() {
    if !skip_unless_available() {
        return;
    }
    let (base, store, view) = fresh_dirs("nonempty");
    std::fs::write(view.join("junk.txt"), "x").unwrap();
    assert!(View::start(&store, &view).is_err(), "非空根必须被拒绝");
    drop_paths(&base);
}

fn drop_paths(base: &Path) {
    let _ = std::fs::remove_dir_all(base);
}

