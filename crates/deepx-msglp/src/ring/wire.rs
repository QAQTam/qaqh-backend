//! worker 边界线格式判别（PLAN 阶段 1）。
//!
//! 硬规则：**reader 必须先检查 `wire`，禁止 untagged 猜测**。
//!
//! - legacy 记录：无 `wire` 字段，保持原 JSON-LP 格式（`Ui2Agent` / `Agent2Ui`）。
//! - Ringing 记录：携带 `wire: "Ringing_domain_v2"`，解析为
//!   `RingingWorkerCommandEnvelope` / `RingingWorkerEventEnvelope`。
//! - 未知 `wire` 值：拒绝并报 `InvalidData`，绝不猜测。

use std::io::{BufRead, Write};

use deepx_proto::{Agent2Ui, Ui2Agent};
use deepx_ringing::worker::{
    RingingWorkerCommandEnvelope, TimelineWorkerIntentEnvelope, WIRE_RINGING_DOMAIN_V2,
    WIRE_TIMELINE_INTENT_V3,
};

/// stdin 方向的可判别命令帧。
#[derive(Debug, Clone)]
pub enum WorkerCommandFrame {
    /// legacy `Ui2Agent`（无 `wire` 字段）。
    Legacy(Ui2Agent),
    /// Ringing `RingingWorkerCommandEnvelope`（`wire: "Ringing_domain_v2"`）。
    Ringing(RingingWorkerCommandEnvelope),
}

/// 读取一行并判别帧类型。空行返回 `Ok(None)`。
///
/// 判别规则（顺序固定）：
/// 1. 解析为 `serde_json::Value`；
/// 2. 无 `wire` 字段 → legacy（保持旧格式兼容）；
/// 3. `wire == "Ringing_domain_v2"` → Ringing envelope；
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
        None => {
            // legacy：反序列化为 Ui2Agent
            let frame = serde_json::from_value::<Ui2Agent>(value)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
            Ok(Some(WorkerCommandFrame::Legacy(frame)))
        }
        Some(w) if w == WIRE_RINGING_DOMAIN_V2 => {
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

/// 写入 legacy 事件帧（默认模式；worker 未切流前保持该格式）。
pub fn write_legacy_event_frame<W: Write>(w: &mut W, event: &Agent2Ui) -> std::io::Result<()> {
    let json = serde_json::to_string(event)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    writeln!(w, "{json}")?;
    w.flush()
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

/// Writes a native Timeline v3 intent frame.
pub fn write_timeline_intent_frame<W: Write>(
    w: &mut W,
    env: &TimelineWorkerIntentEnvelope,
) -> std::io::Result<()> {
    let json = serde_json::to_string(env)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    writeln!(w, "{json}")?;
    w.flush()
}

/// 判别 worker stdout 行：Ringing 事件 vs legacy 事件（daemon 侧使用）。
///
/// 返回 `None` 表示该行是 Ringing 事件（daemon 尚未接入领域消费时跳过）；
/// 返回 `Some(Agent2Ui)` 表示 legacy 事件。
pub fn read_worker_event_line(line: &str) -> Result<Option<Agent2Ui>, String> {
    let value: serde_json::Value =
        serde_json::from_str(line).map_err(|e| format!("invalid JSON: {e}"))?;
    match value.get("wire") {
        None => serde_json::from_value::<Agent2Ui>(value)
            .map(Some)
            .map_err(|e| format!("invalid legacy event: {e}")),
        Some(w) if w == WIRE_RINGING_DOMAIN_V2 => {
            // 解析校验后跳过（领域消费路径由 ChannelRouter 在 T2/T6 接入）
            let env = serde_json::from_value::<deepx_ringing::RingingWorkerEventEnvelope>(value)
                .map_err(|e| format!("invalid ringing event: {e}"))?;
            let _ = env.event.channel();
            Ok(None)
        }
        Some(w) if w == WIRE_TIMELINE_INTENT_V3 => {
            let _ = serde_json::from_value::<TimelineWorkerIntentEnvelope>(value)
                .map_err(|e| format!("invalid timeline intent: {e}"))?;
            Ok(None)
        }
        Some(other) => Err(format!("unknown worker wire tag: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deepx_ringing::RingingCommand as RC;

    #[test]
    fn legacy_command_without_wire_is_preserved() {
        let line = r#"{"type":"user_input","text":"hi"}"#;
        let mut reader = std::io::Cursor::new(format!("{line}\n"));
        let frame = read_worker_command_frame(&mut reader)
            .expect("read")
            .expect("frame");
        match frame {
            WorkerCommandFrame::Legacy(Ui2Agent::UserInput { text, .. }) => assert_eq!(text, "hi"),
            other => panic!("expected legacy user_input, got {other:?}"),
        }
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
        assert!(json.contains("\"wire\":\"Ringing_domain_v2\""));
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
        assert_eq!(value["wire"], "Ringing_domain_v2");
        assert_eq!(value["command"]["channel"], "conversation");
        assert_eq!(value["command"]["type"], "conversation_cancel");
    }

    #[test]
    fn worker_event_line_discrimination() {
        // legacy 事件行
        let legacy = r#"{"type":"ready"}"#;
        let parsed = read_worker_event_line(legacy).expect("legacy ok");
        assert!(matches!(parsed, Some(Agent2Ui::Ready)));

        // Ringing 事件行（校验通过，跳过）
        let env = deepx_ringing::RingingWorkerEventEnvelope::new(
            "s",
            "evt-1",
            deepx_ringing::RingingEvent::Conversation(
                deepx_domain::ConversationEvent::ConversationCancelled { turn_id: None },
            ),
        );
        let json = serde_json::to_string(&env).expect("serialize");
        let parsed = read_worker_event_line(&json).expect("ringing ok");
        assert!(parsed.is_none(), "ringing events are skipped pre-router");

        // 未知 wire 拒绝
        let err =
            read_worker_event_line(r#"{"wire":"nope","type":"ready"}"#).expect_err("must reject");
        assert!(err.contains("unknown worker wire tag"));
    }

    #[test]
    fn event_frame_writers_round_trip() {
        let mut buf = Vec::new();
        write_legacy_event_frame(&mut buf, &Agent2Ui::Ready).expect("legacy write");
        assert!(String::from_utf8_lossy(&buf).contains("\"type\":\"ready\""));

        let mut buf2 = Vec::new();
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
        write_ringing_event_frame(&mut buf2, &env).expect("ringing write");
        let text = String::from_utf8_lossy(&buf2);
        assert!(text.contains("\"wire\":\"Ringing_domain_v2\""));
        assert!(text.contains("\"direction\":\"event\""));
    }

    #[test]
    fn malformed_line_is_invalid_data() {
        let mut reader = std::io::Cursor::new("not-json\n");
        let err = read_worker_command_frame(&mut reader).expect_err("must reject");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }
}
