use super::*;

fn assistant_with_tool_call(id: &str) -> Message {
    Message {
        msg_id: None,
        role: Message::ROLE_ASSISTANT.to_owned(),
        name: None,
        content: vec![qaqh_types::ContentBlock::ToolUse {
            id: id.to_owned(),
            name: "read".to_owned(),
            input: serde_json::json!({"path": "a.txt"}),
        }],
    }
}

fn open_tool_use_present(store: &MessageStore, id: &str) -> bool {
    store
        .get_last_step_pending()
        .iter()
        .any(|pending| pending.id == id)
}

/// 取消路径的收尾原语：把「已执行但未回填结果」的整步摘掉，store 不得
/// 留 open tool_use（否则下轮模型重发同一 tool_use → 工具重复执行）。
#[test]
fn incomplete_step_is_removed_so_no_open_tool_use_survives() {
    let mut store = MessageStore::new("seed");
    store.push_user("go");
    store.push_assistant(assistant_with_tool_call("call-1"));
    assert!(
        open_tool_use_present(&store, "call-1"),
        "前置条件：assistant 步已带 open tool_use"
    );

    let removed = store.remove_last_step_if_incomplete();

    assert!(removed, "未回填的步必须被摘除");
    assert!(
        !open_tool_use_present(&store, "call-1"),
        "取消收尾后不得残留 open tool_use"
    );
}
