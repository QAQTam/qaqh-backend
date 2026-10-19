//! 跨 daemon 重启的 **wire 请求体** 逐字节一致性测试（Responses wire）。
//!
//! 为什么和 chat 版分开：`SessionManager::init` 是进程级 OnceLock，两个测试
//! 不能同进程；而 Responses 是 DeepSeek 账号要迁过去的面（「对话中间插入
//! 工具调用」只有它和 Anthropic 面支持），必须独立锁住它的字节形状。
//!
//! 断言：
//!
//! 1. 历史中间的工具调用在 wire 上表达为 `function_call` + `function_call_output`
//!    item，且**后面还有对话**（chat completions 表达不了这种形状）；
//! 2. 无状态：不下发 `previous_response_id`，`store` 恒为 false；
//! 3. 重启前后请求体逐字节相同；
//! 4. 追加回合后 `input` 数组逐字节延长（前缀缓存合同），`tools` 不变；
//! 5. 再重启一次仍然逐字节复现；缩小工具清单则必须变（灵敏度）。

mod common;

use std::sync::Mutex;

use common::model_wire_capture::{
    CaptureProvider, Wire, body_json, capture_body, create_agent, init_data_root, input_items,
    resume_agent, session_turn_1, temp_root,
};
use qaqh_types::message::{ContentBlock, Message};

/// 串行化触碰共享进程状态（workspace 全局 + CURRENT_WORKSPACE）。
static SERIAL: Mutex<()> = Mutex::new(());

fn text_block(text: &str) -> Message {
    Message {
        msg_id: None,
        role: "assistant".into(),
        name: None,
        content: vec![ContentBlock::text(text)],
    }
}

fn index_of(items: &[serde_json::Value], matches: impl Fn(&serde_json::Value) -> bool) -> usize {
    items
        .iter()
        .position(matches)
        .expect("expected wire item missing")
}

#[test]
fn responses_request_body_is_byte_identical_across_restarts() {
    let _guard = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    let root = temp_root("responses");
    init_data_root(&root);

    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).expect("workspace dir");
    let ws = ws.to_string_lossy().into_owned();

    let provider = CaptureProvider::start();

    // ════ 生命周期 1：首轮工具调用 + 之后再来一轮（工具调用落在历史中间）════
    let mut agent = create_agent(&root, &ws);
    let session_id = agent.session.session_id.clone();
    session_turn_1(&mut agent);
    agent.msg.push_user("second turn after the tool call");
    let _ = agent.msg.push_assistant(text_block("answered turn 2"));
    agent.build_context();
    agent
        .msg
        .flush_meta(&agent.config.model, &agent.config.reasoning_effort);
    agent.drain_persist_ops();

    let first = capture_body(&mut agent, &provider, &session_id, Wire::Responses);

    let first_json = body_json(&first);
    // 断言 2：无状态请求的形状。
    assert_eq!(first_json["store"], serde_json::json!(false));
    assert!(
        first_json.get("previous_response_id").is_none(),
        "DeepSeek Responses 是无状态 API，不得下发 previous_response_id"
    );
    // 断言 1：中间工具调用的 item 形状与顺序。
    let items = input_items(&first_json);
    let call_at = index_of(items, |item| {
        item["type"] == "function_call" && item["call_id"] == "call-1"
    });
    let output_at = index_of(items, |item| {
        item["type"] == "function_call_output" && item["call_id"] == "call-1"
    });
    assert!(
        call_at < output_at,
        "function_call must precede its function_call_output"
    );
    let later_at = index_of(items, |item| {
        item.to_string().contains("second turn after the tool call")
    });
    assert!(
        output_at < later_at,
        "the tool call must sit in the middle of history — chat completions \
         cannot express this shape, Responses can"
    );
    assert!(
        first_json["tools"]
            .as_array()
            .is_some_and(|tools| !tools.is_empty()),
        "tool defs must be on the wire"
    );
    let tools_on_the_wire = first_json["tools"].clone();

    // ════ 断言 3：daemon 重启（磁盘恢复）→ 请求体逐字节相同 ════
    drop(agent);
    let mut agent = resume_agent(&root, &ws, &session_id);
    let after_restart = capture_body(&mut agent, &provider, &session_id, Wire::Responses);
    assert_eq!(
        first, after_restart,
        "the Responses wire body must survive a daemon restart byte-identically"
    );

    // ════ 断言 4：追加回合 → input 数组逐字节延长，tools 不变 ════
    agent.msg.push_user("third turn question");
    let _ = agent.msg.push_assistant(text_block("answered turn 3"));
    agent.build_context();
    agent
        .msg
        .flush_meta(&agent.config.model, &agent.config.reasoning_effort);
    agent.drain_persist_ops();
    let grown = capture_body(&mut agent, &provider, &session_id, Wire::Responses);

    let after_restart_json = body_json(&after_restart);
    let grown_json = body_json(&grown);
    let old_items = input_items(&after_restart_json);
    let new_items = input_items(&grown_json);
    assert!(
        new_items.len() > old_items.len(),
        "appending a turn must grow the wire input"
    );
    assert_eq!(
        after_restart_json["tools"], grown_json["tools"],
        "tools must not be re-ordered or re-rendered by appending a turn"
    );
    for (index, old) in old_items.iter().enumerate() {
        assert_eq!(
            old, &new_items[index],
            "input item {index} changed after appending a turn — prefix cache broken"
        );
    }
    let old_bytes = serde_json::to_vec(old_items).expect("serialize old input");
    let new_bytes = serde_json::to_vec(new_items).expect("serialize new input");
    assert!(
        new_bytes.starts_with(&old_bytes[..old_bytes.len() - 1]),
        "the serialized input array must extend the previous one byte-for-byte"
    );

    // ════ 断言 5：再重启一次仍然逐字节复现；缩小工具清单必须变 ════
    drop(agent);
    let mut agent = resume_agent(&root, &ws, &session_id);
    let after_second_restart = capture_body(&mut agent, &provider, &session_id, Wire::Responses);
    assert_eq!(
        grown, after_second_restart,
        "the appended Responses body must survive another restart byte-identically"
    );

    drop(agent);
    let mut narrowed = resume_agent(&root, &ws, &session_id);
    narrowed.tool_defs.truncate(3);
    let narrowed_body = capture_body(&mut narrowed, &provider, &session_id, Wire::Responses);
    assert_ne!(
        after_second_restart, narrowed_body,
        "dropping tool defs must change the wire body — otherwise the byte-equality \
         assertions above are vacuous"
    );

    drop(narrowed);
    let _ = qaqh_session::SessionManager::global().delete(&session_id);
    let _ = std::fs::remove_dir_all(root);
}
