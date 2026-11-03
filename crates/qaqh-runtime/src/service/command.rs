//! 服务面强类型命令。
//!
//! `(method, params)` 只在 [`Command::decode`] 解码一次：先按方法前缀路由到
//! 领域枚举，再由 serde 把 params 装进**已经在方法表里声明过的** `*Params`
//! 类型（`ringing::service_methods` 与这里共用同一批 DTO）。handler 拿到的是
//! 结构体字段，不再有 `pstr(params, "path")?` 式的字符串取参，也不再靠
//! `match "workspace.list" | "fs.read" | …` 分派。
//!
//! 未知方法在这里以 [`QaqhError::UnknownMethod`] 拒绝（文案与旧实现一致：
//! `unknown method: …`），领域内未知变体同样归为未知方法。

use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::error::QaqhError;

/// 服务面命令的顶层分派：一个变体 = 一个领域 handler 模块。
#[derive(Debug)]
pub enum Command {
    /// `daemon.*` / `diagnostics.*` / `config.*` / `profile.*`
    System(SystemCommand),
    /// `workspace.*` / `fs.*`
    Workspace(WorkspaceCommand),
    /// `session.*` / `skills.*` / `subagent.*`
    Session(SessionCommand),
    /// `todo.*` / `plan.*`
    Task(TaskCommand),
}

impl Command {
    /// 把 wire 的 `(method, params)` 解码成强类型命令。
    pub fn decode(method: &str, params: &Value) -> Result<Self, QaqhError> {
        let domain = method.split('.').next().unwrap_or_default();
        match domain {
            "daemon" | "diagnostics" | "config" | "profile" => {
                Ok(Self::System(decode(method, params)?))
            }
            "workspace" | "fs" => Ok(Self::Workspace(decode(method, params)?)),
            "session" | "skills" | "subagent" => Ok(Self::Session(decode(method, params)?)),
            "todo" | "plan" => Ok(Self::Task(decode(method, params)?)),
            _ => Err(QaqhError::UnknownMethod {
                method: method.to_string(),
            }),
        }
    }
}

/// 把 `method` 作为 tag 合进 params，再交给领域枚举做穷尽解码。
fn decode<T: DeserializeOwned>(method: &str, params: &Value) -> Result<T, QaqhError> {
    let mut object = match params {
        Value::Object(map) => map.clone(),
        Value::Null => Map::new(),
        other => {
            return Err(QaqhError::InvalidParams {
                method: method.to_string(),
                detail: format!("params must be a JSON object, got {other}"),
            });
        }
    };
    object.insert("method".to_string(), Value::String(method.to_string()));
    serde_json::from_value(Value::Object(object)).map_err(|error| {
        let detail = error.to_string();
        // internally tagged 枚举对未知 tag 的报错固定以 `unknown variant` 开头：
        // 那说明**方法名**不认识，而不是字段类型不对。
        if detail.starts_with("unknown variant") {
            QaqhError::UnknownMethod {
                method: method.to_string(),
            }
        } else {
            QaqhError::InvalidParams {
                method: method.to_string(),
                detail,
            }
        }
    })
}

// ─────────────────────────── System ───────────────────────────

#[derive(Debug, Deserialize)]
#[serde(tag = "method")]
pub enum SystemCommand {
    #[serde(rename = "daemon.version")]
    DaemonVersion(DaemonVersionArgs),
    #[serde(rename = "diagnostics.memory.snapshot")]
    DiagnosticsMemorySnapshot(DiagnosticsMemorySnapshotArgs),
    #[serde(rename = "config.load")]
    ConfigLoad(ConfigLoadArgs),
    #[serde(rename = "profile.list")]
    ProfileList(ProfileListArgs),
}

/// `daemon.version`：无参数。
pub type DaemonVersionArgs = crate::ringing::service_methods::DaemonVersionParams;

#[derive(Debug, Default, Deserialize)]
pub struct DiagnosticsMemorySnapshotArgs {
    /// 只回放该序号之后的快照增量；缺省 = 全量。
    pub after_sequence: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ConfigLoadArgs {}

#[derive(Debug, Default, Deserialize)]
pub struct ProfileListArgs {}

// ───────────────────────── Workspace ─────────────────────────

#[derive(Debug, Deserialize)]
#[serde(tag = "method")]
pub enum WorkspaceCommand {
    #[serde(rename = "workspace.list")]
    WorkspaceList(WorkspaceListArgs),
    #[serde(rename = "workspace.get")]
    WorkspaceGet(WorkspaceGetArgs),
    #[serde(rename = "fs.list")]
    FsList(FsListArgs),
    #[serde(rename = "fs.read")]
    FsRead(FsReadArgs),
}

pub type WorkspaceListArgs = crate::ringing::service_methods::WorkspaceListParams;
pub type WorkspaceGetArgs = crate::ringing::service_methods::WorkspaceGetParams;
pub type FsListArgs = crate::ringing::service_methods::FsListParams;
pub type FsReadArgs = crate::ringing::service_methods::FsReadParams;

// ────────────────────────── Session ──────────────────────────

#[derive(Debug, Deserialize)]
#[serde(tag = "method")]
pub enum SessionCommand {
    #[serde(rename = "session.list")]
    SessionList(SessionListArgs),
    #[serde(rename = "session.meta")]
    SessionMeta(SessionMetaArgs),
    #[serde(rename = "session.activity")]
    SessionActivity(SessionActivityArgs),
    #[serde(rename = "session.get_activity")]
    SessionGetActivity(SessionGetActivityArgs),
    #[serde(rename = "session.dashboard")]
    SessionDashboard(SessionDashboardArgs),
    #[serde(rename = "skills.catalog")]
    SkillsCatalog(SkillsCatalogArgs),
    #[serde(rename = "skills.list_tools")]
    SkillsListTools(SkillsListToolsArgs),
}

pub type SessionListArgs = crate::ringing::service_methods::SessionListParams;
pub type SessionMetaArgs = crate::ringing::service_methods::SessionMetaParams;
pub type SessionActivityArgs = crate::ringing::service_methods::SessionActivityParams;
pub type SessionGetActivityArgs = crate::ringing::service_methods::SessionGetActivityParams;

#[derive(Debug, Deserialize)]
pub struct SessionDashboardArgs {
    pub session_id: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct SkillsListToolsArgs {}

pub type SkillsCatalogArgs = crate::ringing::service_methods::SkillsCatalogParams;

// ─────────────────────────── Task ────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(tag = "method")]
pub enum TaskCommand {
    #[serde(rename = "todo.status")]
    TodoStatus(TodoStatusArgs),
    #[serde(rename = "todo.list")]
    TodoList(TodoListArgs),
    #[serde(rename = "plan.read")]
    PlanRead(PlanReadArgs),
}

#[derive(Debug, Deserialize)]
pub struct TodoStatusArgs {
    pub session_id: String,
}

#[derive(Debug, Deserialize)]
pub struct TodoListArgs {
    pub session_id: String,
    #[serde(flatten)]
    pub args: Map<String, Value>,
}

impl TodoListArgs {
    pub fn tool_args(&self) -> Value {
        Value::Object(self.args.clone())
    }
}

#[derive(Debug, Deserialize)]
pub struct PlanReadArgs {
    pub session_id: String,
}
