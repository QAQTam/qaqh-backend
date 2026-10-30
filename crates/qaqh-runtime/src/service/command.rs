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
use serde::{Deserialize, Serialize};
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
    #[serde(rename = "diagnostics.memory.start")]
    DiagnosticsMemoryStart(DiagnosticsMemoryStartArgs),
    #[serde(rename = "diagnostics.memory.stop")]
    DiagnosticsMemoryStop(DiagnosticsMemoryStopArgs),
    #[serde(rename = "diagnostics.memory.snapshot")]
    DiagnosticsMemorySnapshot(DiagnosticsMemorySnapshotArgs),
    #[serde(rename = "config.load")]
    ConfigLoad(ConfigLoadArgs),
    #[serde(rename = "config.save")]
    ConfigSave(ConfigSaveArgs),
    #[serde(rename = "config.set_permission_level")]
    ConfigSetPermissionLevel(ConfigSetPermissionLevelArgs),
    #[serde(rename = "profile.list")]
    ProfileList(ProfileListArgs),
    #[serde(rename = "profile.set_default")]
    ProfileSetDefault(ProfileSetDefaultArgs),
    #[serde(rename = "profile.create")]
    ProfileCreate(ProfileCreateArgs),
    #[serde(rename = "profile.apply")]
    ProfileApply(ProfileApplyArgs),
    #[serde(rename = "profile.save_current")]
    ProfileSaveCurrent(ProfileSaveCurrentArgs),
    #[serde(rename = "profile.delete")]
    ProfileDelete(ProfileDeleteArgs),
}

/// `daemon.version`：无参数。
pub type DaemonVersionArgs = crate::ringing::service_methods::DaemonVersionParams;

#[derive(Debug, Default, Deserialize)]
pub struct DiagnosticsMemoryStartArgs {}

#[derive(Debug, Default, Deserialize)]
pub struct DiagnosticsMemoryStopArgs {}

#[derive(Debug, Default, Deserialize)]
pub struct DiagnosticsMemorySnapshotArgs {
    /// 只回放该序号之后的快照增量；缺省 = 全量。
    pub after_sequence: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ConfigLoadArgs {}

/// `config.save` 的 payload 是 **merge patch 本体**：字段集合由
/// `qaqh_config_api::ConfigPatch` 决定，这里原样透传，避免再抄一遍字段表。
#[derive(Debug, Deserialize)]
pub struct ConfigSaveArgs {
    #[serde(flatten)]
    pub patch: Map<String, Value>,
}

#[derive(Debug, Deserialize)]
pub struct ConfigSetPermissionLevelArgs {
    /// 1 = read-only，2 = workspace-write，3 = skip-permissions。
    pub level: u8,
}

#[derive(Debug, Default, Deserialize)]
pub struct ProfileListArgs {}

#[derive(Debug, Deserialize)]
pub struct ProfileSetDefaultArgs {
    pub id: Option<String>,
    pub name: Option<String>,
}

impl ProfileSetDefaultArgs {
    /// 旧协议允许 `id` 或 `name` 二选一。
    pub fn target(&self) -> Result<&str, QaqhError> {
        self.id
            .as_deref()
            .or(self.name.as_deref())
            .ok_or_else(|| QaqhError::Invalid {
                message: "missing 'id' or 'name' parameter".to_string(),
            })
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProfileCreateArgs {
    pub name: String,
    pub model: String,
    pub id: Option<String>,
    pub provider: Option<String>,
    #[serde(rename = "baseUrl", alias = "base_url")]
    pub base_url: Option<String>,
    pub wire: Option<String>,
    #[serde(
        rename = "reasoningEffort",
        alias = "reasoning_effort",
        alias = "effort"
    )]
    pub reasoning_effort: Option<String>,
    #[serde(rename = "contextLength", alias = "context_length")]
    pub context_length: Option<u32>,
    #[serde(rename = "maxTokens", alias = "max_tokens")]
    pub max_tokens: Option<u32>,
    #[serde(rename = "apiKey", alias = "api_key")]
    pub api_key: Option<String>,
}

impl ProfileCreateArgs {
    /// 显式 id 缺省时用 name；两端空白裁掉（旧实现同款）。
    pub fn profile_id(&self) -> String {
        self.id.as_deref().unwrap_or(&self.name).trim().to_string()
    }

    pub fn api_key(&self) -> Option<&str> {
        self.api_key.as_deref()
    }

    pub fn wire(&self) -> qaqh_types::Wire {
        qaqh_types::Wire::parse(self.wire.as_deref().unwrap_or("chat")).unwrap_or_default()
    }
}

#[derive(Debug, Deserialize)]
pub struct ProfileApplyArgs {
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct ProfileSaveCurrentArgs {
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct ProfileDeleteArgs {
    pub name: String,
}

// ───────────────────────── Workspace ─────────────────────────

#[derive(Debug, Deserialize)]
#[serde(tag = "method")]
pub enum WorkspaceCommand {
    #[serde(rename = "workspace.list")]
    WorkspaceList(WorkspaceListArgs),
    #[serde(rename = "workspace.get")]
    WorkspaceGet(WorkspaceGetArgs),
    #[serde(rename = "workspace.set")]
    WorkspaceSet(WorkspaceSetArgs),
    #[serde(rename = "workspace.create")]
    WorkspaceCreate(WorkspaceCreateArgs),
    #[serde(rename = "workspace.rename")]
    WorkspaceRename(WorkspaceRenameArgs),
    #[serde(rename = "workspace.delete")]
    WorkspaceDelete(WorkspaceDeleteArgs),
    #[serde(rename = "workspace.move_session")]
    WorkspaceMoveSession(WorkspaceMoveSessionArgs),
    #[serde(rename = "workspace.detach")]
    WorkspaceDetach(WorkspaceDetachArgs),
    #[serde(rename = "fs.list")]
    FsList(FsListArgs),
    #[serde(rename = "fs.read")]
    FsRead(FsReadArgs),
}

pub type WorkspaceListArgs = crate::ringing::service_methods::WorkspaceListParams;
pub type WorkspaceGetArgs = crate::ringing::service_methods::WorkspaceGetParams;
pub type WorkspaceSetArgs = crate::ringing::service_methods::WorkspaceSetParams;
pub type WorkspaceCreateArgs = crate::ringing::service_methods::WorkspaceCreateParams;
pub type WorkspaceRenameArgs = crate::ringing::service_methods::WorkspaceRenameParams;
pub type WorkspaceDeleteArgs = crate::ringing::service_methods::WorkspaceDeleteParams;
pub type WorkspaceMoveSessionArgs = crate::ringing::service_methods::WorkspaceMoveSessionParams;
pub type WorkspaceDetachArgs = crate::ringing::service_methods::WorkspaceDetachParams;
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
    #[serde(rename = "session.new")]
    SessionNew(SessionNewArgs),
    #[serde(rename = "session.create")]
    SessionCreate(SessionCreateArgs),
    #[serde(rename = "session.resume")]
    SessionResume(SessionResumeArgs),
    #[serde(rename = "session.set_tool_mode")]
    SessionSetToolMode(SessionSetToolModeArgs),
    #[serde(rename = "session.set_profile")]
    SessionSetProfile(SessionSetProfileArgs),
    #[serde(rename = "session.set_effort")]
    SessionSetEffort(SessionSetEffortArgs),
    #[serde(rename = "session.rename")]
    SessionRename(SessionRenameArgs),
    #[serde(rename = "session.dashboard")]
    SessionDashboard(SessionDashboardArgs),
    #[serde(rename = "skills.operation")]
    SkillsOperation(SkillsOperationArgs),
    #[serde(rename = "skills.reload")]
    SkillsReload(SkillsSessionArgs),
    #[serde(rename = "skills.activate")]
    SkillsActivate(SkillsActivateArgs),
    #[serde(rename = "skills.list_tools")]
    SkillsListTools(SkillsListToolsArgs),
    #[serde(rename = "subagent.spawn")]
    SubagentSpawn(SubagentSpawnArgs),
}

pub type SessionListArgs = crate::ringing::service_methods::SessionListParams;
pub type SessionMetaArgs = crate::ringing::service_methods::SessionMetaParams;
pub type SessionActivityArgs = crate::ringing::service_methods::SessionActivityParams;
pub type SessionGetActivityArgs = crate::ringing::service_methods::SessionGetActivityParams;
pub type SessionCreateArgs = crate::ringing::service_methods::SessionCreateParams;
pub type SessionSetToolModeArgs = crate::ringing::service_methods::SessionSetToolModeParams;
pub type SessionSetProfileArgs = crate::ringing::service_methods::SessionSetProfileParams;
pub type SessionSetEffortArgs = crate::ringing::service_methods::SessionSetEffortParams;
pub type SessionRenameArgs = crate::ringing::service_methods::SessionRenameParams;

#[derive(Debug, Default, Deserialize)]
pub struct SessionNewArgs {
    /// 可选 cwd：前端在 workspace 上下文新建时传入。
    pub cwd: Option<String>,
    /// 可选工具模式预置；空串 = 未指定。
    pub tool_mode: Option<String>,
    pub custom_tools: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub struct SessionResumeArgs {
    pub session_id: String,
}

#[derive(Debug, Deserialize)]
pub struct SessionDashboardArgs {
    pub session_id: String,
}

#[derive(Debug, Deserialize)]
pub struct SkillsOperationArgs {
    pub session_id: String,
    #[serde(rename = "operation_id", alias = "operationId")]
    pub operation_id: String,
    pub action: String,
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct SkillsSessionArgs {
    pub session_id: String,
}

#[derive(Debug, Deserialize)]
pub struct SkillsActivateArgs {
    pub session_id: String,
    pub name: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct SkillsListToolsArgs {}

#[derive(Debug, Default, Deserialize)]
pub struct SubagentSpawnArgs {
    pub tools: Option<Vec<String>>,
    pub model: Option<String>,
    #[serde(rename = "base_url", alias = "baseUrl")]
    pub base_url: Option<String>,
    #[serde(rename = "max_tokens", alias = "maxTokens")]
    pub max_tokens: Option<u32>,
    pub workspace: Option<String>,
}

// ─────────────────────────── Task ────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(tag = "method")]
pub enum TaskCommand {
    #[serde(rename = "todo.status")]
    TodoStatus(TodoStatusArgs),
    #[serde(rename = "todo.cancel")]
    TodoCancel(TodoCancelArgs),
    #[serde(rename = "todo.set")]
    TodoSet(TodoSetArgs),
    #[serde(rename = "todo.list")]
    TodoList(TodoListArgs),
    #[serde(rename = "plan.read")]
    PlanRead(PlanReadArgs),
    #[serde(rename = "plan.action")]
    PlanAction(PlanActionArgs),
}

#[derive(Debug, Deserialize)]
pub struct TodoStatusArgs {
    pub session_id: String,
}

#[derive(Debug, Deserialize)]
pub struct TodoCancelArgs {
    pub session_id: String,
    pub id: String,
}

/// `todo.set` 的业务参数整体交给 `qaqh_workspace::todo` 的 typed 入口——
/// 那边的 `TodoUpdateArgs` 才是唯一字段表，这里只负责拆出会话键。
#[derive(Debug, Deserialize)]
pub struct TodoSetArgs {
    pub session_id: String,
    #[serde(flatten)]
    pub args: Map<String, Value>,
}

impl TodoSetArgs {
    pub fn tool_args(&self) -> Value {
        Value::Object(self.args.clone())
    }
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

#[derive(Debug, Deserialize)]
pub struct PlanActionArgs {
    pub session_id: String,
    #[serde(rename = "item_id", alias = "itemId")]
    pub item_id: String,
    pub action: String,
    #[serde(rename = "user_comment", alias = "userComment")]
    pub user_comment: Option<String>,
}
