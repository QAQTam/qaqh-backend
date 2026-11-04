//! 执行契约基础类型:工具调用 outcome 与执行会话身份的唯一权威定义。
//!
//! 本模块只依赖 serde/JSON,不依赖任何上层 crate;ringing-contract、
//! tool-core、process-tools、session 按需引用同一份 Rust 类型,不经过
//! 双类型或 From 转换。依赖方向只向本 crate 下沉(spec-exec-session-lifecycle
//! §4.0);wire 形状与 TS 公共命名由本模块直接锁定。

use serde::{Deserialize, Serialize};
#[cfg(feature = "ts")]
use ts_rs::TS;

/// 执行会话身份:可续读、输入和停止的执行会话,格式 `execs_<ULID>`。
///
/// 系统 PID、旧 registry 整数 ID 都是内部信息,不作为客户端或模型的操作凭据。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct ExecSessionId(pub String);

impl ExecSessionId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_valid(text: &str) -> bool {
        let body = text.strip_prefix("execs_").unwrap_or(text);
        // ULID:26 字符 Crockford base32(见 ringing-contract::ulid)。
        body.len() == 26
            && body.bytes().all(|byte| {
                byte.is_ascii_digit()
                    || matches!(byte, b'A'..=b'H' | b'J'..=b'N' | b'P'..=b'T' | b'V'..=b'Z')
            })
    }
}

impl AsRef<str> for ExecSessionId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl std::fmt::Display for ExecSessionId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// 规范内容摘要(sha256 指针形式):`sha256:<hex>` 之类字符串值对象。
///
/// 自 ringing-contract 下沉;wire 含义与 TS 命名(`ContentHash`)不变。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct ContentHash(pub String);

impl ContentHash {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// 会话内容引用:对已落库 canonical 内容的哈希引用。
///
/// 自 ringing-contract 下沉;TS 公共命名保持 `SessionContentRef`。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct SessionContentRef(pub ContentHash);

impl SessionContentRef {
    pub fn new(hash: ContentHash) -> Self {
        Self(hash)
    }

    pub fn hash(&self) -> &ContentHash {
        &self.0
    }
}

/// 会话工具错误的规范错误对象。
///
/// 自 ringing-contract 下沉;TS 公共命名保持 `SessionToolError`。错误详情放
/// 规范错误对象,不通过增加随意 reason 字符串绕过枚举。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct SessionToolError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details_ref: Option<SessionContentRef>,
}

/// 一次工具调用的不可变公共 outcome。
///
/// `completed/yielded` 都是一次调用的成功返回,不带 error;`failed` 是已确认
/// 失败;`cancelled` 是已确认取消;`interrupted` 是事实无法继续确认或执行
/// 中断,必须保留原因。finished 的 outcome 不因未来进程退出、进程失败、
/// 重新连接而改写(spec §4.2)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolCallOutcome {
    Completed,
    /// 调用成功返回且执行会话仍在运行(后台继续执行);不是失败。
    Yielded {
        exec_session_id: ExecSessionId,
    },
    Failed {
        error: SessionToolError,
    },
    Cancelled {
        reason: ToolCancellationReason,
    },
    Interrupted {
        reason: ToolInterruptionReason,
    },
}

impl ToolCallOutcome {
    /// outcome 是否代表一次成功返回(completed/yielded)。
    pub fn is_success(&self) -> bool {
        matches!(
            self,
            ToolCallOutcome::Completed | ToolCallOutcome::Yielded { .. }
        )
    }

    /// outcome 关联的执行会话(仅 yielded)。
    pub fn exec_session_id(&self) -> Option<&ExecSessionId> {
        match self {
            ToolCallOutcome::Yielded { exec_session_id } => Some(exec_session_id),
            _ => None,
        }
    }
}

/// 工具调用的取消原因。非 exec 工具与 exec 工具共用;发起者分开记录,
/// 不能把模型发送的中断标为用户主动取消。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[serde(rename_all = "snake_case")]
pub enum ToolCancellationReason {
    /// conversation.cancel 取消整个回合。
    TurnCancel,
    /// Ask/交互被主动关闭,只取消该次交互。
    InteractionDismissed,
    /// 会话关闭/停止。
    SessionShutdown,
}

/// 工具调用的中断原因:事实无法继续确认或执行被外部中断。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[serde(rename_all = "snake_case")]
pub enum ToolInterruptionReason {
    /// daemon 重启,执行归属丢失。
    DaemonRestart,
    /// 执行失联(进程观测不到,效果未知)。
    LostExecution,
    /// 升级迁移:旧数据无法确认原始事实。
    LegacyStateUnknown,
    /// 执行结果未获确认(发出停止信号但未观察到真实终态)。
    ExecutionUnconfirmed,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_session_id_validates_ulid_body() {
        let id = ExecSessionId::new("execs_01ARZ3NDEKTSV4RRFFQ69G5FAV");
        assert!(ExecSessionId::is_valid(id.as_str()));
        assert!(ExecSessionId::is_valid("01ARZ3NDEKTSV4RRFFQ69G5FAV"));
        assert!(!ExecSessionId::is_valid("execs_0"));
        assert!(!ExecSessionId::is_valid("execs_IOLU0194NOTULIDCHARS"));
        // I/O/L/U 不是 Crockford 字母表成员。
        assert!(!ExecSessionId::is_valid("execs_IOLU0194NOTULIDCHARSXXX"));
    }

    #[test]
    fn outcome_wire_shape_is_tagged_union() {
        let yielded = ToolCallOutcome::Yielded {
            exec_session_id: ExecSessionId::new("execs_01ARZ3NDEKTSV4RRFFQ69G5FAV"),
        };
        let value = serde_json::to_value(&yielded).unwrap();
        assert_eq!(value["kind"], "yielded");
        assert_eq!(value["exec_session_id"], "execs_01ARZ3NDEKTSV4RRFFQ69G5FAV");

        let failed = ToolCallOutcome::Failed {
            error: SessionToolError {
                code: "exit_nonzero".into(),
                message: "exit code 1".into(),
                retryable: false,
                details_ref: None,
            },
        };
        let value = serde_json::to_value(&failed).unwrap();
        assert_eq!(value["kind"], "failed");
        assert_eq!(value["error"]["code"], "exit_nonzero");

        let completed = ToolCallOutcome::Completed;
        let value = serde_json::to_value(&completed).unwrap();
        assert_eq!(value, serde_json::json!({"kind": "completed"}));
    }

    #[test]
    fn session_content_ref_is_transparent_on_wire() {
        let value =
            serde_json::to_value(&SessionContentRef::new(ContentHash::new("sha256:abcd"))).unwrap();
        assert_eq!(value, serde_json::json!("sha256:abcd"));
    }
}
