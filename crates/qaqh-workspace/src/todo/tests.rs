//! todo 测试（纯文本范式）：title 即身份、无 ID 分配、并行 in_progress、
//! meta 快照状态机。行格式解析的单元测试在 parse.rs；本文件锁工具面行为。

use super::actions::{exec_todo_set, todo_set_for};
use super::model::{TodoStatus, TodoStore};
use super::store::{read_store, todo_meta_snapshot, todo_path};
use serde_json::Value;

use std::ffi::OsString;

// ═══════════════════════════════════════════════════════
// harness
// ═══════════════════════════════════════════════════════

/// 隔离数据目录（USERPROFILE/HOME → 临时目录）并设置会话上下文；
/// 结束恢复环境，避免污染真实 ~/.qaqh/sessions。
fn with_isolated_todo<F: FnOnce(&str)>(f: F) {
    let _guard = crate::TEST_RUNTIME_SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let home_var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    let old_home: Option<OsString> = std::env::var_os(home_var);
    // Rust 2024: set_var/remove_var are unsafe (test-only, single-threaded via TEST_RUNTIME_SERIAL).
    unsafe { std::env::set_var(home_var, dir.path()) };
    let session_id = format!("test-seed-{}", std::process::id());
    crate::runtime::set_context(&session_id, 4);
    f(&session_id);
    unsafe {
        match old_home {
            Some(value) => std::env::set_var(home_var, value),
            None => std::env::remove_var(home_var),
        }
    }
}

fn parse(result: &Result<String, String>) -> serde_json::Value {
    serde_json::from_str(result.as_ref().unwrap()).unwrap()
}

fn titles(store: &TodoStore) -> Vec<String> {
    store.items.iter().map(|item| item.title.clone()).collect()
}

fn write_list(text: &str) -> serde_json::Value {
    parse_tool_result(
        handle_write(&split_ctx("todo_write", serde_json::json!({ "list": text }))).model_text(),
    )
}

fn update_title(title: &str, status: &str) -> serde_json::Value {
    let result = handle_update(&split_ctx(
        "todo_update",
        serde_json::json!({"title": title, "status": status}),
    ));
    assert!(
        result.error.is_none(),
        "todo_update {title}→{status} 不得失败: {:?}",
        result.error
    );
    parse_tool_result(result.model_text())
}

use super::split::{handle_update, handle_write};

fn parse_tool_result(text: &str) -> serde_json::Value {
    serde_json::from_str(text).unwrap()
}

fn split_ctx(_name: &str, args: Value) -> Value {
    args
}

// ═══════════════════════════════════════════════════════
// ① write：全量文本覆写
// ═══════════════════════════════════════════════════════

#[test]
fn write_parses_the_full_text_block() {
    with_isolated_todo(|_session| {
        let out = write_list(
            "- [ ] alpha | 描述 A\n- [~] beta\n- [x] gamma | | verified",
        );
        assert_eq!(out["total"], 3);
        assert_eq!(out["in_progress"], 1);
        let items = out["items"].as_array().unwrap();
        assert_eq!(items[0]["title"], "alpha");
        assert_eq!(items[0]["description"], "描述 A");
        assert_eq!(items[0]["status"], "pending");
        assert_eq!(items[1]["status"], "in_progress");
        assert_eq!(items[2]["status"], "completed");
        assert_eq!(items[2]["evidence"], "verified");
        // 身份即标题：id 与 title 一致。
        assert_eq!(items[0]["id"], "alpha");
    });
}

#[test]
fn write_rejects_duplicate_titles() {
    with_isolated_todo(|_session| {
        let result = handle_write(&split_ctx(
            "todo_write",
            serde_json::json!({"list": "- [ ] same\n- [x] same"}),
        ));
        assert!(result.error.is_some(), "重复标题必须拒绝");
    });
}

#[test]
fn write_empty_clears_the_list() {
    with_isolated_todo(|session_id| {
        write_list("- [ ] a");
        let cleared = write_list("");
        assert_eq!(cleared["total"], 0);
        assert!(read_store().unwrap().items.is_empty());
        assert_eq!(todo_meta_snapshot(session_id), None, "空列表不注入");
    });
}

#[test]
fn write_rebuild_replaces_instead_of_appending() {
    with_isolated_todo(|_session| {
        write_list("- [ ] a\n- [~] b");
        let out = write_list("- [x] a\n- [~] c");
        assert_eq!(out["total"], 2);
        let store = read_store().unwrap();
        assert_eq!(titles(&store), ["a", "c"], "未引用的 b 随覆写消失");
        assert_eq!(store.items[0].status, TodoStatus::Completed);
    });
}

// ═══════════════════════════════════════════════════════
// ② update：按 title 精确翻转 + 并行 in_progress
// ═══════════════════════════════════════════════════════

#[test]
fn parallel_in_progress_is_allowed() {
    with_isolated_todo(|_session| {
        write_list("- [~] a\n- [~] b\n- [ ] c");
        let store = read_store().unwrap();
        assert_eq!(
            store
                .items
                .iter()
                .filter(|item| item.status == TodoStatus::InProgress)
                .count(),
            2,
            "允许多个 todo 同时处于正在工作阶段"
        );
        // current_id 收敛到第一个 in_progress（展示语义，不是约束）。
        assert_eq!(store.current_id.as_deref(), Some("a"));
    });
}

#[test]
fn update_output_carries_full_items() {
    with_isolated_todo(|_session| {
        write_list("- [~] a\n- [ ] b");
        let after = update_title("a", "completed");
        assert_eq!(after["item"]["status"], "completed");
        let items = after["items"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["status"], "completed");
        assert_eq!(items[1]["status"], "pending");
    });
}

#[test]
fn update_unknown_title_lists_the_current_list() {
    with_isolated_todo(|_session| {
        write_list("- [ ] real-task");
        let result = handle_update(&split_ctx(
            "todo_update",
            serde_json::json!({"title": "real task", "status": "completed"}),
        ));
        let error = result.error.expect("近似标题必须 not_found");
        assert_eq!(error.code, "not_found");
        // 提示里带当前清单——模型可自行纠正拼写。
        assert!(
            error.message.contains("real-task") || error.hint.as_deref().unwrap_or_default().contains("real-task"),
            "错误面必须列出可用标题: {error:?}"
        );
    });
}

// ═══════════════════════════════════════════════════════
// ③ meta 快照状态机（注入契约）
// ═══════════════════════════════════════════════════════

#[test]
fn meta_snapshot_none_without_a_list() {
    with_isolated_todo(|session_id| {
        assert_eq!(todo_meta_snapshot(session_id), None, "无列表不注入");
        write_list("");
        // 空列表同样不注入（零 token）。
        assert_eq!(todo_meta_snapshot(session_id), None);
    });
}

#[test]
fn meta_snapshot_compact_line_names_the_active_task() {
    with_isolated_todo(|session_id| {
        write_list("- [x] first\n- [~] second\n- [ ] third");
        let snapshot = todo_meta_snapshot(session_id).expect("活跃列表必须注入");
        assert!(snapshot.contains("1/3 done"), "{snapshot}");
        assert!(snapshot.contains("now: second"), "{snapshot}");
        assert!(snapshot.contains("in_progress"), "{snapshot}");
        // 紧凑式不携带完整列表（token 经济）——只有一行。
        assert_eq!(snapshot.lines().count(), 1);
    });
}

#[test]
fn meta_snapshot_all_done_notifies_once() {
    with_isolated_todo(|session_id| {
        write_list("- [~] a\n- [ ] b");
        update_title("a", "completed");
        update_title("b", "cancelled");
        // 全部终态：下一次快照回一次 cleared 并置归档位。
        let cleared = todo_meta_snapshot(session_id).expect("全 done 必须回一次 cleared");
        assert!(cleared.contains("all tasks done"), "{cleared}");
        assert!(read_store().unwrap().archived, "归档位必须置位");
        // 之后每次快照都是 None（"下一次工具请求时置空而非挂旧列表"）。
        assert_eq!(todo_meta_snapshot(session_id), None);
        assert_eq!(todo_meta_snapshot(session_id), None);
    });
}

#[test]
fn meta_snapshot_unarchives_on_rebuild() {
    with_isolated_todo(|session_id| {
        write_list("- [x] a | | done");
        // write 直接建全完成列表：立即归档，首次快照回 cleared 一次。
        let cleared = todo_meta_snapshot(session_id).expect("首次快照回 cleared");
        assert!(cleared.contains("all tasks done"), "{cleared}");
        assert_eq!(todo_meta_snapshot(session_id), None);

        // 下一次 todo_write 全量重建：归档解除，注入恢复。
        write_list("- [~] fresh");
        assert!(!read_store().unwrap().archived);
        let snapshot = todo_meta_snapshot(session_id).expect("重建后恢复注入");
        assert!(snapshot.contains("now: fresh"), "{snapshot}");
    });
}

// ═══════════════════════════════════════════════════════
// 形态守卫（文本范式 + 双工具收敛）
// ═══════════════════════════════════════════════════════

#[test]
fn split_handlers_reject_cross_fields() {
    // write 只认 `list`：旧 items 形态与顶层便利字段一律拒绝。
    assert!(
        handle_write(&split_ctx(
            "todo_write",
            serde_json::json!({"list": "- [ ] t", "items": []})
        ))
        .error
        .is_some()
    );
    assert!(
        handle_write(&split_ctx(
            "todo_write",
            serde_json::json!({"items": [{"title": "t", "status": "pending"}]})
        ))
        .error
        .is_some()
    );
    assert!(
        handle_write(&split_ctx("todo_write", serde_json::json!({"title": "t"})))
            .error
            .is_some()
    );
    // update 只认 {title, status, evidence?}：ids/updates/id 一律拒绝。
    assert!(
        handle_update(&split_ctx(
            "todo_update",
            serde_json::json!({"ids": ["x"], "status": "pending"})
        ))
        .error
        .is_some()
    );
    assert!(
        handle_update(&split_ctx(
            "todo_update",
            serde_json::json!({"id": "x", "status": "pending"})
        ))
        .error
        .is_some()
    );
    assert!(
        handle_update(&split_ctx(
            "todo_update",
            serde_json::json!({"updates": [{"title": "x", "status": "pending"}]})
        ))
        .error
        .is_some()
    );
}

#[test]
fn v1_v2_action_vocabulary_retired() {
    for verb in [
        "create", "insert", "set", "list", "create_batch", "update", "cancel",
    ] {
        let out = handle_write(&split_ctx(
            "todo_write",
            serde_json::json!({ "action": verb, "list": "" }),
        ));
        assert!(out.error.is_some(), "action={verb} 必须返回错误");
    }
}

#[test]
fn session_parameterized_set_and_list_hit_explicit_session() {
    // HTTP service 面 / CLI 直访契约：显式 seed 读写与工具路径写同一份
    // todo.json（todo_list 工具退役后 service 面保留）。
    with_isolated_todo(|session_id| {
        write_list("- [ ] 工具路径建项");
        let set = parse(&todo_set_for(
            session_id,
            &serde_json::json!({"title": "工具路径建项", "status": "completed", "evidence": "via CLI"}),
        ));
        assert_eq!(set["item"]["status"], "completed");
        let listed = parse(&super::actions::todo_list_for(
            session_id,
            &serde_json::json!({}),
        ));
        assert_eq!(listed["items"].as_array().unwrap().len(), 1);
        assert_eq!(listed["items"][0]["status"], "completed");
    });
}

#[test]
fn set_status_never_erases_metadata() {
    with_isolated_todo(|_session| {
        write_list("- [ ] Keep me | Keep this too");
        exec_todo_set(&serde_json::json!({
            "title": "Keep me", "status": "in_progress"
        }))
        .unwrap();
        exec_todo_set(&serde_json::json!({
            "title": "Keep me", "status": "completed", "evidence": "verified"
        }))
        .unwrap();
        let store = read_store().unwrap();
        assert_eq!(store.items[0].title, "Keep me");
        assert_eq!(store.items[0].description, "Keep this too");
        assert_eq!(store.items[0].evidence.as_deref(), Some("verified"));
        assert_eq!(store.items[0].status, TodoStatus::Completed);
    });
}

#[test]
fn legacy_t_id_store_still_loads() {
    // 旧 T<n> todo.json 平滑迁移：serde 忽略 next_id/id 字段差异，
    // title 即身份继续可 update。
    with_isolated_todo(|_session| {
        let legacy = serde_json::json!({
            "items": [
                {"id": "T1", "title": "legacy-a", "description": "", "status": "pending"},
                {"id": "T2", "title": "legacy-b", "description": "", "status": "pending"}
            ],
            "mode": "manual",
            "current_id": null,
            "auto_turns": 0,
            "max_auto_turns": 24,
            "next_id": 3
        });
        let path = todo_path().expect("test ctx has session");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, legacy.to_string()).unwrap();
        update_title("legacy-a", "in_progress");
        let store = read_store().unwrap();
        assert_eq!(store.items[0].status, TodoStatus::InProgress);
        assert_eq!(titles(&store), ["legacy-a", "legacy-b"]);
    });
}
