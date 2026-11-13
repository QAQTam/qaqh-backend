//! JSON-LP IPC 协议帧定义与传输层。
//!
//! 协议：每行一个完整 JSON 对象（`\n` 分隔），称为一帧。
//!
//! Agent → Tools: tools_init, tool_call_req, tool_cancel, tools_shutdown
//! Tools → Agent: tools_ready, tool_progress, tool_result, tool_result_message, tool_error

use serde::{Deserialize, Serialize};

// ── 入站帧（Agent → Tools）──

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum InboundFrame {
    #[serde(rename = "tools_init")]
    Init {
        allowed_tools: Vec<String>,
        session_seed: String,
        auto_mode: bool,
    },

    #[serde(rename = "tool_call_req")]
    CallReq {
        id: String,
        name: String,
        action: String,
        args: serde_json::Value,
        /// 超时覆盖（秒），None = 使用 handler 默认值。
        timeout_secs: Option<u64>,
    },

    #[serde(rename = "tool_cancel")]
    Cancel {
        /// None = 取消所有，Some(id) = 取消指定工具。
        id: Option<String>,
    },

    #[serde(rename = "tools_shutdown")]
    Shutdown,
}

// ── 出站帧（Tools → Agent）──

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
pub enum OutboundFrame {
    /// Tools 进程就绪，附带可用工具定义列表。
    #[serde(rename = "tools_ready")]
    Ready {
        tools: Vec<dsx_types::ToolDef>,
    },

    /// 流式进度输出。
    #[serde(rename = "tool_progress")]
    Progress {
        id: String,
        content: String,
        /// "stdout" | "stderr" | "progress"
        stream_type: String,
    },

    /// 旧式工具结果（纯文本，向后兼容）。
    #[serde(rename = "tool_result")]
    Result {
        id: String,
        success: bool,
        content: String,
    },

    /// 新式工具结果消息——包含构造 Message::tool() 所需的全部字段。
    /// Agent 收到此帧后可直接构造 conversation Message，无需额外解析。
    #[serde(rename = "tool_result_message")]
    ToolResultMessage {
        id: String,
        name: String,
        action: String,
        success: bool,
        content: String,
        /// Anthropic 协议：Some(true) 表示 is_error。
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },

    /// 工具执行出错（拦截/未知工具/panic/超时）。
    #[serde(rename = "tool_error")]
    ToolError {
        id: String,
        error: String,
        /// "UNKNOWN_TOOL" | "BLOCKED" | "TIMEOUT" | "PANIC" | "FORBIDDEN"
        code: String,
    },

}

// ── 帧读写 ──

use std::io::{self, BufRead, Write};

/// 从 reader 读取下一个入站帧。返回 None 表示 EOF。
pub fn read_frame(reader: &mut impl BufRead) -> io::Result<Option<InboundFrame>> {
    let mut line = String::new();
    let n = reader.read_line(&mut line)?;
    if n == 0 {
        return Ok(None); // EOF
    }
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    match serde_json::from_str::<InboundFrame>(trimmed) {
        Ok(frame) => Ok(Some(frame)),
        Err(e) => Err(io::Error::new(io::ErrorKind::InvalidData, e)),
    }
}

/// 向 writer 写出站帧（自动追加 \n 并 flush）。
pub fn write_frame(writer: &mut impl Write, frame: &OutboundFrame) -> io::Result<()> {
    let json = serde_json::to_string(frame)?;
    writeln!(writer, "{}", json)?;
    writer.flush()?;
    Ok(())
}

// ── IPC 主循环 ──

use crate::ToolManager;

/// IPC 主循环：从 stdin 读取 Agent 帧 → ToolManager 路由 → 写入 stdout。
///
/// 仅在收到 ToolsShutdown 或 EOF 时返回。
pub fn ipc_main_loop(manager: &mut ToolManager) {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = io::BufReader::new(stdin.lock());
    let mut writer = stdout.lock();

    loop {
        let frame = match read_frame(&mut reader) {
            Ok(Some(f)) => f,
            Ok(None) => break,  // EOF
            Err(e) => {
                let _ = write_frame(&mut writer, &OutboundFrame::ToolError {
                    id: "ipc".into(),
                    error: format!("IPC parse error: {}", e),
                    code: "IPC_ERROR".into(),
                });
                continue;
            }
        };

        match frame {
            InboundFrame::Init { allowed_tools, session_seed, auto_mode } => {
                manager.apply_init(allowed_tools, &session_seed, auto_mode);
                let tools = manager.filtered_defs();
                if write_frame(&mut writer, &OutboundFrame::Ready { tools }).is_err() {
                    break; // stdout closed
                }
            }

            InboundFrame::CallReq { id, name, action, args, timeout_secs } => {
                let response = manager.handle_req(id, &name, &action, args, timeout_secs);
                if write_frame(&mut writer, &response).is_err() {
                    break;
                }
            }

            InboundFrame::Cancel { id } => {
                manager.cancel_tool(id.as_deref());
            }

            InboundFrame::Shutdown => break,
        }
    }
}
