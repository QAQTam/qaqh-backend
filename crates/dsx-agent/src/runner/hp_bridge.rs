//! HP (Health Platform) TCP bridge: stream reading, frame dispatch, result emission.

use std::io::BufReader;
use std::net::TcpStream;
use std::sync::mpsc;

use dsx_proto::{self, Agent2Ui};

/// Send a ToolResult frame via the agent-to-TUI channel (`agent_tx`).
pub fn emit_tool_result(
    tx: &mpsc::Sender<Agent2Ui>,
    id: &str,
    name: &str,
    content: &str,
    success: bool,
    args: Option<String>,
) {
    let _ = tx.send(Agent2Ui::ToolResult {
        id: id.to_string(),
        name: name.to_string(),
        content: content.to_string(),
        success,
        args,
    });
}

/// Read one frame from the HP TCP stream.
/// Returns `Ok(Some(frame))` on success, `Ok(None)` on EOF,
/// `Err(message)` on parse error.
pub fn read_hp_frame(
    hp: &mut BufReader<TcpStream>,
) -> Result<Option<dsx_proto::HpToAgent>, String> {
    match dsx_proto::read_frame(hp) {
        Ok(Some(r)) => Ok(Some(r)),
        Ok(None) => {
            log::warn!("dsx-agent: HP connection closed (EOF)");
            Err("HP connection closed unexpectedly.".into())
        }
        Err(e) => {
            log::warn!("dsx-agent: HP parse error: {e}");
            Err(format!("HP protocol error: {}", e))
        }
    }
}