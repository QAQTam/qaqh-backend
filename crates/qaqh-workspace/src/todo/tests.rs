use super::actions::{exec_todo_create, exec_todo_set, todo_set_for};
use super::model::{TodoStatus, TodoStore};
use super::parse::expand_todo_ids;
use super::store::{load_todo, read_store, save_todo, todo_path};
use serde_json::Value;

use super::*;
use crate::tool_api::ToolProjection;
use std::ffi::OsString;

#[test]
fn todo_range_capped() {
    // 巨大范围必须报错而非物化（防 OOM/卡死 agent loop）。
    assert!(expand_todo_ids("T1-T4000000000").is_err());
    // 恰好等于上限的可接受（含端点 1000 个）。
    assert_eq!(expand_todo_ids("T1-T1000").unwrap().len(), 1000);
    // 超上限一个即拒绝。
    assert!(expand_todo_ids("T1-T1001").is_err());
    // 普通小范围不受影响（含端点）。
    assert_eq!(expand_todo_ids("T2-T4").unwrap(), vec!["T2", "T3", "T4"]);
}
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

fn ids(store: &TodoStore) -> Vec<String> {
    store.items.iter().map(|item| item.id.clone()).collect()
}

#[test]
fn create_group_assigns_consecutive_ids_atomically() {
    with_isolated_todo(|_session| {
        exec_todo_create(&serde_json::json!({"title": "single"}), false).unwrap();
        let result = exec_todo_create(
            &serde_json::json!({
                "items": [
                    {"title": "a"},
                    {"title": "b", "description": "desc b"},
                    {"title": "c"}
                ]
            }),
            false,
        );
        let value = parse(&result);
        let got: Vec<&str> = value["created"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["id"].as_str().unwrap())
            .collect();
        assert_eq!(got, ["T2", "T3", "T4"]);
        assert_eq!(ids(&read_store().unwrap()), ["T1", "T2", "T3", "T4"]);
    });
}

#[test]
fn create_group_is_atomic_on_validation_failure() {
    with_isolated_todo(|_session| {
        let result = exec_todo_create(
            &serde_json::json!({
                "items": [{"title": "good"}, {"title": "   "}]
            }),
            false,
        );
        assert!(result.is_err());
        assert!(read_store().unwrap().items.is_empty());
    });
}

#[test]
fn create_rejects_oversized_groups() {
    with_isolated_todo(|_session| {
        // v4：空 items 对模型工具是合法的空清单（覆写语义）；旧 create 路径
        // 仅 HTTP/CLI 预留，此处只锁 >20 上限仍拒绝。
        let items: Vec<Value> = (0..21)
            .map(|index| serde_json::json!({"title": format!("t{index}"), "status": "pending"}))
            .collect();
        let oversized = handle_write(&split_ctx(
            "todo_write",
            serde_json::json!({"items": items}),
        ));
        assert!(oversized.error.is_some());
        let ok = handle_write(&split_ctx(
            "todo_write",
            serde_json::json!({"items": [
                {"title": "within", "status": "pending"}
            ]}),
        ));
        assert!(ok.error.is_none());
    });
}

#[test]
fn insert_preserves_ids_and_changes_display_order() {
    with_isolated_todo(|_session| {
        exec_todo_create(
            &serde_json::json!({
                "items": [{"title": "a"}, {"title": "b"}]
            }),
            false,
        )
        .unwrap();
        let result = exec_todo_create(
            &serde_json::json!({"title": "child", "after_id": "T1"}),
            true,
        );
        assert_eq!(parse(&result)["created"][0]["id"], "T3");
        assert_eq!(ids(&read_store().unwrap()), ["T1", "T3", "T2"]);
    });
}

#[test]
fn set_status_is_id_only_and_never_erases_metadata() {
    with_isolated_todo(|_session| {
        exec_todo_create(
            &serde_json::json!({"title": "Keep me", "description": "Keep this too"}),
            false,
        )
        .unwrap();
        exec_todo_set(&serde_json::json!({
            "id": "T1",
            "status": "in_progress",
            "title": "",
            "description": ""
        }))
        .unwrap();
        exec_todo_set(&serde_json::json!({
            "id": "T1",
            "status": "completed",
            "evidence": "verified",
            "title": "",
            "description": ""
        }))
        .unwrap();
        let store = read_store().unwrap();
        assert_eq!(store.items[0].title, "Keep me");
        assert_eq!(store.items[0].description, "Keep this too");
        assert_eq!(store.items[0].evidence.as_deref(), Some("verified"));
        assert_eq!(store.items[0].status, TodoStatus::Completed);
        assert!(store.current_id.is_none());
    });
}

#[test]
fn current_id_falls_back_to_another_in_progress_task() {
    with_isolated_todo(|_session| {
        exec_todo_create(
            &serde_json::json!({"items": [{"title": "a"}, {"title": "b"}]}),
            false,
        )
        .unwrap();
        exec_todo_set(&serde_json::json!({"id": "T1", "status": "in_progress"})).unwrap();
        exec_todo_set(&serde_json::json!({"id": "T2", "status": "in_progress"})).unwrap();
        exec_todo_set(&serde_json::json!({"id": "T1", "status": "completed"})).unwrap();
        assert_eq!(read_store().unwrap().current_id.as_deref(), Some("T2"));
    });
}

#[test]
fn insert_requires_exactly_one_existing_anchor() {
    with_isolated_todo(|_session| {
        exec_todo_create(&serde_json::json!({"title": "parent"}), false).unwrap();

        let missing = exec_todo_create(&serde_json::json!({"title": "child"}), true);
        assert!(missing.is_err());

        let invalid = exec_todo_create(
            &serde_json::json!({"title": "child", "after_id": "not-an-id"}),
            true,
        );
        assert!(invalid.is_err());

        let absent = exec_todo_create(
            &serde_json::json!({"title": "child", "before_id": "T99"}),
            true,
        );
        assert!(absent.is_err());

        let conflicting = exec_todo_create(
            &serde_json::json!({
                "title": "child",
                "after_id": "T1",
                "before_id": "T1"
            }),
            true,
        );
        assert!(conflicting.is_err());
        assert_eq!(ids(&read_store().unwrap()), ["T1"]);
    });
}

#[test]
fn set_batch_ids_with_range_sets_same_status() {
    with_isolated_todo(|_session| {
        exec_todo_create(
            &serde_json::json!({"items": [{"title": "a"}, {"title": "b"}, {"title": "c"}]}),
            false,
        )
        .unwrap();
        let result = exec_todo_set(&serde_json::json!({
            "ids": ["T1-T3"],
            "status": "completed"
        }));
        let value = parse(&result);
        assert_eq!(value["updated"].as_array().unwrap().len(), 3);
        assert_eq!(value["not_found"].as_array().unwrap().len(), 0);
        let store = read_store().unwrap();
        assert_eq!(
            store
                .items
                .iter()
                .filter(|item| item.status == TodoStatus::Completed)
                .count(),
            3
        );
        assert!(store.current_id.is_none());

        // 逗号列表 + 单个混合：T1,T3 + T2
        exec_todo_set(&serde_json::json!({
            "ids": ["T1,T3", "T2"],
            "status": "in_progress"
        }))
        .unwrap();
        let store = read_store().unwrap();
        assert_eq!(
            store
                .items
                .iter()
                .filter(|item| item.status == TodoStatus::InProgress)
                .count(),
            3
        );
    });
}

#[test]
fn set_updates_parallel_sets_per_item_status() {
    with_isolated_todo(|_session| {
        exec_todo_create(
            &serde_json::json!({"items": [{"title": "a"}, {"title": "b"}, {"title": "c"}]}),
            false,
        )
        .unwrap();
        let result = exec_todo_set(&serde_json::json!({
            "updates": [
                {"id": "T1", "status": "completed"},
                {"id": "T2", "status": "in_progress"},
                {"id": "T3", "status": "cancelled", "evidence": "skip"}
            ]
        }));
        let value = parse(&result);
        assert_eq!(value["updated"].as_array().unwrap().len(), 3);
        let store = read_store().unwrap();
        assert_eq!(store.items[0].status, TodoStatus::Completed);
        assert_eq!(store.items[1].status, TodoStatus::InProgress);
        assert_eq!(store.items[2].status, TodoStatus::Cancelled);
        assert_eq!(store.items[2].evidence.as_deref(), Some("skip"));
        assert_eq!(store.current_id.as_deref(), Some("T2"));
    });
}

#[test]
fn set_batch_reports_not_found_without_aborting() {
    with_isolated_todo(|_session| {
        exec_todo_create(&serde_json::json!({"title": "only"}), false).unwrap();
        // 部分命中：T1 更新，T2/T3 记入 not_found
        let result = exec_todo_set(&serde_json::json!({
            "ids": ["T1-T3"],
            "status": "completed"
        }));
        let value = parse(&result);
        assert_eq!(value["updated"].as_array().unwrap().len(), 1);
        assert_eq!(value["not_found"], serde_json::json!(["T2", "T3"]));
        assert_eq!(read_store().unwrap().items[0].status, TodoStatus::Completed);

        // 全部未命中 → 整体 NOT_FOUND
        let result = exec_todo_set(&serde_json::json!({
            "ids": ["T9-T10"],
            "status": "completed"
        }));
        assert!(result.is_err());
    });
}

#[test]
fn v2_field_guard_rejects_metadata_on_set() {
    let args = serde_json::json!({
        "action": "set",
        "id": "T1",
        "status": "completed",
        "title": ""
    });
    assert!(
        reject_fields(
            &args,
            &["title", "description", "items", "after_id", "before_id"],
            "set"
        )
        .is_err()
    );
}

#[test]
fn v1_v2_action_vocabulary_retired() {
    // 聚合退役（v3 直接替换，无软迁移并存——owner 产品早期无外部依赖）：
    // 旧 action 维（create/insert/set/list 及 V1 别名）在 todo_write/update
    // 的 items-only/单一形态下全部落 INVALID_INPUT（防回归守卫）。
    for verb in [
        "create",
        "insert",
        "set",
        "list",
        "create_batch",
        "update",
        "cancel",
    ] {
        let out = handle_write(&split_ctx(
            "todo_write",
            serde_json::json!({ "action": verb, "items": [] }),
        ));
        assert!(out.error.is_some(), "action={verb} 必须返回错误");
    }
}

#[test]
fn session_parameterized_set_and_list_hit_explicit_session() {
    // HTTP service 面 / CLI 直访契约：显式 seed 读写（write_store_for /
    // read_store_for），与工具路径（runtime ctx）写同一份 todo.json。
    with_isolated_todo(|session_id| {
        exec_todo_create(&serde_json::json!({"title": "工具路径建项"}), false).unwrap();
        let id = ids(&load_todo().unwrap())[0].clone();
        let set = parse(&todo_set_for(
            session_id,
            &serde_json::json!({"id": id, "status": "completed", "evidence": "via CLI"}),
        ));
        assert_eq!(set["item"]["status"], "completed");
        let listed = parse(&todo_list_for(session_id, &serde_json::json!({})));
        assert_eq!(listed["items"].as_array().unwrap().len(), 1);
        assert_eq!(listed["items"][0]["status"], "completed");
    });
}

#[test]
fn typed_todo_list_matches_wire_and_projection() {
    with_isolated_todo(|session_id| {
        exec_todo_create(&serde_json::json!({"title": "typed list"}), false).unwrap();
        let typed = super::typed::todo_list_for_typed(session_id, &serde_json::json!({})).unwrap();
        assert_eq!(typed.items.len(), 1);
        assert_eq!(typed.counts.total, 1);
        assert_eq!(
            typed.display(&serde_json::json!({})).summary.as_deref(),
            Some("1 task(s) · 0 in progress")
        );

        let value = super::typed::todo_list_value_for(
            session_id,
            &serde_json::json!({"session_id": session_id, "status": "pending"}),
        )
        .unwrap();
        assert_eq!(value["status"], "ok");
        assert_eq!(value["items"][0]["title"], "typed list");
        assert_eq!(value["counts"]["total"], 1);
    });
}

#[test]
fn high_water_id_survives_item_removal() {
    // 回归：max+1 推导在"删除最大项后新建"会复用 ID，破坏 IDs stable。
    // 高水位持久化后：T1-T3 建成 → next_id=4 落盘 → 移除 T3 → 新建仍得 T4。
    with_isolated_todo(|_session| {
        exec_todo_create(
            &serde_json::json!({"items": [{"title": "a"}, {"title": "b"}, {"title": "c"}]}),
            false,
        )
        .unwrap();
        assert_eq!(read_store().unwrap().next_id, 4);
        let mut store = load_todo().unwrap();
        store.items.pop(); // 模拟未来的删除/归档：移除 T3
        save_todo(&store).unwrap();
        exec_todo_create(&serde_json::json!({"items": [{"title": "d"}]}), false).unwrap();
        assert_eq!(read_store().unwrap().items.last().unwrap().id, "T4");
    });
}

#[test]
fn legacy_store_without_next_id_migrates() {
    // 旧格式文件（无 next_id 字段）→ 首次分配按现存最大号 +1 迁移。
    with_isolated_todo(|_session| {
        let legacy = serde_json::json!({
            "items": [
                {"id": "T1", "title": "a", "description": "", "status": "completed"},
                {"id": "T2", "title": "b", "description": "", "status": "pending"}
            ],
            "mode": "manual",
            "current_id": null,
            "auto_turns": 0,
            "max_auto_turns": 24
        });
        let path = todo_path().expect("test ctx has session");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, legacy.to_string()).unwrap();
        exec_todo_create(&serde_json::json!({"items": [{"title": "c"}]}), false).unwrap();
        let store = read_store().unwrap();
        assert_eq!(store.items.last().unwrap().id, "T3");
        assert_eq!(store.next_id, 4);
    });
}

#[test]
fn set_edits_title_description_without_status() {
    // 纯编辑：updates 项可省略 status，仅改 title/description。
    with_isolated_todo(|_session| {
        exec_todo_create(
            &serde_json::json!({"items": [{"title": "原始", "description": "初版描述"}]}),
            false,
        )
        .unwrap();
        let result =
            exec_todo_set(&serde_json::json!({"updates": [{"id": "T1", "title": "改名", "description": "新验收标准"}]}))
                .unwrap();
        // 单条更新走 V1 兼容路径（item+message），非批量形态。
        assert!(result.contains("Todo T1 updated."));
        let store = read_store().unwrap();
        assert_eq!(store.items[0].title, "改名");
        assert_eq!(store.items[0].description, "新验收标准");
        assert_eq!(store.items[0].status, TodoStatus::Pending, "纯编辑不改状态");

        // description 传空串 = 显式清空；title 空串拒绝。
        exec_todo_set(&serde_json::json!({"updates": [{"id": "T1", "description": ""}]})).unwrap();
        assert_eq!(read_store().unwrap().items[0].description, "");
        assert!(
            exec_todo_set(&serde_json::json!({"updates": [{"id": "T1", "title": ""}]})).is_err()
        );
        // 什么都不改的条目拒绝。
        assert!(exec_todo_set(&serde_json::json!({"updates": [{"id": "T1"}]})).is_err());
    });
}

#[test]
fn set_edit_rejects_oversized_title() {
    with_isolated_todo(|_session| {
        exec_todo_create(&serde_json::json!({"items": [{"title": "a"}]}), false).unwrap();
        let long = "x".repeat(101);
        assert!(
            exec_todo_set(&serde_json::json!({"updates": [{"id": "T1", "title": long}]})).is_err()
        );
    });
}

// ═══════════════════════════════════════════════════════
// W1 拆分（PR-DT-1）：todo_write / todo_update / todo_list
// ═══════════════════════════════════════════════════════

use super::split::{handle_list, handle_update, handle_write};

fn parse_tool_result(text: &str) -> serde_json::Value {
    serde_json::from_str(text).unwrap()
}

fn split_ctx(_name: &str, args: Value) -> Value {
    args
}

#[test]
fn split_handlers_reject_cross_fields() {
    // 三件套各自白名单之外的字段一律 INVALID_INPUT（形态守卫）。
    assert!(
        handle_write(&split_ctx(
            "todo_write",
            serde_json::json!({"items": [{"title": "t"}], "id": "T1"})
        ))
        .error
        .is_some()
    );
    // 单条 title 便利形态已随聚合退役——write 只认 items。
    assert!(
        handle_write(&split_ctx("todo_write", serde_json::json!({"title": "t"})))
            .error
            .is_some()
    );
    // 单一形态：ids/updates 一律拒绝——一次一条。
    assert!(
        handle_update(&split_ctx(
            "todo_update",
            serde_json::json!({"ids": ["T1"], "status": "pending"})
        ))
        .error
        .is_some()
    );
    assert!(
        handle_update(&split_ctx(
            "todo_update",
            serde_json::json!({"updates": [{"id": "T1", "status": "pending"}]})
        ))
        .error
        .is_some()
    );
    assert!(
        handle_list(&split_ctx("todo_list", serde_json::json!({"ids": ["T1"]})))
            .error
            .is_some()
    );
}

#[test]
fn split_roundtrip_via_handlers() {
    with_isolated_todo(|_session| {
        // 首次覆写建 2 条（v4：status 必填，assigned 回填新分配的 ID）
        let first = parse_tool_result(
            handle_write(&split_ctx(
                "todo_write",
                serde_json::json!({"items": [
                    {"title": "a", "status": "pending"},
                    {"title": "b", "status": "in_progress"}
                ]}),
            ))
            .model_text(),
        );
        assert_eq!(first["assigned"], serde_json::json!(["T1", "T2"]));
        assert_eq!(first["total"], 2);
        // write 第二轮：全量覆写——替换而非追加；保留项带 id 引用，
        // 新项缺省 id 分配下一号（T3）。
        let second = parse_tool_result(
            handle_write(&split_ctx(
                "todo_write",
                serde_json::json!({"items": [
                    {"id": "T1", "title": "a", "status": "completed"},
                    {"id": "T2", "title": "b", "status": "in_progress"},
                    {"title": "c", "status": "pending"}
                ]}),
            ))
            .model_text(),
        );
        assert_eq!(second["total"], 3);
        assert_eq!(second["assigned"], serde_json::json!(["T3"]));
        assert_eq!(second["current_id"], "T2");
        assert_eq!(ids(&read_store().unwrap()), ["T1", "T2", "T3"]);
    });
}

#[test]
fn write_empty_clears_and_restarts_id_sequence() {
    with_isolated_todo(|_session| {
        handle_write(&split_ctx(
            "todo_write",
            serde_json::json!({"items": [{"title": "a", "status": "pending"}]}),
        ));
        // v4：空 items = 空清单（覆写语义的自然结果），next_id 不重置——
        // 下次写入从高水位继续分配，永不复用旧号。
        let cleared = parse_tool_result(
            handle_write(&split_ctx("todo_write", serde_json::json!({"items": []}))).model_text(),
        );
        assert_eq!(cleared["replaced"], 1);
        assert_eq!(cleared["total"], 0);
        assert!(read_store().unwrap().items.is_empty());
        assert_eq!(read_store().unwrap().next_id, 2);
        // 清空后的新写入不复用 T1：ID 唯一性跨覆写永续。
        let after = parse_tool_result(
            handle_write(&split_ctx(
                "todo_write",
                serde_json::json!({"items": [{"title": "fresh", "status": "pending"}]}),
            ))
            .model_text(),
        );
        assert_eq!(after["assigned"], serde_json::json!(["T2"]));
    });
}

#[test]
fn write_full_replace_updates_status_inline_and_reassigns_unknown_id() {
    with_isolated_todo(|_session| {
        // 首次覆写建 2 条，T2 in_progress（写即状态）。
        let first = parse_tool_result(
            handle_write(&split_ctx(
                "todo_write",
                serde_json::json!({"items": [
                    {"title": "a", "status": "pending"},
                    {"title": "b", "status": "in_progress"}
                ]}),
            ))
            .model_text(),
        );
        assert_eq!(first["current_id"], "T2");
        // 覆写翻转状态：T1 in_progress、T2 completed，不需要 update 往返。
        let flipped = parse_tool_result(
            handle_write(&split_ctx(
                "todo_write",
                serde_json::json!({"items": [
                    {"id": "T1", "title": "a", "status": "in_progress"},
                    {"id": "T2", "title": "b", "status": "completed", "evidence": "done"}
                ]}),
            ))
            .model_text(),
        );
        assert_eq!(flipped["current_id"], "T1");
        let store = read_store().unwrap();
        assert_eq!(store.items[1].status, TodoStatus::Completed);
        assert_eq!(store.items[1].evidence.as_deref(), Some("done"));
        // 引用未知 ID 不再整次失败：降级为「按新建处理 + 回执 remap」。
        let bogus = parse_tool_result(
            handle_write(&split_ctx(
                "todo_write",
                serde_json::json!({"items": [
                    {"id": "T9", "title": "ghost", "status": "pending"}
                ]}),
            ))
            .model_text(),
        );
        assert_eq!(bogus["total"], 1);
        assert_eq!(bogus["assigned"], serde_json::json!(["T3"]));
        assert!(
            bogus["message"].as_str().unwrap().contains("T9->T3"),
            "{bogus}"
        );
        assert_eq!(read_store().unwrap().items[0].id, "T3");
        // 缺 status 拒绝（覆写语义下状态必填）。
        let no_status = handle_write(&split_ctx(
            "todo_write",
            serde_json::json!({"items": [{"title": "x"}]}),
        ));
        assert!(no_status.error.is_some());
        // id 重复引用拒绝。
        let dup = handle_write(&split_ctx(
            "todo_write",
            serde_json::json!({"items": [
                {"id": "T1", "title": "a", "status": "pending"},
                {"id": "T1", "title": "again", "status": "pending"}
            ]}),
        ));
        assert!(dup.error.is_some());
    });
}

/// 只翻转状态、不重抄标题：`title` 省略时沿用同 id 既有条目的标题。
///
/// 实测里「漏写 title」是最高频的失败（整次覆写被拒，计划原地踏步），
/// 而模型的本意通常就是「T2 做完了」——既有条目里就有标题，没必要重抄。
#[test]
fn write_inherits_title_when_referencing_existing_id() {
    with_isolated_todo(|_session| {
        let seeded = handle_write(&split_ctx(
            "todo_write",
            serde_json::json!({"items": [
                {"title": "跑通后端契约", "status": "in_progress"},
                {"title": "补前端面板", "status": "pending"}
            ]}),
        ));
        assert!(seeded.error.is_none(), "{:?}", seeded.error);

        let flipped = parse_tool_result(
            handle_write(&split_ctx(
                "todo_write",
                serde_json::json!({"items": [
                    {"id": "T1", "status": "completed", "evidence": "done"},
                    {"id": "T2", "status": "in_progress"}
                ]}),
            ))
            .model_text(),
        );
        assert_eq!(flipped["current_id"], "T2");
        assert_eq!(flipped["assigned"], serde_json::json!([]));
        let store = read_store().unwrap();
        assert_eq!(store.items[0].title, "跑通后端契约");
        assert_eq!(store.items[1].title, "补前端面板");
        assert_eq!(store.items[0].status, TodoStatus::Completed);

        // 新条目没标题仍然拒绝：继承只对既有 id 成立，凭空建无标题条目没意义。
        let anonymous = handle_write(&split_ctx(
            "todo_write",
            serde_json::json!({"items": [{"status": "pending"}]}),
        ));
        assert!(anonymous.error.is_some());
        // 显式空标题视同省略——同样是「沿用既有」，不是「清空标题」。
        let blank = parse_tool_result(
            handle_write(&split_ctx(
                "todo_write",
                serde_json::json!({"items": [{"id": "T2", "title": "   ", "status": "in_progress"}]}),
            ))
            .model_text(),
        );
        assert_eq!(blank["total"], 1);
        assert_eq!(read_store().unwrap().items[0].title, "补前端面板");
    });
}

/// typed 桥的入参闸门：`title` 可选必须体现在 `TodoWriteArgs` 上。
///
/// 实测那次失败正是 `serde_json::from_value::<TodoWriteArgs>` 抛
/// `missing field 'title'` → 回执 `invalid arguments: ...`，压根走不到执行层。
/// 这条路径**不做 JSON schema 校验**，所以只改 schema 是修不掉的。
#[test]
fn typed_write_args_accept_missing_title() {
    let args: super::typed::TodoWriteArgs = serde_json::from_value(serde_json::json!({
        "items": [{"id": "T1", "status": "in_progress"}]
    }))
    .expect("title-less item must deserialize");
    assert!(args.items[0].title.is_none());
    assert!(args.items[0].description.is_none());
}

/// 端到端回归：`title` 缺失必须能穿过 typed 桥走到执行层。
///
/// 实测那次失败就是 `TypedToolAdapter` 里
/// `serde_json::from_value::<TodoWriteArgs>` 抛 `missing field 'title'` →
/// 回执 `invalid arguments: ...`，**压根没执行**。这条路径不做 JSON schema
/// 校验，所以只改 schema 是修不掉的——必须让 typed args 自己也收得下。
#[test]
fn typed_write_accepts_title_less_items_end_to_end() {
    use crate::tool_api::ErasedTool;

    with_isolated_todo(|session_id| {
        let run = |args: Value| {
            let mut ctx = crate::file_mutate::ambient_tool_context(
                "todo-typed",
                std::time::Duration::from_secs(15),
            );
            // `ambient_tool_context` 读的是另一套线程局部；这里显式钉住隔离会话。
            ctx.session_id = session_id.to_string();
            crate::tool_api::TypedToolAdapter::new(super::typed::TodoWriteTool)
                .execute(ctx, args)
                .unwrap_or_else(|fatal| panic!("todo_write fatal: {}", fatal.message))
        };

        let seeded = run(serde_json::json!({"items": [
            {"title": "跑通后端契约", "status": "in_progress"}
        ]}));
        assert_eq!(
            seeded.status,
            qaqh_types::ToolStatus::Ok,
            "seed failed: {:?}",
            seeded.error
        );

        // 只带 id + status：旧实现在这里就返回 invalid arguments。
        let flipped = run(serde_json::json!({"items": [
            {"id": "T1", "status": "completed"}
        ]}));
        assert_eq!(flipped.status, qaqh_types::ToolStatus::Ok);
        assert!(flipped.error.is_none(), "{:?}", flipped.error);
        let store = read_store().unwrap();
        assert_eq!(store.items[0].title, "跑通后端契约");
        assert_eq!(store.items[0].status, TodoStatus::Completed);
    });
}

#[test]
fn split_plan_blocked_and_conflict_semantics() {
    // 写类工具受 plan 阻断；todo_list 是读，plan 模式放行。
    for blocked in ["todo_write", "todo_update"] {
        assert!(
            crate::PLAN_BLOCKED.contains(&blocked),
            "{blocked} 应在 PLAN_BLOCKED"
        );
    }
    assert!(!crate::PLAN_BLOCKED.contains(&"todo_list"));
    // 冲突键：store 是单一资源，三件套共享合成键（同轮读写顺序约束）。
    for name in ["todo_list", "todo_update", "todo_write"] {
        let paths = crate::conflict::file_write_paths(name, &serde_json::json!({}));
        assert_eq!(paths, vec!["__qaqh_todo__".to_string()], "{name} 冲突键");
    }
}

#[test]
fn todo_v3_registered_and_legacy_names_retired() {
    let mgr = crate::registration::build_tool_manager(&[]);
    let names: Vec<String> = mgr
        .all_defs()
        .iter()
        .map(|d| d.function.name.clone())
        .collect();
    for name in ["todo_write", "todo_update", "todo_list"] {
        assert!(names.contains(&name.to_string()), "{name} 应已注册");
    }
    for legacy in ["todo", "todo_create", "todo_insert", "todo_set"] {
        assert!(
            !names.contains(&legacy.to_string()),
            "{legacy} 应已随 v3 形态替换退役"
        );
    }
}
