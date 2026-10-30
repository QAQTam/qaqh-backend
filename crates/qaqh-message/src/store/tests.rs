use super::*;
use qaqh_types::ContentBlock;

fn assistant_with_tools(tools: &[(&str, &str)]) -> Message {
    Message {
        msg_id: None,
        role: "assistant".to_string(),
        name: None,
        content: tools
            .iter()
            .map(|(id, name)| ContentBlock::ToolUse {
                id: (*id).to_string(),
                name: (*name).to_string(),
                input: serde_json::json!({}),
            })
            .collect(),
    }
}

fn context_result(context: &[Message], id: &str) -> String {
    context
        .iter()
        .flat_map(|message| &message.content)
        .find_map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id,
                result,
                ..
            } if tool_use_id == id => Some(result.model_text().to_string()),
            _ => None,
        })
        .expect("tool result must be present in gate context")
}

/// 模型侧工具结果信封（gate 投影原样渲染）。
fn context_envelope(context: &[Message], id: &str) -> String {
    context
        .iter()
        .flat_map(|message| &message.content)
        .find_map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id,
                result,
                ..
            } if tool_use_id == id => Some(result.render_xml_envelope()),
            _ => None,
        })
        .expect("tool result must be present in gate context")
}

/// 词元钱包读数：工具结果在**落盘那一刻**盖戳，之后不再是动态值。
///
/// 两条语义一起锁住：
/// 1. 没有 API 读数时信封与历史逐字节一致（不凭空写 0%）；
/// 2. 有新读数后，**只有新落盘的结果**带字段，已落盘的历史不漂移——
///    否则每开一个回合就把整条历史改写一遍，前缀缓存全灭。
#[test]
fn tool_results_carry_the_api_context_reading_only_when_stamped() {
    qaqh_types::clear_api_context_usage();
    let mut store = MessageStore::new_ephemeral("context-stamp");
    store.push_user("inspect files");
    store.push_assistant(assistant_with_tools(&[("read-1", "read")]));
    store.push_tool_result_direct("read-1", "READ_RESULT", true);

    let context = store.build_context_for_gate(&[]);
    assert_eq!(
        context_envelope(&context, "read-1"),
        "<qaqh_tool_result status=\"ok\">\nREAD_RESULT\n</qaqh_tool_result>",
        "无读数时信封必须与历史逐字节一致"
    );

    // 模拟 gate 回报：本轮输入 375k / 1M 窗口（四舍五入 → 38%）。
    qaqh_types::note_api_context_usage(375_000, 1_000_000);
    store.push_user("run it");
    store.push_assistant(assistant_with_tools(&[("exec-1", "exec")]));
    store.push_tool_result_direct("exec-1", "EXEC_RESULT", true);

    let context = store.build_context_for_gate(&[]);
    assert_eq!(
        context_envelope(&context, "exec-1"),
        "<qaqh_tool_result status=\"ok\" context_used=\"38%\" context_window=\"1000000\">\nEXEC_RESULT\n</qaqh_tool_result>"
    );
    assert_eq!(
        context_envelope(&context, "read-1"),
        "<qaqh_tool_result status=\"ok\">\nREAD_RESULT\n</qaqh_tool_result>",
        "已落盘的历史结果不得因新读数漂移"
    );
    qaqh_types::clear_api_context_usage();
}

#[test]
fn push_image_externally_stores_image_ref_with_disk_backing() {
    let prev = std::env::var("QAQH_DATA_DIR").ok();
    let tmp = std::env::temp_dir().join(format!("qaqh-store-img-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    // SAFETY: 单元测试共享进程环境；本用例前后成对设置/恢复该变量，
    // 且 message store 只在此处读取数据根，不存在并发读者。
    unsafe { std::env::set_var("QAQH_DATA_DIR", &tmp) };

    let b64 = "aGVsbG8gaW1hZ2U=";
    let mut store = MessageStore::new_ephemeral("img-test");
    store.push_user("look at this");
    store.push_image_to_last_user("image/png", b64);

    // 消息内只留 ImageRef 索引，bytes_len 与 base64 文本长度一致。
    let turn = store.turns.last().expect("turn");
    let sha = qaqh_types::image_store::sha256_hex(b64.as_bytes());
    match &turn.user.content[1] {
        ContentBlock::ImageRef {
            sha256,
            mime_type,
            bytes_len,
        } => {
            assert_eq!(mime_type, "image/png");
            assert_eq!(*bytes_len, b64.len());
            assert_eq!(sha256, &sha);
        }
        other => panic!("expected ImageRef, got {other:?}"),
    }
    // 字节确实落盘（同一 data root，内容寻址可读回）。
    assert_eq!(
        qaqh_types::image_store::load_image_b64(&sha, "image/png").expect("disk read"),
        b64
    );

    // 空载荷：store_image_b64 拒绝 → 回退 inline Image（不丢数据）。
    store.push_image_to_last_user("image/png", "");
    let turn = store.turns.last().expect("turn");
    assert!(
        matches!(&turn.user.content[2], ContentBlock::Image { .. }),
        "empty payload must fall back to inline Image"
    );

    match prev {
        // SAFETY: 恢复进入本用例前的数据根（与上方 set_var 成对）。
        Some(v) => unsafe { std::env::set_var("QAQH_DATA_DIR", v) },
        // SAFETY: 原本未设置 → 恢复为未设置状态。
        None => unsafe { std::env::remove_var("QAQH_DATA_DIR") },
    }
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn system_input_roundtrip_preserves_mid_stream_system_message() {
    let mut store = MessageStore::new_ephemeral("test");
    store.push_user("first user turn");
    store.push_assistant(Message {
        msg_id: None,
        role: "assistant".to_string(),
        name: None,
        content: vec![ContentBlock::text("first answer")],
    });
    store.push_system_input("[SUBAGENT 'explore' COMPLETED]\n\nfinal answer here");
    store.push_user("second user turn");
    store.push_assistant(Message {
        msg_id: None,
        role: "assistant".to_string(),
        name: None,
        content: vec![ContentBlock::text("second answer")],
    });

    // to_vec: mid-stream system message keeps its role and position.
    let vec = store.to_vec();
    let roles: Vec<&str> = vec.iter().map(|m| m.role.as_str()).collect();
    assert_eq!(
        roles,
        vec!["user", "assistant", "system", "user", "assistant"]
    );
    assert!(vec[2]
        .content
        .iter()
        .any(|b| matches!(b, ContentBlock::Text { text } if text.starts_with("[SUBAGENT 'explore' COMPLETED]"))));

    // from_messages replay restores the [SUBAGENT ...] injection as a
    // TRAILING message（wire 合法位置），而非 mid-stream turn
    // （function_call_output 之后固化的非法位置曾导致 provider 丢弃），
    // 且保持其写入顺序位置（位于两个真实 turn 之间）。
    let (restored, repairs) = MessageStore::from_messages("test", &vec, 0);
    assert!(
        repairs.is_empty(),
        "replay should not report repairs: {repairs:?}"
    );
    let restored_vec = restored.to_vec();
    let restored_roles: Vec<&str> = restored_vec.iter().map(|m| m.role.as_str()).collect();
    // to_vec 是权威快照：trailing 注入必须保持写入顺序位置（此前 to_vec
    // 把 trailing 一律钉在末尾，重启全量快照会丢失注入的原始时间位置）。
    assert_eq!(
        restored_roles,
        vec!["user", "assistant", "system", "user", "assistant"]
    );
    let trailing = restored.trailing_messages();
    assert_eq!(
        trailing.len(),
        1,
        "subagent injection must restore as trailing"
    );
    assert!(trailing[0]
        .content
        .iter()
        .any(|b| matches!(b, ContentBlock::Text { text } if text.starts_with("[SUBAGENT 'explore' COMPLETED]"))));
}

#[test]
fn write_order_merge_interleaves_injection_into_history() {
    // 复现线上症状的回归测试：子代理报告在回合中途落盘后，必须进入历史
    // （位于其 ack 之前、后续用户消息之前），而不是永远钉在请求末尾被
    // 模型当作"新发的 user 消息"。
    let mut store = MessageStore::new("seed");
    store.push_user("帮我调研 X");
    store.push_assistant(Message {
        msg_id: None,
        role: "assistant".to_string(),
        name: None,
        content: vec![ContentBlock::text("好的，我来调研")],
    });

    // 子代理报告在回合中途到达 → trailing 注入（user 角色 + name=subagent）。
    let mut inject = Message::user("[SUBAGENT 'explore' COMPLETED]\n\n调研结果：X 是 Y");
    inject.name = Some("subagent".into());
    assert!(store.push_trailing_system(inject));

    // 模型对报告的确认成为同一 turn 的后续 step。
    store.push_assistant(Message {
        msg_id: None,
        role: "assistant".to_string(),
        name: None,
        content: vec![ContentBlock::text("收到报告，已纳入调研")],
    });
    // 用户随后发送新消息。
    store.push_user("继续");

    let ctx = store.build_context_for_gate(&[]);
    let texts: Vec<String> = ctx
        .iter()
        .map(|m| {
            m.content
                .iter()
                .find_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.clone()),
                    _ => None,
                })
                .unwrap_or_default()
        })
        .collect();
    assert_eq!(
        texts,
        vec![
            "帮我调研 X",
            "好的，我来调研",
            "[SUBAGENT 'explore' COMPLETED]\n\n调研结果：X 是 Y",
            "收到报告，已纳入调研",
            "继续",
        ],
        "注入必须按写入顺序位于其 ack 与后续用户消息之前"
    );
    let report = ctx
        .iter()
        .find(|m| {
            m.content.iter().any(
                |b| matches!(b, ContentBlock::Text { text } if text.starts_with("[SUBAGENT ")),
            )
        })
        .unwrap();
    assert_eq!(report.role, "user");
    assert_eq!(report.name.as_deref(), Some("subagent"));
    // 最后一条不是报告——模型不再把它当作最新消息。
    assert!(!texts.last().unwrap().starts_with("[SUBAGENT "));
}

#[test]
fn write_order_merge_survives_full_snapshot_roundtrip() {
    let mut store = MessageStore::new("seed");
    store.push_user("第一轮");
    store.push_assistant(Message {
        msg_id: None,
        role: "assistant".to_string(),
        name: None,
        content: vec![ContentBlock::text("第一轮回答")],
    });
    let mut inject = Message::user("[SUBAGENT 'x' COMPLETED]\n\n中途报告");
    inject.name = Some("subagent".into());
    store.push_trailing_system(inject);
    store.push_user("第二轮");
    store.push_assistant(Message {
        msg_id: None,
        role: "assistant".to_string(),
        name: None,
        content: vec![ContentBlock::text("第二轮回答")],
    });

    // 全量快照（undo/compact/重启）往返后，注入的写入位置必须保持不变。
    let snapshot = store.to_vec();
    let (restored, repairs) = MessageStore::from_messages("seed", &snapshot, 0);
    assert!(
        repairs.is_empty(),
        "replay should not report repairs: {repairs:?}"
    );
    let restored_vec = restored.to_vec();
    let roles: Vec<&str> = restored_vec.iter().map(|m| m.role.as_str()).collect();
    assert_eq!(
        roles,
        vec!["user", "assistant", "user", "user", "assistant"]
    );
    // 报告位于第一轮与第二轮之间。
    let report_idx = restored_vec
        .iter()
        .position(|m| {
            m.content.iter().any(
                |b| matches!(b, ContentBlock::Text { text } if text.starts_with("[SUBAGENT ")),
            )
        })
        .unwrap();
    assert_eq!(report_idx, 2);
    assert_eq!(restored_vec[report_idx].name.as_deref(), Some("subagent"));
    // gate 上下文与快照同序。
    let ctx = restored.build_context_for_gate(&[]);
    assert_eq!(ctx.len(), restored_vec.len());
}

#[test]
fn compact_summary_keeps_write_position_before_trailing() {
    let mut store = MessageStore::new("seed");
    store.push_user("第一轮");
    store.push_assistant(Message {
        msg_id: None,
        role: "assistant".to_string(),
        name: None,
        content: vec![ContentBlock::text("第一轮回答")],
    });
    let mut inject = Message::user("[SUBAGENT 'x' COMPLETED]\n\n报告");
    inject.name = Some("subagent".into());
    store.push_trailing_system(inject);
    store.push_user("第二轮");
    store.push_assistant(Message {
        msg_id: None,
        role: "assistant".to_string(),
        name: None,
        content: vec![ContentBlock::text("第二轮回答")],
    });

    store.apply_compact("前一轮摘要", 1);
    let ctx = store.build_context_for_gate(&[]);
    let texts: Vec<String> = ctx
        .iter()
        .map(|m| {
            m.content
                .iter()
                .find_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.clone()),
                    _ => None,
                })
                .unwrap_or_default()
        })
        .collect();
    // A1 折叠语义：注入位于被压缩的第一轮（msg_id < 首个保留 turn）——
    // 其文本已进入 compact 摘要输入，随旧 turn 一起折叠，不再出现在活跃
    // 上下文（避免"注入复活 + 双重表示"）。摘要仍占据最早 turn 位置。
    assert!(texts[0].starts_with("[Compacted 1 turns]"));
    assert!(
        !texts.iter().any(|t| t.starts_with("[SUBAGENT ")),
        "compacted-region injection must be folded"
    );
    assert_eq!(texts[1], "第二轮");
    assert_eq!(texts[2], "第二轮回答");
}

#[test]
fn compact_keeps_trailing_injection_in_kept_region() {
    let mut store = MessageStore::new("seed");
    store.push_user("第一轮");
    store.push_assistant(Message {
        msg_id: None,
        role: "assistant".to_string(),
        name: None,
        content: vec![ContentBlock::text("第一轮回答")],
    });
    store.push_user("第二轮");
    store.push_assistant(Message {
        msg_id: None,
        role: "assistant".to_string(),
        name: None,
        content: vec![ContentBlock::text("第二轮回答")],
    });
    // 注入落在第二个（保留）turn 之后——属于保留区，compact 后须存活。
    let mut inject = Message::user("[SUBAGENT 'y' COMPLETED]\n\n后报告");
    inject.name = Some("subagent".into());
    store.push_trailing_system(inject);

    store.apply_compact("前一轮摘要", 1);
    let ctx = store.build_context_for_gate(&[]);
    let texts: Vec<String> = ctx
        .iter()
        .map(|m| {
            m.content
                .iter()
                .find_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.clone()),
                    _ => None,
                })
                .unwrap_or_default()
        })
        .collect();
    // 摘要在前，保留 turn 居中，保留区注入在最后——不被折叠、不挂最前。
    assert!(texts[0].starts_with("[Compacted 1 turns]"));
    assert_eq!(texts[1], "第二轮");
    assert_eq!(texts[2], "第二轮回答");
    assert_eq!(texts[3], "[SUBAGENT 'y' COMPLETED]\n\n后报告");
}

#[test]
fn trailing_injection_never_splits_toolcall_from_result() {
    // ② 不变量：write-order 合并后，任何 trailing 注入都不能夹在 assistant
    // 的 tool_call 与对应 tool_result 之间（会破坏工具调用对完整性）。
    let mut store = MessageStore::new("seed");
    store.push_user("跑工具");
    store.push_assistant(assistant_with_tools(&[("exec-1", "exec")]));
    // 工具结果在注入之前已提交（lap 边界先落盘结果、后落盘注入）→ 注入
    // 的写入 id 大于该步所有结果，排序后位于结果之后，不夹在中间。
    store.push_tool_result_direct("exec-1", "EXEC_RESULT", true);
    let mut inject = Message::user("[SUBAGENT 'z' COMPLETED]\n\nmid报告");
    inject.name = Some("subagent".into());
    store.push_trailing_system(inject);

    let flat = store.flat_in_write_order(0);
    // 辅助定位：tool_call 在 assistant，tool_result 在 tool 消息；注入必须
    // 位于 tool_result 之后（同一写入序），绝不位于二者之间。
    let has_tool_result = |m: &Message| {
        m.content
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
    };
    let has_injection = |m: &Message| {
        m.content
            .iter()
            .any(|b| matches!(b, ContentBlock::Text { text } if text.starts_with("[SUBAGENT ")))
    };
    let result_idx = flat.iter().position(has_tool_result).unwrap();
    let inject_idx = flat.iter().position(has_injection).unwrap();
    // 注入排在 tool_result 之后（而非夹在 assistant 与其 tool_result 之间）。
    assert!(
        inject_idx > result_idx,
        "injection must not split toolcall/result"
    );
}

/// 不变量：`push_system` 只在会话建立（历史为空）时可用；此后注入走
/// trailing，按写入序追加在尾部。
///
/// 这条断言是 P6 设计输入 A 的**替代品**：实测生产只有会话建立的三个入口
/// 调用 `push_system`（skills / MCP / subagent / goal 全部走 trailing），
/// 所以「中途 push_system 把新内容顶到最前」不是现存 bug，而是**潜在**
/// 编程错误。把它变成 debug_assert 后，未来任何引入该形态的改动会在测试
/// 构建里立刻失败，而不是静默击穿前缀缓存。
#[test]
fn push_system_is_creation_only_and_injection_stays_trailing() {
    let mut store = MessageStore::new_ephemeral("test");
    // 建立期：历史为空 → 允许。
    assert!(!store.push_system(Message::system("base")));
    store.push_user("hello");

    // 建立期之后注入必须走 trailing，并且落在 base 之后。
    store.push_trailing_system(Message::developer("<skill_context_envelope />"));
    let rendered = store.to_vec();
    let roles: Vec<&str> = rendered.iter().map(|m| m.role.as_str()).collect();
    assert_eq!(
        roles,
        vec!["system", "user", "developer"],
        "base 必须仍是第一段；注入在尾部"
    );
}

/// 反向证据：历史非空时 `push_system` 会命中 debug_assert（测试构建即红）。
#[test]
#[should_panic(expected = "push_system 只能用于会话建立")]
fn push_system_after_history_is_a_programming_error() {
    let mut store = MessageStore::new_ephemeral("test");
    store.push_system(Message::system("base"));
    store.push_user("hello");
    store.push_system(Message::system("late catalog"));
}

#[test]
fn push_system_input_starts_a_turn_that_accepts_assistant_reply() {
    let mut store = MessageStore::new_ephemeral("test");
    store.push_system_input("[SUBAGENT 'x' COMPLETED]\n\nresult");
    store.push_assistant(Message {
        msg_id: None,
        role: "assistant".to_string(),
        name: None,
        content: vec![ContentBlock::text("ack")],
    });

    let vec = store.to_vec();
    let roles: Vec<&str> = vec.iter().map(|m| m.role.as_str()).collect();
    assert_eq!(roles, vec!["system", "assistant"]);
}

#[test]
fn current_step_projects_each_result_verbatim() {
    let mut store = MessageStore::new_ephemeral("test");
    store.push_user("inspect files");
    store.push_assistant(assistant_with_tools(&[
        ("write-1", "write"),
        ("read-1", "read"),
        ("exec-1", "exec"),
    ]));
    store.push_tool_result_direct("write-1", "WRITE_RESULT", true);
    store.push_tool_result_direct("read-1", "READ_RESULT", true);
    store.push_tool_result_direct("exec-1", "EXEC_RESULT", true);

    let context = store.build_context_for_gate(&[]);

    assert_eq!(context_result(&context, "write-1"), "WRITE_RESULT");
    assert_eq!(context_result(&context, "read-1"), "READ_RESULT");
    assert_eq!(context_result(&context, "exec-1"), "EXEC_RESULT");
}

#[test]
fn display_diff_flows_to_last_step_results_but_not_model_projection() {
    // 展示平面 diff：push_tool_result_direct_with_diff 存入结构化 ToolResult
    // → last_step_tool_results 带出（timeline 消费）→ 模型投影（gate context）
    // 绝不携带 diff 正文。
    let mut store = MessageStore::new_ephemeral("test");
    store.push_user("edit a file");
    store.push_assistant(assistant_with_tools(&[("edit-1", "edit")]));
    let diff = "--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1 +1 @@\n-old\n+new\n";
    store.push_tool_result_direct_with_diff(
        "edit-1",
        "[OK] edit\n  src/a.rs: 1/1 hunk(s) applied at L2 (+1 -1)",
        true,
        Some(diff.to_string()),
    );

    let results = store.last_step_tool_results();
    assert_eq!(results.len(), 1);
    let step_result = &results[0];
    assert_eq!(step_result.tool_call_id, "edit-1");
    assert!(step_result.result.model_text().starts_with("[OK] edit"));
    assert_eq!(step_result.result.status, qaqh_types::ToolStatus::Ok);
    assert_eq!(step_result.result.diff.as_deref(), Some(diff));

    // 模型投影保持精简：diff 正文缺席（展示平面单独携带）。
    let context = store.build_context_for_gate(&[]);
    let projected = context_result(&context, "edit-1");
    assert_eq!(
        projected,
        "[OK] edit\n  src/a.rs: 1/1 hunk(s) applied at L2 (+1 -1)"
    );
    assert!(
        !projected.contains("+++ b/"),
        "diff leaked to model: {projected}"
    );
}

#[test]
fn completed_step_projects_each_result_verbatim() {
    let mut store = MessageStore::new_ephemeral("test");
    store.push_user("first turn");
    store.push_assistant(assistant_with_tools(&[
        ("write-1", "write"),
        ("read-1", "read"),
    ]));
    store.push_tool_result_direct("write-1", "WRITE_RESULT", true);
    store.push_tool_result_direct("read-1", "READ_RESULT", true);
    store.push_user("second turn");

    let context = store.build_context_for_gate(&[]);

    assert_eq!(context_result(&context, "write-1"), "WRITE_RESULT");
    assert_eq!(context_result(&context, "read-1"), "READ_RESULT");
}

#[test]
fn historical_read_preserves_full_content() {
    let mut store = MessageStore::new_ephemeral("test");
    store.push_user("inspect file");
    store.push_assistant(assistant_with_tools(&[("read-1", "read")]));
    store.push_tool_result_direct(
        "read-1",
        &serde_json::json!({
            "status": "ok",
            "path": "src/lib.rs",
            "start_line": 40,
            "end_line": 80,
            "total_lines": 300,
            "content": "sensitive and lengthy source body"
        })
        .to_string(),
        true,
    );
    store.push_user("continue");

    let stored: serde_json::Value = serde_json::from_str(&context_result(
        &store.build_context_for_gate(&[]),
        "read-1",
    ))
    .expect("stored read remains valid JSON");
    assert_eq!(stored["path"], "src/lib.rs");
    assert_eq!(stored["start_line"], 40);
    assert!(!stored["content"].as_str().unwrap().contains("folded"));
    assert!(stored.to_string().contains("lengthy source body"));
}

#[test]
fn historical_edit_result_passes_through_verbatim() {
    let mut store = MessageStore::new_ephemeral("test");
    store.push_user("make edit");
    store.push_assistant(assistant_with_tools(&[("edit-1", "edit")]));
    let body = "[OK] src/lib.rs:42 +3 -2 | edit\n\n@@ -42,2 +42,3 @@\n-full diff body";
    store.push_tool_result_direct("edit-1", body, true);

    assert_eq!(
        context_result(&store.build_context_for_gate(&[]), "edit-1"),
        body
    );
}

#[test]
fn content_bearing_tool_kept_intact_on_active_step() {
    // web is content-bearing: a short result stays fully visible — the
    // model must consume it without re-fetching the URL.
    let mut store = MessageStore::new_ephemeral("test");
    store.push_user("fetch page");
    store.push_assistant(assistant_with_tools(&[("web-1", "web_fetch")]));
    store.push_tool_result_direct("web-1", "PAGE_TEXT_BODY", true);

    assert_eq!(
        context_result(&store.build_context_for_gate(&[]), "web-1"),
        "PAGE_TEXT_BODY"
    );
}

#[test]
fn tool_result_never_changes_after_storage() {
    // Storage-time finalization: the SAME bytes are rendered whether the
    // step is active or historical. A later step must not change the
    // earlier result (that used to fold it, breaking the prefix cache at
    // that tool message on the very next round).
    let mut store = MessageStore::new_ephemeral("test");
    store.push_user("fetch page");
    store.push_assistant(assistant_with_tools(&[("web-1", "web_fetch")]));
    store.push_tool_result_direct("web-1", "PAGE_TEXT_BODY", true);
    let active_ctx = store.build_context_for_gate(&[]);
    let active_render = context_result(&active_ctx, "web-1");

    // New step arrives → the web result is now historical.
    store.push_assistant(assistant_with_tools(&[("edit-1", "edit")]));
    store.push_tool_result_direct("edit-1", "OK_RECEIPT", true);

    let context = store.build_context_for_gate(&[]);
    assert_eq!(
        context_result(&context, "web-1"),
        active_render,
        "historical web result must render identically to the active-step render"
    );
    // 工具结果在工具侧定型，存储字节即最终形态——无论活动/历史渲染一致。
    assert_eq!(context_result(&context, "edit-1"), "OK_RECEIPT");
}

#[test]
fn long_tool_result_passes_through_at_storage() {
    // message 侧不再截断：工具侧（tool_side_fold）在回传前已定型，
    // 存储层原样保存，后续渲染逐字节一致。
    // 20K > 旧 message 侧 16K 上限，但 < qaqh-types 24K 硬顶——
    // 若 message 侧截断仍存在，此断言会失败。
    let body = "paragraph\n".repeat(2_000); // ~20K chars
    let mut store = MessageStore::new_ephemeral("test");
    store.push_user("fetch page");
    store.push_assistant(assistant_with_tools(&[("web-1", "web_fetch")]));
    store.push_tool_result_direct("web-1", &body, true);

    let first = context_result(&store.build_context_for_gate(&[]), "web-1");
    assert_eq!(first, body, "store must pass the result through verbatim");

    // Same bytes on every later render (historical position included).
    store.push_assistant(assistant_with_tools(&[("edit-1", "edit_file")]));
    store.push_tool_result_direct("edit-1", "OK_RECEIPT", true);
    let second = context_result(&store.build_context_for_gate(&[]), "web-1");
    assert_eq!(first, second, "stored result must be stable across renders");
}

#[test]
fn edit_failure_result_passes_through_uncut() {
    // 失败结果（no_match 候选/详情）不折叠：模型必须看到失败原因与候选才能
    // 修正重试——折叠会剪断反馈闭环（曾导致 edit 失败时模型只能盲猜
    // 重试 → TTL 过期 → 缓存失效螺旋）。
    let mut store = MessageStore::new_ephemeral("test");
    store.push_user("edit file");
    store.push_assistant(assistant_with_tools(&[("edit-1", "edit")]));
    store.push_tool_result_direct(
        "edit-1",
        "[ERROR] no_match\n  detail: old matches nothing near expected context\n  candidates: [L12, L40]",
        true,
    );

    let projected = context_result(&store.build_context_for_gate(&[]), "edit-1");
    assert!(projected.contains("no_match"));
    assert!(
        projected.contains("candidates"),
        "failure candidates must survive: {projected}"
    );
    assert!(!projected.contains("[edit diff folded"));
}

#[test]
fn edit_partial_failure_passes_through_uncut() {
    // partial 模式同样透传：失败 hunk 的详情（含候选）不能被折叠。
    let mut store = MessageStore::new_ephemeral("test");
    store.push_user("edit file");
    store.push_assistant(assistant_with_tools(&[("edit-1", "edit")]));
    store.push_tool_result_direct(
        "edit-1",
        "[PARTIAL] 1 hunk(s) applied, 1 failed — re-send ONLY the failed hunks\n  hunk 1: no_match detail with candidates",
        true,
    );

    let projected = context_result(&store.build_context_for_gate(&[]), "edit-1");
    assert!(projected.contains("no_match"));
    assert!(projected.contains("hunk 1"));
    assert!(!projected.contains("[edit diff folded"));
}

#[test]
fn annotation_injected_into_first_user_message_not_last() {
    // The [Environment] annotation is pinned to the FIRST user message:
    // its position never moves, so turn-1's message renders identically
    // once turn 2 arrives (prefix cache survives the turn boundary).
    let mut store = MessageStore::new_ephemeral("test");
    store.push_user("first turn");
    store.push_assistant(assistant_with_tools(&[("edit-1", "edit_file")]));
    store.push_tool_result_direct("edit-1", "OK_RECEIPT", true);
    store.push_user("second turn");

    let context = store.build_context_for_gate(&[String::from(
        "<workspace_path>F:\\QAQ-Harness</workspace_path>",
    )]);
    let users: Vec<String> = context
        .iter()
        .filter(|m| m.role == "user")
        .map(|m| {
            m.content
                .iter()
                .filter_map(|b| match b {
                    qaqh_types::ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect();
    assert_eq!(users.len(), 2);
    assert!(
        users[0].starts_with("[Environment]\n<workspace_path>F:\\QAQ-Harness</workspace_path>"),
        "first user message must carry the annotation, got: {}",
        users[0]
    );
    assert_eq!(
        users[1], "second turn",
        "later user messages stay untouched"
    );
}

#[test]
fn prefix_stable_across_turns_with_tool_rounds() {
    // Core regression: the entire turn-1 segment (user message, tool
    // calls, tool results, final answer) must render byte-identically in
    // turn-2 requests. This was broken by (a) annotation injection into
    // the last user message and (b) position-dependent tool folding.
    let mut store = MessageStore::new_ephemeral("test");
    store.push_user("first turn");
    store.push_assistant(assistant_with_tools(&[("web-1", "web_fetch")]));
    store.push_tool_result_direct("web-1", "WEB_BODY", true);
    store.push_assistant(assistant_with_tools(&[("exec-1", "exec")]));
    store.push_tool_result_direct("exec-1", "EXEC_OUTPUT", true);
    store.push_assistant(Message {
        msg_id: None,
        role: "assistant".into(),
        name: None,
        content: vec![qaqh_types::ContentBlock::Text {
            text: "done".into(),
        }],
    });
    let turn1_ctx = store.build_context_for_gate(&[String::from("ann")]);

    store.push_user("second turn");
    let turn2_ctx = store.build_context_for_gate(&[String::from("ann")]);

    assert!(turn2_ctx.len() > turn1_ctx.len());
    let turn1_ser = serde_json::to_string(&turn1_ctx).expect("serialize turn-1 context");
    let turn2_prefix_ser =
        serde_json::to_string(&turn2_ctx[..turn1_ctx.len()]).expect("serialize turn-2 prefix");
    assert_eq!(
        turn2_prefix_ser, turn1_ser,
        "turn-1 message segment must be byte-identical in turn 2"
    );
}

#[test]
fn tool_result_stays_verbatim_even_on_active_step() {
    // write/edit 的结果在工具侧定型：活动步骤也原样渲染，不再折叠。
    let mut store = MessageStore::new_ephemeral("test");
    store.push_user("write file");
    store.push_assistant(assistant_with_tools(&[("write-1", "write")]));
    store.push_tool_result_direct("write-1", "WRITE_RESULT", true);

    assert_eq!(
        context_result(&store.build_context_for_gate(&[]), "write-1"),
        "WRITE_RESULT"
    );
}

#[test]
fn skill_result_remains_complete_across_steps_and_turns() {
    let activation = format!(
        "[QAQH_SKILL_V1]\nname: large\n--- instructions ---\n{}\n[END_QAQH_SKILL_V1]",
        "instruction\n".repeat(600)
    );
    let mut store = MessageStore::new_ephemeral("test");
    store.push_user("activate it");
    store.push_assistant(assistant_with_tools(&[("skill-1", "skills")]));
    store.push_tool_result_direct("skill-1", &activation, true);

    assert_eq!(
        context_result(&store.build_context_for_gate(&[]), "skill-1"),
        activation
    );
    store.push_user("continue");
    assert_eq!(
        context_result(&store.build_context_for_gate(&[]), "skill-1"),
        activation
    );
}

#[test]
fn glob_listing_remains_complete_across_steps_and_turns() {
    // glob 的产物就是文件清单：折叠成首行会让模型拿不到清单、
    // 被迫换 exec rg 绕行。与 read/skills 同理由透传
    // （工具自身 max_results 熔断 = self-limited）。
    let listing = "crates/qaqh-config/src/config.rs\ncrates/qaqh-config/src/lib.rs\ncrates/qaqh-config/src/prompt.rs\ncrates/qaqh-config/src/registry.rs\n";
    let mut store = MessageStore::new_ephemeral("test");
    store.push_user("list files");
    store.push_assistant(assistant_with_tools(&[("glob-1", "glob")]));
    store.push_tool_result_direct("glob-1", listing, true);

    assert_eq!(
        context_result(&store.build_context_for_gate(&[]), "glob-1"),
        listing
    );
    store.push_user("continue");
    assert_eq!(
        context_result(&store.build_context_for_gate(&[]), "glob-1"),
        listing
    );
}

#[test]
fn skills_resource_action_remains_complete_across_steps_and_turns() {
    let resource = "reference line\n".repeat(600);
    let mut store = MessageStore::new_ephemeral("test");
    store.push_user("read its reference");
    store.push_assistant(assistant_with_tools(&[("resource-1", "skills")]));
    store.push_tool_result_direct("resource-1", &resource, true);

    assert_eq!(
        context_result(&store.build_context_for_gate(&[]), "resource-1"),
        resource
    );
    store.push_user("continue");
    assert_eq!(
        context_result(&store.build_context_for_gate(&[]), "resource-1"),
        resource
    );
}

#[test]
fn restore_keeps_distinct_protected_skill_system_messages() {
    let skill = "[QAQH_SKILL_V1]\nname: review\n[END_QAQH_SKILL_V1]";
    let messages = vec![
        Message::system("base prompt"),
        Message::system(skill),
        Message::system(skill),
        Message::system("stale duplicate prompt"),
        Message::user("hello"),
    ];
    let (store, repairs) = MessageStore::from_messages("test", &messages, 0);
    assert_eq!(store.system_messages().len(), 2);
    assert!(store.system_messages().iter().any(|message| {
        message
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::Text { text } if text == skill))
    }));
    assert_eq!(repairs.len(), 2);
}

fn persisted_turns(count: usize) -> MessageStore {
    let mut store = MessageStore::new("compact-append-test");
    for i in 1..=count {
        store.push_user(&format!("u{i}"));
        store.push_assistant(Message {
            msg_id: None,
            role: "assistant".into(),
            name: None,
            content: vec![ContentBlock::Text {
                text: format!("a{i}"),
            }],
        });
    }
    store
}

#[test]
fn compaction_appends_summary_with_fresh_id_and_watermark() {
    let mut store = persisted_turns(4);
    store.flush_meta("m", "high");
    let _ = store.take_persist_ops();
    store.apply_compact("summary", 2);
    store.persist_compaction("m", "high");

    let ops = store.take_persist_ops();
    let (messages, covered) = ops
        .iter()
        .find_map(|op| match op {
            PersistOp::Append {
                messages,
                compact_covered_through_msg_id,
                ..
            } => Some((messages, *compact_covered_through_msg_id)),
            _ => None,
        })
        .expect("compaction must append a summary");
    assert_eq!(messages.len(), 1);
    assert!(is_compaction_summary(&messages[0]));
    assert_eq!(
        messages[0].msg_id,
        Some(9),
        "summary must use a fresh append id, not a compacted turn id"
    );
    assert_eq!(
        covered,
        Some(4),
        "watermark must cover every message before the first kept turn"
    );
}

#[test]
fn compaction_summary_is_rendered_first_despite_fresh_id() {
    let mut store = persisted_turns(4);
    store.apply_compact("summary", 2);

    let view = store.build_context_for_gate(&[]);
    assert!(is_compaction_summary(&view[0]));
    assert!(view.iter().any(|m| {
        m.content
            .iter()
            .any(|b| matches!(b, ContentBlock::Text { text } if text == "u3"))
    }));
    assert!(!view.iter().any(|m| {
        m.content
            .iter()
            .any(|b| matches!(b, ContentBlock::Text { text } if text == "u1"))
    }));
}

#[test]
fn full_rewrite_preserves_compaction_watermark_unless_all_turns_are_cleared() {
    let mut store = persisted_turns(4);
    store.apply_compact("summary", 2);
    let _ = store.take_persist_ops();

    store.snapshot_full("m", "high");
    let op = store.take_persist_ops().pop().expect("save full op");
    assert!(matches!(
        op,
        PersistOp::SaveFull {
            compact_covered_through_msg_id: Some(4),
            ..
        }
    ));

    // Undo across the compacted boundary clears the summary too; the
    // rewritten archive must not retain a watermark with nothing to hide.
    assert!(store.truncate_before_turn("t1"));
    store.snapshot_full("m", "high");
    let op = store.take_persist_ops().pop().expect("save full op");
    assert!(matches!(
        op,
        PersistOp::SaveFull {
            compact_covered_through_msg_id: None,
            ..
        }
    ));
}

#[test]
fn repeated_compaction_replaces_the_old_summary_in_active_view() {
    let mut store = persisted_turns(4);
    store.apply_compact("first", 2);
    let _ = store.take_persist_ops();
    store.push_user("u5");
    store.push_assistant(Message {
        msg_id: None,
        role: "assistant".into(),
        name: None,
        content: vec![ContentBlock::Text { text: "a5".into() }],
    });
    store.apply_compact("second", 2);

    let view = store.build_context_for_gate(&[]);
    assert!(is_compaction_summary(&view[0]));
    let rendered = serde_json::to_string(&view).unwrap();
    assert!(rendered.contains("second"));
    assert!(!rendered.contains("first"));
}

#[test]
fn find_last_step_tool_call_returns_correct_id() {
    let mut store = MessageStore::new_ephemeral("test");
    store.push_user("hello");

    let assistant = Message {
        msg_id: None,
        role: "assistant".into(),
        name: None,
        content: vec![
            ContentBlock::Text {
                text: "Let me ask...".into(),
            },
            ContentBlock::ToolUse {
                id: "call_abc".into(),
                name: "ask_user".into(),
                input: serde_json::json!({"question": "Which?"}),
            },
        ],
    };
    store.push_assistant(assistant);

    let id = store.find_last_step_tool_call("ask_user").unwrap();
    assert_eq!(id, "call_abc");
    assert!(store.find_last_step_tool_call("bogus").is_none());
}

#[test]
fn find_last_step_tool_call_no_turns_returns_none() {
    let store = MessageStore::new_ephemeral("test");
    assert!(store.find_last_step_tool_call("ask_user").is_none());
}

#[test]
fn compact_does_not_reuse_archived_turn_ids() {
    let mut store = MessageStore::new_ephemeral("test");
    for index in 1..=5 {
        assert_eq!(store.allocate_turn_id(), format!("t{index}"));
        store.push_user(&format!("turn {index}"));
    }

    store.apply_compact("checkpoint", 2);

    assert_eq!(store.turn_count(), 3); // summary + two kept turns
    assert_eq!(store.allocate_turn_id(), "t6");
}

#[test]
fn truncate_before_turn_uses_global_sequence_after_compact() {
    let mut store = MessageStore::new_ephemeral("test");
    for index in 1..=5 {
        store.allocate_turn_id();
        store.push_user(&format!("turn {index}"));
    }
    // 物理裁剪：turns = [summary, t4, t5]，next_turn_seq 继续涨（t6 下一个）。
    store.apply_compact("checkpoint", 2);
    assert_eq!(store.turn_count(), 3);

    // 撤 t5（活跃最后一条）：t5 → idx 2 → 删除 t5，剩 [summary, t4]。
    assert!(store.truncate_before_turn("t5"));
    assert_eq!(store.turn_count(), 2);

    // 撤 t4（第一个活跃 turn）：保留合成摘要，剩 [summary]。
    assert!(store.truncate_before_turn("t4"));
    assert_eq!(store.turn_count(), 1);

    // 撤已被 compact 的 t3（序号早于活跃段起点）：清空（含摘要）。
    assert!(store.truncate_before_turn("t3"));
    assert_eq!(store.turn_count(), 0);

    // 越界序号（未来 turn）拒绝。
    assert!(!store.truncate_before_turn("t99"));
    // 非数字/零前缀拒绝。
    assert!(!store.truncate_before_turn("abc"));
    assert!(!store.truncate_before_turn("t0"));
}

/// 回归：保留区覆盖全部历史时（压缩区只剩上一份摘要占位），压缩必须
/// 什么都不做。
///
/// 旧实现照样插入一条新摘要并推进 `context_revision`：真实轮次一个没少，
/// 上下文里却多出一段检查点；调用方再按"turn 数变没变"判定终态，就会报
/// "压缩已取消"——用户看到的就是"失败却莫名多出一段上下文"。
#[test]
fn compact_without_foldable_turns_writes_nothing() {
    let mut store = MessageStore::new_ephemeral("test");
    for index in 1..=3 {
        store.push_user(&format!("turn {index}"));
    }
    let first = store.apply_compact("first checkpoint body", 1);
    assert_eq!(first.folded_turns, 2, "首次压缩折叠两轮");
    assert!(first.applied());

    store.push_user("turn 4");
    let turns_before = store.turn_count();
    let revision_before = store.context_revision();
    // 保留区 = 除第一项（上一份摘要）以外的全部 turn。
    let second = store.apply_compact("second checkpoint body", turns_before - 1);

    assert_eq!(
        second.folded_turns, 0,
        "只剩摘要占位可压，不得计为折叠任何真实 turn"
    );
    assert!(!second.applied(), "没有真实 turn 可折叠时不得写入摘要");
    assert_eq!(store.turn_count(), turns_before, "活跃 turn 数不得变化");
    assert_eq!(
        store.context_revision(),
        revision_before,
        "no-op 压缩不得推进 context_revision"
    );
    let serialized = serde_json::to_string(&store.build_context_for_gate(&[])).unwrap();
    assert!(serialized.contains("first checkpoint body"));
    assert!(
        !serialized.contains("second checkpoint body"),
        "no-op 压缩不得把新摘要塞进上下文"
    );
}

#[test]
fn truncate_before_turn_without_compact_keeps_index_equivalence() {
    let mut store = MessageStore::new_ephemeral("test");
    for index in 1..=4 {
        store.allocate_turn_id();
        store.push_user(&format!("turn {index}"));
    }
    // 无 compact：t1..t4 即 turns[0..4]，next_turn_seq = 5。
    assert!(store.truncate_before_turn("t3"));
    assert_eq!(store.turn_count(), 2); // 剩 t1, t2
    assert!(!store.truncate_before_turn("t3")); // 已删，越界拒绝
}

#[test]
fn repeated_compact_replaces_the_old_summary_in_gate_context() {
    let mut store = MessageStore::new_ephemeral("test");
    for index in 1..=5 {
        store.push_user(&format!("turn {index}"));
    }
    store.apply_compact("first checkpoint body", 2);
    store.push_user("work after first checkpoint");
    store.push_user("latest work");

    store.apply_compact("second checkpoint body", 2);

    let serialized = serde_json::to_string(&store.build_context_for_gate(&[])).unwrap();
    assert!(serialized.contains("second checkpoint body"));
    assert!(!serialized.contains("first checkpoint body"));
    assert_eq!(serialized.matches("[Compacted ").count(), 1);
}

#[test]
fn restored_turn_allocator_can_follow_immutable_archive_count() {
    let messages = vec![Message::user("active summary"), Message::user("recent")];
    let (mut store, _) = MessageStore::from_messages("test", &messages, 0);
    store.ensure_next_turn_seq(31);

    assert_eq!(store.allocate_turn_id(), "t31");
}

#[test]
fn orphaned_patch_restore_reports_unknown_side_effects_without_reexecution() {
    let messages = vec![
        Message::user("apply patch"),
        assistant_with_tools(&[("call_patch", "apply_patch")]),
    ];
    let (store, repairs) = MessageStore::from_messages("test", &messages, 0);
    assert_eq!(repairs.len(), 1);
    let context = store.build_context_for_gate(&[]);
    let note = context_result(&context, "call_patch");
    assert!(note.contains("outcome unknown"), "{note}");
    assert!(note.contains("side effects may have occurred"), "{note}");
    assert!(note.contains("Do NOT retry automatically"), "{note}");
    assert!(!note.contains("not executed"), "{note}");
    let (restored_again, repairs_again) = MessageStore::from_messages("test", &context, 0);
    assert!(repairs_again.is_empty(), "{repairs_again:?}");
    let replayed = restored_again.build_context_for_gate(&[]);
    assert_eq!(context_result(&replayed, "call_patch"), note);
}

#[test]
fn from_messages_restores_user_subagent_injection_as_trailing() {
    // user 角色 + name="subagent" + [SUBAGENT 前缀：注入消息回放时
    // 恢复为 trailing，不得当作真实用户 turn（不建回合）。
    let mut inject = Message::user("[SUBAGENT 'replay_test_x' COMPLETED]\n\nREPLAY-TEST-OK-X");
    inject.name = Some("subagent".into());
    let messages = vec![
        Message::system("base instructions"),
        Message::user("real user message"),
        inject,
    ];
    let (store, _) = MessageStore::from_messages("test", &messages, 0);

    // 只有一条真实用户 turn；注入在 trailing。
    assert_eq!(store.turn_count(), 1);
    assert_eq!(store.trailing_messages.len(), 1);
    let text = store.trailing_messages[0]
        .content
        .iter()
        .find_map(|b| match b {
            qaqh_types::ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .unwrap_or("");
    assert!(text.contains("replay_test_x"));

    // gate context：system + 真实 user + 注入 user（trailing 在 turns 之后）。
    let ctx = store.build_context_for_gate(&[]);
    let roles: Vec<&str> = ctx.iter().map(|m| m.role.as_str()).collect();
    assert_eq!(roles, vec!["system", "user", "user"]);
    let last = ctx.last().unwrap();
    assert_eq!(last.name.as_deref(), Some("subagent"));
}

/// 工作区变更审计注入（PR2）：只有 name="workspace"、**没有** [SUBAGENT
/// 前缀——正是四处硬编码 "subagent" 时会漏判的形态。恢复必须回到
/// trailing，不得当成真实用户 turn（否则虚增回合数、并把报告钉进对话史）。
#[test]
fn from_messages_restores_workspace_injection_as_trailing() {
    let mut inject = Message::user(concat!(
        "[workspace-changes turn=t1 round=0 mark=s0001790946308788_0001 calls=call-1]\n",
        "[workspace] 自标记以来 1 个文件变更\n",
        "─── src/a.py (新增, - → 26B)",
    ));
    inject.name = Some("workspace".into());
    let messages = vec![
        Message::system("base instructions"),
        Message::user("real user message"),
        inject,
    ];
    let (store, _) = MessageStore::from_messages("test", &messages, 0);

    assert_eq!(store.turn_count(), 1, "注入不得建回合");
    assert_eq!(store.trailing_messages.len(), 1, "注入必须恢复为 trailing");
    let text = store.trailing_messages[0]
        .content
        .iter()
        .find_map(|b| match b {
            qaqh_types::ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .unwrap_or("");
    assert!(text.contains("[workspace-changes"), "{text}");

    // Environment 块必须钉在真实用户消息上，而不是这条注入上：
    // 注入排在首位时会抢先成为「第一条 user」。
    let ctx = store.build_context_for_gate(&[]);
    let names: Vec<Option<&str>> = ctx.iter().map(|m| m.name.as_deref()).collect();
    assert!(names.contains(&Some("workspace")), "{names:?}");
}

// ── T14：压缩不得剪坏 tool_use ↔ tool_result 配对 ──
//
// 背景（codex-thinking-block 多轮实测）：历史里缺了调用记录时，模型会连着
// 几轮不再发起工具调用；反过来，孤儿 tool_result 会被 provider 判成无效
// （anthropic 侧直接丢弃，因为回填不出函数名）。所以「压缩不得只剪掉一半」
// 是**协议级**约束，不是洁癖——这里把它钉在 store 上。

/// 活跃上下文里的工具配对是否闭合。返回 `(悬挂调用, 孤儿结果)`。
///
/// 只对**已答完**的回合成立：run 进行中「调用已发、结果未回」是合法状态，
/// 那种悬挂由 `from_messages` 的 `[RESTORE]` 合成结果兜底，不在本判据内。
fn unpaired_tool_ids(context: &[Message]) -> (Vec<String>, Vec<String>) {
    let mut calls: Vec<String> = Vec::new();
    let mut results: Vec<String> = Vec::new();
    for message in context {
        for block in &message.content {
            match block {
                ContentBlock::ToolUse { id, .. } => calls.push(id.clone()),
                ContentBlock::ToolResult { tool_use_id, .. } => {
                    results.push(tool_use_id.clone())
                }
                _ => {}
            }
        }
    }
    let dangling = calls
        .iter()
        .filter(|id| !results.contains(id))
        .cloned()
        .collect();
    let orphans = results
        .iter()
        .filter(|id| !calls.contains(id))
        .cloned()
        .collect();
    (dangling, orphans)
}

/// 调用与其结果在上下文里的位置（断言「结果必须排在调用之后」用）。
fn tool_pair_positions(context: &[Message], id: &str) -> (usize, usize) {
    let call = context
        .iter()
        .position(|m| {
            m.content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolUse { id: cid, .. } if cid == id))
        })
        .expect("tool_use 必须在上下文里");
    let result = context
        .iter()
        .position(|m| {
            m.content.iter().any(
                |b| matches!(b, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == id),
            )
        })
        .expect("tool_result 必须在上下文里");
    (call, result)
}

/// 压缩按 **turn** 裁：被折叠的 turn 里，`tool_use` 与它的结果必须整体消失。
/// 裁前裁后配对都要闭合，保留区那一对要整对留下且结果排在调用之后。
#[test]
fn compaction_drops_tool_pairs_whole_and_keeps_the_rest_paired() {
    let mut store = MessageStore::new("pair-compact");
    // turn1：一对完整的调用/结果（会被压缩掉）。
    store.push_user("读一下配置");
    store.push_assistant(assistant_with_tools(&[("call_1", "read")]));
    store.push_tool_result("call_1", "config body", true);
    // turn2：保留区里的另一对。
    store.push_user("再读日志");
    store.push_assistant(assistant_with_tools(&[("call_2", "read")]));
    store.push_tool_result("call_2", "log body", true);

    let before = store.build_context_for_gate(&[]);
    assert_eq!(
        unpaired_tool_ids(&before),
        (Vec::new(), Vec::new()),
        "裁前必须闭合"
    );
    let (call, result) = tool_pair_positions(&before, "call_1");
    assert!(call < result, "结果不得排到它的调用之前");

    store.apply_compact("摘要", 1);

    let after = store.build_context_for_gate(&[]);
    assert_eq!(
        unpaired_tool_ids(&after),
        (Vec::new(), Vec::new()),
        "裁后必须闭合"
    );
    let rendered = serde_json::to_string(&after).expect("serialize context");
    assert!(
        !rendered.contains("call_1") && !rendered.contains("config body"),
        "被折叠那一对必须整对消失：{rendered}"
    );
    assert!(
        rendered.contains("call_2") && rendered.contains("log body"),
        "保留区那一对必须整对留下：{rendered}"
    );
    let (call, result) = tool_pair_positions(&after, "call_2");
    assert!(call < result, "结果不得排到它的调用之前");
}

/// 工具**未完成**时用户直接开下一轮：`push_user` 会就地合成终态结果
/// （`auto_complete_unfulfilled`），把这一对**关在同一个 turn 里**——这正是
/// 「压缩按 turn 裁」得以成立的前提。若改成"结果落到当时最新的 turn"，这一对
/// 就横跨两个 turn，压缩必然剪在中间。
///
/// 迟到的真结果此时是重复帧：不得再插一条，也不得被记成孤儿（daemon 会把孤儿
/// 结果当领域错误上报，见 `loop_outcome.rs`）。
#[test]
fn interrupting_a_pending_tool_closes_the_pair_inside_its_turn() {
    let mut store = MessageStore::new("pair-interrupt");
    store.push_user("跑个长命令");
    store.push_assistant(assistant_with_tools(&[("call_9", "exec")]));
    store.push_user("算了，先看看别的");

    let before = store.build_context_for_gate(&[]);
    assert_eq!(
        unpaired_tool_ids(&before),
        (Vec::new(), Vec::new()),
        "打断后配对必须已经闭合"
    );
    let (call, result) = tool_pair_positions(&before, "call_9");
    assert!(call < result, "结果必须排在调用之后");
    assert!(
        context_result(&before, "call_9").contains("CANCELLED"),
        "合成结果要说明为什么没跑"
    );

    let call_turn = store
        .turns
        .iter()
        .position(|t| {
            t.steps.iter().any(|s| {
                s.assistant
                    .content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::ToolUse { id, .. } if id == "call_9"))
            })
        })
        .expect("调用所在 turn");
    let result_turn = store
        .turns
        .iter()
        .position(|t| {
            t.steps.iter().any(|s| {
                s.tool_results.iter().any(|m| {
                    m.content.iter().any(
                        |b| matches!(b, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "call_9"),
                    )
                })
            })
        })
        .expect("结果所在 turn");
    assert_eq!(call_turn, result_turn, "结果必须落在调用所在 turn");

    // 迟到的真结果：重复帧，静默丢弃，且**不**算孤儿。
    store.push_tool_result("call_9", "exec output", true);
    let after = store.build_context_for_gate(&[]);
    let rendered = serde_json::to_string(&after).expect("serialize context");
    assert!(!rendered.contains("exec output"), "重复结果不得再插一条");
    assert_eq!(
        unpaired_tool_ids(&after),
        (Vec::new(), Vec::new()),
        "重复帧不得破坏配对"
    );
    assert!(
        store.take_orphan_tool_results().is_empty(),
        "重复结果不是孤儿，不该触发领域错误上报"
    );
}

// ── BUG-2026-09-13-04 回归：用户图片必须随消息落盘 ──

#[test]
fn push_image_reaches_persisted_append_op() {
    let prev = std::env::var("QAQH_DATA_DIR").ok();
    let tmp = std::env::temp_dir().join(format!("qaqh-store-img4-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    // SAFETY: 单元测试共享进程环境；本用例前后成对设置/恢复该变量，
    // 且 message store 只在此处读取数据根，不存在并发读者。
    unsafe { std::env::set_var("QAQH_DATA_DIR", &tmp) };

    let b64 = "aGVsbG8gaW1hZ2U=";
    let mut store = MessageStore::new("persist-seed");
    store.push_user("look at this");
    store.push_image_to_last_user("image/png", b64);
    store.flush_meta("test-model", "high");

    // 内存视图：图片块在场。
    let turn = store.turns.last().expect("turn");
    assert!(matches!(
        turn.user.content[1],
        ContentBlock::ImageRef { .. }
    ));

    // 持久化视图：Append op 里的同 msg_id 消息也必须带图片块。
    let ops = store.take_persist_ops();
    let append = ops
        .iter()
        .find_map(|op| match op {
            PersistOp::Append { messages, .. } => Some(messages),
            _ => None,
        })
        .expect("Append op must be queued");
    let persisted_user = append
        .iter()
        .find(|m| m.role == "user")
        .expect("user message in append batch");
    assert!(
        matches!(persisted_user.content[1], ContentBlock::ImageRef { .. }),
        "persisted user message must carry the image block, got {persisted_user:?}"
    );

    match prev {
        // SAFETY: 恢复进入本用例前的数据根（与上方 set_var 成对）。
        Some(v) => unsafe { std::env::set_var("QAQH_DATA_DIR", v) },
        // SAFETY: 原本未设置 → 恢复为未设置状态。
        None => unsafe { std::env::remove_var("QAQH_DATA_DIR") },
    }
    let _ = std::fs::remove_dir_all(&tmp);
}

// ── BUG-2026-09-13-25 回归：push_image_to_last_user 必须给出丢失通道 ──

/// 无 user turn（ingest 被拒 / replaying）时不得静默丢图：返回 false
/// 让调用方上报，且图片块绝不落进任何 turn。
#[test]
fn push_image_without_user_turn_reports_drop() {
    let mut store = MessageStore::new_ephemeral("no-turn-seed");
    assert!(store.turns.is_empty(), "precondition: no turn yet");

    let attached = store.push_image_to_last_user("image/png", "aGVsbG8=");

    assert!(
        !attached,
        "no user turn → push_image_to_last_user must report the drop (return false)"
    );
    assert!(
        store.turns.is_empty(),
        "no turn may be fabricated just to host the image"
    );
}

/// 正常路径：有 user turn 时返回 true，图片块真的挂上去了。
#[test]
fn push_image_with_user_turn_reports_attach() {
    let prev = std::env::var("QAQH_DATA_DIR").ok();
    let tmp = std::env::temp_dir().join(format!("qaqh-store-img25-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    // SAFETY: 单元测试共享进程环境；本用例前后成对设置/恢复该变量，
    // 且 message store 只在此处读取数据根，不存在并发读者。
    unsafe { std::env::set_var("QAQH_DATA_DIR", &tmp) };

    let mut store = MessageStore::new_ephemeral("has-turn-seed");
    store.push_user("look at this");
    assert!(store.push_image_to_last_user("image/png", "aGVsbG8="));

    let turn = store.turns.last().expect("turn");
    assert!(matches!(
        turn.user.content[1],
        ContentBlock::ImageRef { .. } | ContentBlock::Image { .. }
    ));

    match prev {
        // SAFETY: 恢复进入本用例前的数据根（与上方 set_var 成对）。
        Some(v) => unsafe { std::env::set_var("QAQH_DATA_DIR", v) },
        // SAFETY: 原本未设置 → 恢复为未设置状态。
        None => unsafe { std::env::remove_var("QAQH_DATA_DIR") },
    }
    let _ = std::fs::remove_dir_all(&tmp);
}

/// 防御分支：已 flush 落盘后 push 图 → SaveFull 重写补齐（model 沿用
/// 最近一次 flush，不得为空）。
#[test]
fn push_image_after_flush_queues_savefull_with_cached_model() {
    let mut store = MessageStore::new("late-image-seed");
    store.push_user("hello");
    store.flush_meta("orig-model", "mid");
    let _ = store.take_persist_ops();

    store.push_image_to_last_user("image/png", "aGVsbG8=");
    let ops = store.take_persist_ops();
    match ops.last().expect("SaveFull op") {
        PersistOp::SaveFull {
            model, messages, ..
        } => {
            assert_eq!(model, "orig-model");
            let user = messages
                .iter()
                .find(|m| m.role == "user")
                .expect("user in snapshot");
            assert!(
                matches!(user.content[1], ContentBlock::ImageRef { .. }),
                "snapshot must carry the image block"
            );
        }
        other => panic!("expected SaveFull, got {other:?}"),
    }
}

/// #13：中断流抢救路径落盘的悬挂 ToolUse（`id:""` / `name:""`）在
/// **持久化前**就被清洗掉——写侧治本，历史归档里不会再出现悬空工具。
#[test]
fn hanging_tool_use_is_dropped_before_persist() {
    let mut store = MessageStore::new("hanging-tool-seed");
    store.push_user("interrupted turn");
    let assistant = Message {
        msg_id: None,
        role: "assistant".to_string(),
        name: None,
        content: vec![
            ContentBlock::text("partial answer"),
            ContentBlock::ToolUse {
                id: String::new(),
                name: String::new(),
                input: serde_json::Value::Null,
            },
        ],
    };
    let _ = store.push_assistant(assistant);
    store.flush_meta("test-model", "mid");
    let persisted: Vec<Message> = store
        .take_persist_ops()
        .iter()
        .flat_map(|op| match op {
            PersistOp::SaveFull { messages, .. } => messages.clone(),
            PersistOp::Append { messages, .. } => messages.clone(),
            _ => Vec::new(),
        })
        .collect();
    assert!(!persisted.is_empty(), "an assistant step must be persisted");
    assert!(
        !persisted.iter().any(|m| m
            .content
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolUse { .. }))),
        "hanging tool_use must be cleansed before persistence: {persisted:?}"
    );
    // 正文不受影响：只丢悬挂工具块，不丢整条 assistant 消息。
    assert!(persisted.iter().any(|m| {
        m.content
            .iter()
            .any(|b| matches!(b, ContentBlock::Text { text } if text == "partial answer"))
    }));
    // 无工具调用 → 回合可正常收尾，不产生孤儿 step。
    assert!(store.turns().last().expect("turn").steps[0].all_tools_satisfied());
}

/// 反例对照：完整 tool_use（有 id 有 name）照常落盘，清洗不误伤。
#[test]
fn well_formed_tool_use_survives_persistence() {
    let mut store = MessageStore::new("good-tool-seed");
    store.push_user("go");
    let _ = store.push_assistant(assistant_with_tools(&[("call-1", "read")]));
    store.flush_meta("test-model", "mid");
    let persisted: Vec<Message> = store
        .take_persist_ops()
        .iter()
        .flat_map(|op| match op {
            PersistOp::SaveFull { messages, .. } => messages.clone(),
            PersistOp::Append { messages, .. } => messages.clone(),
            _ => Vec::new(),
        })
        .collect();
    assert!(
        persisted.iter().any(|m| m.content.iter().any(|b| matches!(
            b,
            ContentBlock::ToolUse { id, name, .. } if id == "call-1" && name == "read"
        ))),
        "well-formed tool_use must survive: {persisted:?}"
    );
}
