//! 跨 daemon 重启的 **wire 请求体** 逐字节一致性测试（chat completions）。
//!
//! 与 `restart_prefix_cache.rs` 的分工：那边断言的是内存里的
//! `Vec<Message>` 上下文；这边断言的是 **真正发出去的 HTTP 请求体字节**。
//! 二者不是同一件事——gate 会重排/合成消息（system 提顶、`name` 丢弃、
//! tool_call 派生、schema 清理），provider 的 `tools` / `max_tokens` /
//! `reasoning_effort` 等字段也都在 wire 上。前缀缓存吃的是后者，所以
//! 只有后者逐字节稳定，"重启后仍然命中缓存"才是可验证的事实。
//!
//! 断言：
//!
//! 1. 同一进程、同一上下文连续两次请求 → 请求体逐字节相同（排除 HashMap
//!    遍历序、时间戳等隐藏随机源）；
//! 2. 重建 `AgentState` + 从磁盘恢复（= daemon 重启）→ 请求体与重启前
//!    逐字节相同（system prompt / `[Environment]` 注解 / 工具清单全部
//!    来自持久化面，不是重启时重新生成）；
//! 3. 重启后追加一个回合 → 旧消息序列逐字节是新请求体的前缀，工具清单与
//!    请求参数不变（provider 前缀缓存的 append-only 合同）；
//! 4. 再重启一次仍然逐字节复现；
//! 5. 缩小工具清单 → 请求体必须变（证明前四条不是空转）。
//!
//! 走真实 HTTP：本地 `tiny_http` 假 provider 收请求体（装置在
//! `common/model_wire_capture.rs`），`chat_stream` 走完整生产路径。
//!
//! 单例约束：`SessionManager::init` 是进程级 OnceLock，二次 init 会 panic。
//! 本文件是独立测试二进制（独立进程），因此可以自己 init 一次；
//! responses 版本在 `restart_request_body_responses.rs`（另一个进程）。

mod common;

use std::sync::Mutex;

use common::model_wire_capture::{
    CaptureProvider, Wire, body_json, capture_body, create_agent, init_data_root, messages,
    resume_agent, session_turn_1, temp_root,
};
use qaqh_types::message::{ContentBlock, Message};

/// 串行化触碰共享进程状态（workspace 全局 + CURRENT_WORKSPACE）。
static SERIAL: Mutex<()> = Mutex::new(());

#[test]
fn request_body_is_byte_identical_across_restarts() {
    let _guard = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    let root = temp_root("chat");
    init_data_root(&root);

    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).expect("workspace dir");
    let ws = ws.to_string_lossy().into_owned();

    let provider = CaptureProvider::start();

    // ════ 生命周期 1：建会话、跑完首轮、落盘 ════
    let mut agent = create_agent(&root, &ws);
    let session_id = agent.session.session_id.clone();
    session_turn_1(&mut agent);
    agent.build_context();
    agent
        .msg
        .flush_meta(&agent.config.model, &agent.config.reasoning_effort);
    agent.drain_persist_ops();

    let first = capture_body(&mut agent, &provider, &session_id, Wire::Chat);

    // 断言 0（灵敏度）：请求体里必须真的有 system prompt、[Environment]
    // 注解、工具 schema 与工具结果——否则下面的相等断言是空转。
    let first_json = body_json(&first);
    let first_msgs = messages(&first_json);
    assert!(
        first_msgs
            .iter()
            .any(|m| m["role"] == "system" && m.to_string().chars().count() > 1000),
        "system prompt must be on the wire"
    );
    assert!(
        first_msgs
            .iter()
            .any(|m| m.to_string().contains("[Environment]")),
        "the frozen [Environment] annotation must be on the wire"
    );
    assert!(
        first_msgs
            .iter()
            .any(|m| m.to_string().contains("azure-falcon-42")),
        "tool result must be on the wire"
    );
    assert!(
        first_json["tools"]
            .as_array()
            .is_some_and(|tools| !tools.is_empty()),
        "tool defs must be on the wire"
    );
    let tools_on_the_wire = first_json["tools"].clone();

    // ════ 断言 1：同一进程、同一上下文 → 请求体逐字节相同 ════
    let second = capture_body(&mut agent, &provider, &session_id, Wire::Chat);
    assert_eq!(
        first, second,
        "two requests over the same context must serialize to identical bodies \
         (a difference here means a hidden nondeterministic source in the wire shape)"
    );

    // ════ 断言 2：daemon 重启（磁盘恢复）→ 请求体逐字节相同 ════
    drop(agent);
    let mut agent = resume_agent(&root, &ws, &session_id);
    let after_restart = capture_body(&mut agent, &provider, &session_id, Wire::Chat);
    assert_eq!(
        first, after_restart,
        "the wire body must survive a daemon restart byte-identically \
         (system prompt, [Environment] annotation and tool defs must come from \
          the persisted session, not be regenerated at resume)"
    );
    assert_eq!(
        body_json(&after_restart)["tools"],
        tools_on_the_wire,
        "tool defs must be part of the stable prefix"
    );

    // ════ 断言 3：重启后追加回合 → 旧消息是新请求体的字节前缀 ════
    agent.msg.push_user("second turn question");
    let _ = agent.msg.push_assistant(Message {
        msg_id: None,
        role: "assistant".into(),
        name: None,
        content: vec![ContentBlock::text("answered turn 2")],
    });
    agent.build_context();
    agent
        .msg
        .flush_meta(&agent.config.model, &agent.config.reasoning_effort);
    agent.drain_persist_ops();
    let grown = capture_body(&mut agent, &provider, &session_id, Wire::Chat);

    let grown_json = body_json(&grown);
    let after_restart_json = body_json(&after_restart);
    let old_msgs = messages(&after_restart_json);
    let new_msgs = messages(&grown_json);
    assert!(
        new_msgs.len() > old_msgs.len(),
        "appending a turn must grow the wire messages"
    );
    assert_eq!(
        body_json(&after_restart)["tools"],
        grown_json["tools"],
        "tools must not be re-ordered or re-rendered by appending a turn"
    );
    for (index, old) in old_msgs.iter().enumerate() {
        assert_eq!(
            old, &new_msgs[index],
            "message {index} changed after appending a turn — prefix cache broken"
        );
    }
    // 数组字节前缀：messages 是请求体里的一段连续 JSON，旧数组去掉结尾 `]`
    // 后必须逐字节等于新数组的开头。
    let old_bytes = serde_json::to_vec(old_msgs).expect("serialize old messages");
    let new_bytes = serde_json::to_vec(new_msgs).expect("serialize new messages");
    assert!(
        new_bytes.starts_with(&old_bytes[..old_bytes.len() - 1]),
        "the serialized messages array must extend the previous one byte-for-byte"
    );

    // ════ 断言 4：追加后再重启一次 → 仍然逐字节复现 ════
    drop(agent);
    let mut agent = resume_agent(&root, &ws, &session_id);
    let after_second_restart = capture_body(&mut agent, &provider, &session_id, Wire::Chat);
    assert_eq!(
        grown, after_second_restart,
        "the appended body must survive another restart byte-identically"
    );

    // ════ 断言 5（灵敏度）：缩小工具清单 → 请求体必须变 ════
    // 证明断言 1–4 的相等不是"沙箱里什么都比不出来"：`tools` 是缓存前缀的
    // 第一段，少一个工具就必须改字节。前四条只能证明「没变」，这条证明
    // 「该变的时候看得见」。
    drop(agent);
    let mut narrowed = resume_agent(&root, &ws, &session_id);
    narrowed.tool_defs.truncate(3);
    let narrowed_body = capture_body(&mut narrowed, &provider, &session_id, Wire::Chat);
    assert_ne!(
        after_second_restart, narrowed_body,
        "dropping tool defs must change the wire body — otherwise the byte-equality \
         assertions above are vacuous"
    );

    drop(narrowed);
    let _ = qaqh_session::SessionManager::global().delete(&session_id);
    let _ = std::fs::remove_dir_all(root);
}
