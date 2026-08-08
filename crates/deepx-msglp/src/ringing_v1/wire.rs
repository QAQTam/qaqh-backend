//! worker 边界线格式判别（M3 后：仅 Ringing 线格式）。
//!
//! 硬规则：**reader 必须先检查 `wire`，禁止 untagged 猜测**。
//!
//! - Ringing 记录：携带 `wire: "Ringing_domain_v1"`，解析为
//!   `RingingWorkerCommandEnvelope` / `RingingWorkerEventEnvelope`。
//! - 缺失/未知 `wire` 值：拒绝并报 `InvalidData`，绝不猜测
//!   （legacy 无 `wire` 帧已在 M3 完全拆除）。

use std::io::{BufRead, Write};

use deepx_ringing::worker::{
    RingingTimelineIntentEnvelope, RingingWorkerCommandEnvelope, WIRE_RINGING_DOMAIN_V1,
    WIRE_RINGING_TIMELINE_INTENT_V1,
};

/// stdin 方向的可判别命令帧。
#[derive(Debug, Clone)]
pub enum WorkerCommandFrame {
    /// Ringing `RingingWorkerCommandEnvelope`（`wire: "Ringing_domain_v1"`）。
    Ringing(RingingWorkerCommandEnvelope),
}

/// 读取一行并判别帧类型。空行返回 `Ok(None)`。
///
/// 判别规则（顺序固定）：
/// 1. 解析为 `serde_json::Value`；
/// 2. 无 `wire` 字段 → `InvalidData`（legacy 帧已拆除）；
/// 3. `wire == "Ringing_domain_v1"` → Ringing envelope；
/// 4. 其它 `wire` 值 → `InvalidData`（禁止猜测）。
pub fn read_worker_command_frame<R: BufRead>(
    r: &mut R,
) -> std::io::Result<Option<WorkerCommandFrame>> {
    let mut line = String::new();
    let n = r.read_line(&mut line)?;
    if n == 0 || line.trim().is_empty() {
        return Ok(None);
    }
    let value: serde_json::Value = serde_json::from_str(&line)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    match value.get("wire") {
        None => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "worker command frame missing `wire` tag (legacy frames removed in M3)",
        )),
        Some(w) if w == WIRE_RINGING_DOMAIN_V1 => {
            let frame = serde_json::from_value::<RingingWorkerCommandEnvelope>(value)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
            Ok(Some(WorkerCommandFrame::Ringing(frame)))
        }
        Some(other) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("unknown worker wire tag: {other}"),
        )),
    }
}

/// 写入 Ringing 事件帧（切流后使用；携带 `wire` 判别字段）。
pub fn write_ringing_event_frame<W: Write>(
    w: &mut W,
    env: &deepx_ringing::RingingWorkerEventEnvelope,
) -> std::io::Result<()> {
    let json = serde_json::to_string(env)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    writeln!(w, "{json}")?;
    w.flush()
}

/// Writes a native Ringing V1 timeline intent frame.
pub fn write_timeline_intent_frame<W: Write>(
    w: &mut W,
    env: &RingingTimelineIntentEnvelope,
) -> std::io::Result<()> {
    let json = serde_json::to_string(env)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    writeln!(w, "{json}")?;
    w.flush()
}

/// 校验 worker stdout 行（daemon 侧使用）：仅接受 Ringing / timeline 事件。
pub fn read_worker_event_line(line: &str) -> Result<(), String> {
    let value: serde_json::Value =
        serde_json::from_str(line).map_err(|e| format!("invalid JSON: {e}"))?;
    match value.get("wire") {
        None => Err("legacy event frame without `wire` tag is no longer supported".into()),
        Some(w) if w == WIRE_RINGING_DOMAIN_V1 => {
            let env = serde_json::from_value::<deepx_ringing::RingingWorkerEventEnvelope>(value)
                .map_err(|e| format!("invalid ringing event: {e}"))?;
            let _ = env.event.channel();
            Ok(())
        }
        Some(w) if w == WIRE_RINGING_TIMELINE_INTENT_V1 => {
            let _ = serde_json::from_value::<RingingTimelineIntentEnvelope>(value)
                .map_err(|e| format!("invalid timeline intent: {e}"))?;
            Ok(())
        }
        Some(other) => Err(format!("unknown worker wire tag: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deepx_ringing::RingingCommand as RC;

    #[test]
    fn command_without_wire_tag_is_rejected() {
        // M3：legacy 无 `wire` 帧已拆除，缺失 `wire` 直接拒绝。
        let line = r#"{"type":"user_input","text":"hi"}"#;
        let mut reader = std::io::Cursor::new(format!("{line}\n"));
        let err = read_worker_command_frame(&mut reader).expect_err("must reject");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("missing `wire` tag"));
    }

    #[test]
    fn ringing_command_with_wire_tag_is_parsed() {
        let env = RingingWorkerCommandEnvelope::new(
            "s1",
            "cmd-1",
            RC::Conversation(deepx_domain::ConversationCommand::ConversationCancel {
                turn_id: None,
            }),
        );
        let json = serde_json::to_string(&env).expect("serialize");
        assert!(json.contains("\"wire\":\"Ringing_domain_v1\""));
        let mut reader = std::io::Cursor::new(format!("{json}\n"));
        let frame = read_worker_command_frame(&mut reader)
            .expect("read")
            .expect("frame");
        match frame {
            WorkerCommandFrame::Ringing(parsed) => {
                assert_eq!(parsed.command_id, "cmd-1");
                assert_eq!(parsed.seed, "s1");
            }
            other => panic!("expected ringing frame, got {other:?}"),
        }
    }

    #[test]
    fn unknown_wire_tag_is_rejected() {
        let mut reader =
            std::io::Cursor::new("{\"wire\":\"something_else\",\"type\":\"cancel\"}\n");
        let err = read_worker_command_frame(&mut reader).expect_err("must reject");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn blank_line_returns_none() {
        let mut reader = std::io::Cursor::new("\n\n");
        assert!(
            read_worker_command_frame(&mut reader)
                .expect("read")
                .is_none()
        );
    }

    #[test]
    fn ringing_command_remains_typed_at_worker_boundary() {
        let env = RingingWorkerCommandEnvelope::new(
            "s",
            "c",
            RC::Conversation(deepx_domain::ConversationCommand::ConversationCancel {
                turn_id: None,
            }),
        );
        let value = serde_json::to_value(&env).expect("serialize");
        assert_eq!(value["wire"], "Ringing_domain_v1");
        assert_eq!(value["command"]["channel"], "conversation");
        assert_eq!(value["command"]["type"], "conversation_cancel");
    }

    #[test]
    fn worker_event_line_discrimination() {
        // legacy 事件行（M3 后拒绝）
        let err = read_worker_event_line(r#"{"type":"ready"}"#).expect_err("must reject");
        assert!(err.contains("no longer supported"));

        // Ringing 事件行（校验通过，跳过）
        let env = deepx_ringing::RingingWorkerEventEnvelope::new(
            "s",
            "evt-1",
            deepx_ringing::RingingEvent::Conversation(
                deepx_domain::ConversationEvent::ConversationCancelled { turn_id: None },
            ),
        );
        let json = serde_json::to_string(&env).expect("serialize");
        read_worker_event_line(&json).expect("ringing ok");

        // 未知 wire 拒绝
        let err =
            read_worker_event_line(r#"{"wire":"nope","type":"ready"}"#).expect_err("must reject");
        assert!(err.contains("unknown worker wire tag"));
    }

    #[test]
    fn event_frame_writers_round_trip() {
        let mut buf = Vec::new();
        let env = deepx_ringing::RingingWorkerEventEnvelope::new(
            "s",
            "e",
            deepx_ringing::RingingEvent::Tool(deepx_domain::ToolEvent::ToolStarted {
                tool_call_id: "c".into(),
                turn_id: "t".into(),
                round_num: 0,
                name: "exec".into(),
            }),
        );
        write_ringing_event_frame(&mut buf, &env).expect("ringing write");
        let text = String::from_utf8_lossy(&buf);
        assert!(text.contains("\"wire\":\"Ringing_domain_v1\""));
        assert!(text.contains("\"direction\":\"event\""));
    }

    #[test]
    fn malformed_line_is_invalid_data() {
        let mut reader = std::io::Cursor::new("not-json\n");
        let err = read_worker_command_frame(&mut reader).expect_err("must reject");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }
}
