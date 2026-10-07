//! 会话列表投影 — 桌面壳/TUI 共享的展示面收敛。
//!
//! 从 webui gateway 迁入(原 `qaqh-webui-gateway/src/lib.rs` 的私有实现):前端
//! 只需要列表卡片字段,`cwd`/`model`/`skills`/`workspace_id` 等宿主侧细节不出
//! daemon 的 `session.list` 投影层。与 TUI 共享受益(单源,后续 TUI 接入同一函数)。

use serde_json::{Value, json};

/// 把 `session.list` 的返回收敛为列表卡片字段:
/// `session_id, title, created_at, updated_at, message_count, turn_count,
/// status, archived, ephemeral`。
///
/// 非数组输入(防护)收敛为空数组;缺失字段以 Null/false 兜底,保证键总是存在
/// (BETA-01 Phase D 契约:`session_id` 保留 verbatim,`seed` 等旧键不得出现)。
pub fn sanitize_session_list(value: Value) -> Value {
    let Some(entries) = value.as_array() else {
        return json!([]);
    };
    let sanitized = entries
        .iter()
        .map(|entry| {
            json!({
                "session_id": entry.get("session_id").cloned().unwrap_or(Value::Null),
                "title": entry.get("title").cloned().unwrap_or(Value::Null),
                "created_at": entry.get("created_at").cloned().unwrap_or(Value::Null),
                "updated_at": entry.get("updated_at").cloned().unwrap_or(Value::Null),
                "message_count": entry.get("message_count").cloned().unwrap_or(Value::Null),
                "turn_count": entry.get("turn_count").cloned().unwrap_or(Value::Null),
                "status": entry.get("status").cloned().unwrap_or(json!("not_running")),
                "archived": entry.get("archived").cloned().unwrap_or(Value::Bool(false)),
                "ephemeral": entry.get("ephemeral").cloned().unwrap_or(Value::Bool(false)),
            })
        })
        .collect::<Vec<_>>();
    Value::Array(sanitized)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn projects_card_fields_and_strips_host_fields() {
        let raw = json!([
            {
                "session_id": "0123abcd",
                "title": "hello",
                "created_at": 1,
                "updated_at": 2,
                "message_count": 3,
                "turn_count": 4,
                "status": "working",
                "archived": false,
                "ephemeral": false,
                "cwd": "/home/user",
                "model": "test-model",
                "workspace_id": "ws",
                "skills": {"x": 1},
                "seed": "legacy-seed",
            }
        ]);
        let out = sanitize_session_list(raw);
        let entries = out.as_array().unwrap();
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        for key in [
            "session_id",
            "title",
            "created_at",
            "updated_at",
            "message_count",
            "turn_count",
            "status",
            "archived",
            "ephemeral",
        ] {
            assert!(entry.get(key).is_some(), "missing card key {key}");
        }
        for key in ["cwd", "model", "workspace_id", "skills", "seed", "running"] {
            assert!(entry.get(key).is_none(), "host key {key} must be stripped");
        }
        assert_eq!(entry["session_id"], json!("0123abcd"));
        assert_eq!(entry["status"], json!("working"));
    }

    #[test]
    fn missing_fields_fall_back_to_null_or_false() {
        let out = sanitize_session_list(json!([{"session_id": "s1"}]));
        let entry = out.as_array().unwrap()[0].clone();
        assert_eq!(entry["session_id"], json!("s1"));
        assert_eq!(entry["title"], Value::Null);
        assert_eq!(entry["status"], json!("not_running"));
        assert_eq!(entry["archived"], json!(false));
        assert_eq!(entry["ephemeral"], json!(false));
    }

    #[test]
    fn non_array_input_becomes_empty_list() {
        assert_eq!(sanitize_session_list(json!({"x": 1})), json!([]));
        assert_eq!(sanitize_session_list(json!("nope")), json!([]));
    }
}
