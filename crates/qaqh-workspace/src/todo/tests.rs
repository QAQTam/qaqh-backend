//! todo 测试（纯文本范式）：title 即身份、无 ID 分配、并行 in_progress、
//! meta 快照状态机。行格式解析的单元测试在 parse.rs；本文件锁工具面行为。
//!
//! v4.5 P2-1b：宿主是**每用例自建的 MemoryTodoHost**（显式传参，无全局槽），
//! 用例天然隔离、可并发。

use super::actions::todo_set_for;
use super::model::{TodoStatus, TodoStore};
use super::store::{load_todo_for, todo_meta_snapshot};
use serde_json::Value;

// ═══════════════════════════════════════════════════════
// harness（v4.5 P2-1b：每用例独立内存宿主 + 唯一 session key——旧的
// QAQH_DATA_DIR 环境隔离、全局槽与全局锁串行化整体退场，用例可并发跑）
// ═══════════════════════════════════════════════════════

static SESSION_COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn with_isolated_todo<F: FnOnce(&std::sync::Arc<super::host::MemoryTodoHost>, &str)>(f: F) {
    let n = SESSION_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let host = std::sync::Arc::new(super::host::MemoryTodoHost::default());
    let session_id = format!("test-seed-{n}");
    f(&host, &session_id);
}

fn parse(result: &Result<String, String>) -> serde_json::Value {
    serde_json::from_str(result.as_ref().unwrap()).unwrap()
}

fn titles(store: &TodoStore) -> Vec<String> {
    store.items.iter().map(|item| item.title.clone()).collect()
}

/// Ok = 输出信封；Err = typed 的 `json_err_string` 信封（code/message/hint）。
fn typed_write(
    host: &super::host::MemoryTodoHost,
    session_id: &str,
    args: Value,
) -> Result<serde_json::Value, serde_json::Value> {
    super::typed::todo_write_for_typed(host, session_id, &args)
        .and_then(|out| out.to_envelope_value())
        .map_err(|error| parse_error_envelope(&error))
}

fn typed_update(
    host: &super::host::MemoryTodoHost,
    session_id: &str,
    args: Value,
) -> Result<serde_json::Value, serde_json::Value> {
    todo_set_for(host, session_id, &args)
        .map(|text| parse_tool_result(&text))
        .map_err(|error| parse_error_envelope(&error))
}

fn write_list(host: &super::host::MemoryTodoHost, session_id: &str, text: &str) -> serde_json::Value {
    typed_write(host, session_id, serde_json::json!({ "list": text }))
        .unwrap_or_else(|error| panic!("todo_write 不得失败: {error}"))
}

fn update_title(
    host: &super::host::MemoryTodoHost,
    session_id: &str,
    title: &str,
    status: &str,
) -> serde_json::Value {
    typed_update(host, session_id, serde_json::json!({ "title": title, "status": status }))
        .unwrap_or_else(|error| panic!("todo_update {title}→{status} 不得失败: {error}"))
}

fn parse_tool_result(text: &str) -> serde_json::Value {
    serde_json::from_str(text).unwrap()
}

/// 错误通道一般是 `json_err_string` 信封；不是 JSON 就原样包一层，让断言读得到
/// code/message，而不是在 unwrap 上炸掉用例本身。
fn parse_error_envelope(raw: &str) -> serde_json::Value {
    serde_json::from_str(raw)
        .unwrap_or_else(|_| serde_json::json!({ "code": "tool_error", "message": raw }))
}

// ═══════════════════════════════════════════════════════
// ① write：全量文本覆写
// ═══════════════════════════════════════════════════════

#[test]
fn write_parses_the_full_text_block() {
    with_isolated_todo(|host, session_id| {
        let out = write_list(host, session_id, "- [ ] alpha | 描述 A\n- [~] beta\n- [x] gamma | | verified");
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
    with_isolated_todo(|host, session_id| {
        assert!(
            typed_write(host, session_id, serde_json::json!({"list": "- [ ] same\n- [x] same"})).is_err(),
            "重复标题必须拒绝"
        );
    });
}

#[test]
fn write_empty_clears_the_list() {
    with_isolated_todo(|host, session_id| {
        write_list(host, session_id, "- [ ] a");
        let cleared = write_list(host, session_id, "");
        assert_eq!(cleared["total"], 0);
        assert!(load_todo_for(&**host, session_id).unwrap().items.is_empty());
        assert_eq!(todo_meta_snapshot(&**host, session_id), None, "空列表不注入");
    });
}

#[test]
fn write_rebuild_replaces_instead_of_appending() {
    with_isolated_todo(|host, session_id| {
        write_list(host, session_id, "- [ ] a\n- [~] b");
        let out = write_list(host, session_id, "- [x] a\n- [~] c");
        assert_eq!(out["total"], 2);
        let store = load_todo_for(&**host, session_id).unwrap();
        assert_eq!(titles(&store), ["a", "c"], "未引用的 b 随覆写消失");
        assert_eq!(store.items[0].status, TodoStatus::Completed);
    });
}

// ═══════════════════════════════════════════════════════
// ② update：按 title 精确翻转 + 并行 in_progress
// ═══════════════════════════════════════════════════════

#[test]
fn parallel_in_progress_is_allowed() {
    with_isolated_todo(|host, session_id| {
        write_list(host, session_id, "- [~] a\n- [~] b\n- [ ] c");
        let store = load_todo_for(&**host, session_id).unwrap();
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
    with_isolated_todo(|host, session_id| {
        write_list(host, session_id, "- [~] a\n- [ ] b");
        let after = update_title(host, session_id, "a", "completed");
        assert_eq!(after["item"]["status"], "completed");
        let items = after["items"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["status"], "completed");
        assert_eq!(items[1]["status"], "pending");
    });
}

#[test]
fn update_unknown_title_lists_the_current_list() {
    with_isolated_todo(|host, session_id| {
        write_list(host, session_id, "- [ ] real-task");
        let error = typed_update(host, session_id, serde_json::json!({
            "title": "real task", "status": "completed"
        }))
        .expect_err("近似标题必须 not_found");
        assert_eq!(error["code"], "not_found");
        // 提示里带当前清单——模型可自行纠正拼写。
        assert!(
            error["message"]
                .as_str()
                .unwrap_or_default()
                .contains("real-task")
                || error["hint"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("real-task"),
            "错误面必须列出可用标题: {error:?}"
        );
    });
}

// ═══════════════════════════════════════════════════════
// ③ meta 快照状态机（注入契约）
// ═══════════════════════════════════════════════════════

#[test]
fn meta_snapshot_none_without_a_list() {
    with_isolated_todo(|host, session_id| {
        assert_eq!(todo_meta_snapshot(&**host, session_id), None, "无列表不注入");
        write_list(host, session_id, "");
        // 空列表同样不注入（零 token）。
        assert_eq!(todo_meta_snapshot(&**host, session_id), None);
    });
}

#[test]
fn meta_snapshot_compact_line_names_the_active_task() {
    with_isolated_todo(|host, session_id| {
        write_list(host, session_id, "- [x] first\n- [~] second\n- [ ] third");
        let snapshot = todo_meta_snapshot(&**host, session_id).expect("活跃列表必须注入");
        assert!(snapshot.contains("1/3 done"), "{snapshot}");
        assert!(snapshot.contains("now: second"), "{snapshot}");
        assert!(snapshot.contains("in_progress"), "{snapshot}");
        // 紧凑式不携带完整列表（token 经济）——只有一行。
        assert_eq!(snapshot.lines().count(), 1);
    });
}

#[test]
fn meta_snapshot_all_done_notifies_once() {
    with_isolated_todo(|host, session_id| {
        write_list(host, session_id, "- [~] a\n- [ ] b");
        update_title(host, session_id, "a", "completed");
        update_title(host, session_id, "b", "cancelled");
        // 全部终态：下一次快照回一次 cleared 并置归档位。
        let cleared = todo_meta_snapshot(&**host, session_id).expect("全 done 必须回一次 cleared");
        assert!(cleared.contains("all tasks done"), "{cleared}");
        assert!(load_todo_for(&**host, session_id).unwrap().archived, "归档位必须置位");
        // 之后每次快照都是 None（"下一次工具请求时置空而非挂旧列表"）。
        assert_eq!(todo_meta_snapshot(&**host, session_id), None);
        assert_eq!(todo_meta_snapshot(&**host, session_id), None);
    });
}

#[test]
fn meta_snapshot_unarchives_on_rebuild() {
    with_isolated_todo(|host, session_id| {
        write_list(host, session_id, "- [x] a | | done");
        // write 直接建全完成列表：立即归档，首次快照回 cleared 一次。
        let cleared = todo_meta_snapshot(&**host, session_id).expect("首次快照回 cleared");
        assert!(cleared.contains("all tasks done"), "{cleared}");
        assert_eq!(todo_meta_snapshot(&**host, session_id), None);

        // 下一次 todo_write 全量重建：归档解除，注入恢复。
        write_list(host, session_id, "- [~] fresh");
        assert!(!load_todo_for(&**host, session_id).unwrap().archived);
        let snapshot = todo_meta_snapshot(&**host, session_id).expect("重建后恢复注入");
        assert!(snapshot.contains("now: fresh"), "{snapshot}");
    });
}

// ═══════════════════════════════════════════════════════
// 形态守卫（文本范式 + 双工具收敛）
// ═══════════════════════════════════════════════════════

#[test]
fn typed_entries_reject_cross_fields() {
    with_isolated_todo(|host, session_id| {
        // write 只认 `list`：旧 items 形态与顶层便利字段一律拒绝
        // （形态锁 = TodoWriteArgs 的 deny_unknown_fields，不再有第二层白名单）。
        assert!(typed_write(host, session_id, serde_json::json!({"list": "- [ ] t", "items": []})).is_err());
        assert!(
            typed_write(host, session_id, serde_json::json!({"items": [{"title": "t", "status": "pending"}]}))
                .is_err()
        );
        assert!(typed_write(host, session_id, serde_json::json!({"title": "t"})).is_err());
        // update 只认 {title, status, evidence?}：ids/updates/id 一律拒绝。
        assert!(typed_update(host, session_id, serde_json::json!({"ids": ["x"], "status": "pending"})).is_err());
        assert!(typed_update(host, session_id, serde_json::json!({"id": "x", "status": "pending"})).is_err());
        assert!(
            typed_update(host, session_id, serde_json::json!({"updates": [{"title": "x", "status": "pending"}]}))
                .is_err()
        );
    });
}

#[test]
fn v1_v2_action_vocabulary_retired() {
    with_isolated_todo(|host, session_id| {
        for verb in [
            "create",
            "insert",
            "set",
            "list",
            "create_batch",
            "update",
            "cancel",
        ] {
            assert!(
                typed_write(host, session_id, serde_json::json!({ "action": verb, "list": "" })).is_err(),
                "action={verb} 必须返回错误"
            );
        }
    });
}

#[test]
fn session_parameterized_set_and_list_hit_explicit_session() {
    // HTTP service 面 / CLI 直访契约：显式 seed 读写与工具路径写同一份
    // todo.json（todo_list 工具退役后 service 面保留）。
    with_isolated_todo(|host, session_id| {
        write_list(host, session_id, "- [ ] 工具路径建项");
        let set = parse(&todo_set_for(
            &**host,
            session_id,
            &serde_json::json!({"title": "工具路径建项", "status": "completed", "evidence": "via CLI"}),
        ));
        assert_eq!(set["item"]["status"], "completed");
        let listed = parse(&super::actions::todo_list_for(
            &**host,
            session_id,
            &serde_json::json!({}),
        ));
        assert_eq!(listed["items"].as_array().unwrap().len(), 1);
        assert_eq!(listed["items"][0]["status"], "completed");
    });
}

#[test]
fn set_status_never_erases_metadata() {
    with_isolated_todo(|host, session_id| {
        write_list(host, session_id, "- [ ] Keep me | Keep this too");
        typed_update(host, session_id, serde_json::json!({
            "title": "Keep me", "status": "in_progress"
        }))
        .unwrap();
        typed_update(host, session_id, serde_json::json!({
            "title": "Keep me", "status": "completed", "evidence": "verified"
        }))
        .unwrap();
        let store = load_todo_for(&**host, session_id).unwrap();
        assert_eq!(store.items[0].title, "Keep me");
        assert_eq!(store.items[0].description, "Keep this too");
        assert_eq!(store.items[0].evidence.as_deref(), Some("verified"));
        assert_eq!(store.items[0].status, TodoStatus::Completed);
    });
}

#[test]
fn legacy_t_id_store_still_loads() {
    // 旧 T<n> 代清单（id != title）的行为契约：title 仍是身份，id 原样保留。
    // 「旧文件读进来」那一层已收编进启动吸入，由
    // `qaqh-session/tests/sidecar_ingestion.rs` 钉住；这里只验证宿主形态下
    // 的读写语义（内存宿主直接 seed 状态，不再有文件写路径）。
    with_isolated_todo(|host, session_id| {
        let legacy = TodoStore {
            items: vec![
                super::model::TodoItem {
                    id: "T1".to_string(),
                    title: "legacy-a".to_string(),
                    description: String::new(),
                    status: TodoStatus::Pending,
                    evidence: None,
                },
                super::model::TodoItem {
                    id: "T2".to_string(),
                    title: "legacy-b".to_string(),
                    description: String::new(),
                    status: TodoStatus::Pending,
                    evidence: None,
                },
            ],
            mode: Default::default(),
            current_id: None,
            auto_turns: 0,
            max_auto_turns: 24,
            archived: false,
            cleared_notified: false,
        };
        super::host::TodoHost::save_todo(&**host, session_id, &legacy).expect("seed legacy store");
        update_title(host, session_id, "legacy-a", "in_progress");
        let store = load_todo_for(&**host, session_id).unwrap();
        assert_eq!(store.items[0].status, TodoStatus::InProgress);
        assert_eq!(titles(&store), ["legacy-a", "legacy-b"]);
        assert_eq!(store.items[0].id, "T1", "id 必须原样保留");
    });
}

#[test]
fn concurrent_updates_all_take_effect() {
    // 原子性判据（内存宿主镜像；真 DB 形态钉在
    // `qaqh-session/tests/todos_atomicity.rs`）：同一清单 8 个并发
    // todo_update 交错 RMW，全部生效、不得有条目被旧快照覆盖回 pending。
    with_isolated_todo(|host, session_id| {
        write_list(host, session_id, "- [ ] t1\n- [ ] t2\n- [ ] t3\n- [ ] t4");
        let session_id = session_id.to_string();
        let workers: Vec<_> = (0..8)
            .map(|worker| {
                let sid = session_id.clone();
                let host = std::sync::Arc::clone(host);
                std::thread::spawn(move || {
                    let title = format!("t{}", (worker % 4) + 1);
                    let status = if worker < 4 { "completed" } else { "cancelled" };
                    super::typed::todo_update_for_typed(
                        &*host,
                        &sid,
                        &serde_json::json!({ "title": title, "status": status }),
                    )
                })
            })
            .collect();
        for worker in workers {
            worker.join().expect("worker").expect("update ok");
        }
        let store = load_todo_for(&**host, &session_id).unwrap();
        assert_eq!(store.items.len(), 4);
        assert!(
            store.items.iter().all(|item| matches!(
                item.status,
                TodoStatus::Completed | TodoStatus::Cancelled
            )),
            "8 个并发 update 必须全部生效: {:?}",
            store
                .items
                .iter()
                .map(|item| (item.title.clone(), item.status.clone()))
                .collect::<Vec<_>>()
        );
    });
}
