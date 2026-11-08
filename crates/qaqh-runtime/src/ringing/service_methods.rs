//! Ringing 服务面方法表（`POST /ringing/v2/service/{method}` 的单一权威清单）。
//!
//! 旧 `/queries/{name}`（闭表白名单）与 `/actions/{name}`（前缀 allowlist）
//! 双端点已合并于此，其 slash/dot 双别名容忍一并拆除：单一规范形态
//! `module.method`，一个方法一个条目，`Read` = 无副作用查询，`Write` = 变更操作。
//!
//! 会话写操作走唯一 RingingCommand 目录；服务面只保留查询与账户 RPC。

use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

pub mod accounts;

use crate::QaqhService;

/// 方法类别：决定错误码形状（`query_failed` / `action_failed`），
/// 也是对调用方"只读 / 变更"的契约声明。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MethodKind {
    Read,
    Write,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MethodInfo {
    pub kind: MethodKind,
    /// 要求会话参数并做 lease 归属校验（Read 中带会话作用域的子集；
    /// Write 一律在 params 携带会话键时校验归属）。
    pub requires_session: bool,
    /// Process-wide diagnostics expose other sessions and require admin scope.
    pub admin_only: bool,
}

/// Wire params for the typed service-method slice. Optional TS derive is used
/// only by the contract exporter; serde stays the runtime authority.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct DaemonVersionParams {}

/// Remote filesystem browsing accepts an optional session scope that narrows
/// access to that session's workspace; it is not a lease-bound session call.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct FsListParams {
    pub path: String,
    #[cfg_attr(feature = "ts", ts(optional))]
    pub scope_session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct FsReadParams {
    pub path: String,
    #[cfg_attr(feature = "ts", ts(optional))]
    pub max_bytes: Option<u64>,
    #[cfg_attr(feature = "ts", ts(optional))]
    pub scope_session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct FsEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub is_file: bool,
    pub size: u64,
    pub modified_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct FsReadResult {
    pub path: String,
    pub size: u64,
    pub truncated: bool,
    pub content: String,
}

/// `session.list` optionally includes archived sessions.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct SessionListParams {
    #[serde(default)]
    pub include_archived: bool,
}

/// `session.meta` 定位单条会话；meta 不存在时 result 为 `null`（键不缺、
/// 整个结果就是 null）。
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct SessionMetaParams {
    pub session_id: String,
}

/// `session.activity` 无参数；params 恒为 `{}`。结果条目是
/// `qaqh_ringing_contract::SessionActivity`（daemon `/activity` 观测端点同款形状）。
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct SessionActivityParams {}

/// `session.get_activity` 定位单条会话，返回该会话的工具执行记录
/// （`SessionToolActivity[]`，最近优先）。
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct SessionGetActivityParams {
    pub session_id: String,
}

/// `exec_session.list` returns one session's active executions and recent terminal summaries.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct ExecSessionListParams {
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct ExecSessionListResult {
    pub sessions: Vec<qaqh_ringing_contract::ExecSessionView>,
}

/// Load one persisted turn by its canonical turn id for session-level origin links.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct SessionTurnParams {
    pub session_id: String,
    pub turn_id: String,
}

/// `session.get_activity` 的单条工具执行记录。`args` 是调用参数的
/// JSON 字符串（原样保留模型侧形状，不二次结构化）；`finished_at_ms`
/// 是该次调用终态事实的 epoch 毫秒（与 `ToolFinished.finished_at_ms` 同源）。
/// 旧字段 `time: String` 曾同时承载 "msg_id" 与 "epoch 毫秒字符串" 两种语义，
/// 现拆为强类型。
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct SessionToolActivity {
    pub tool_name: String,
    pub summary: String,
    pub status: qaqh_types::ToolStatus,
    pub finished_at_ms: i64,
    pub args: String,
}

/// `skills.catalog` 参数：按 `cwd` 发现技能目录，缺省取 daemon 的当前目录。
///
/// daemon 已完成解析、校验与**同名遮蔽**（先命中者胜），客户端只做展示：
/// 不再自己扫目录、解析 YAML 或复刻优先级。
#[derive(Debug, Clone, Serialize, serde::Deserialize, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct SkillsCatalogParams {
    #[serde(default)]
    pub cwd: Option<String>,
}

/// `skills.catalog` 的单个技能条目。
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct SkillCatalogEntry {
    pub name: String,
    pub description: String,
    /// `project` | `user`
    pub scope: String,
    /// `project` | `user` | `bundled`
    pub origin: String,
    pub path: String,
}

/// 发现期诊断（目录不可读、SKILL.md 非法等）。
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct SkillCatalogDiagnostic {
    /// `warning` | `error`
    pub severity: String,
    pub path: String,
    pub message: String,
}

/// `skills.catalog` 结果。`diagnostics` 为空数组表示全部干净——
/// 客户端据此显示空态，而不是伪造「演示数据」。
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct SkillsCatalogResult {
    pub skills: Vec<SkillCatalogEntry>,
    pub diagnostics: Vec<SkillCatalogDiagnostic>,
}

/// `workspace.get` 定位单条会话；result 为其工作区 cwd（未绑定为空串）。
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct WorkspaceGetParams {
    pub session_id: String,
}

/// `workspace.list` 无参数；params 恒为 `{}`。结果条目为
/// `qaqh_ringing_contract::WorkspaceEntry`（meta + `missing_dir`）。
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct WorkspaceListParams {}

// ── B08:此前 untyped 的 8 个方法的权威 DTO ─────────────────────────────

/// `config.load` 无参数；params 恒为 `{}`。结果为 `qaqh_config_api::ConfigDto`
/// (camelCase wire,与 `config.save` 的读形状同源)。
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct ConfigLoadParams {}

/// `diagnostics.memory.snapshot` 参数:`after_sequence` 之后的相位增量,
/// 缺省全量。结果为 `qaqh_memwatch::MemorySnapshot`。
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct DiagnosticsMemorySnapshotParams {
    #[serde(default)]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub after_sequence: Option<u64>,
}

/// `profile.list` 无参数;结果为 legacy profiles 目录的摘要读模型。
/// 该目录的持久化 schema 是 `qaqh_types::PersistentProfiles`(ProfileConfig),
/// 与新 provider 目录(providers.toml)分属不同文件、不同 revision 体系
/// (契约裁定 D3);default 指针在 config,不在本文件。
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct ProfileListParams {}

/// `profile.list` 结果:名字 → 摘要(非敏感;api_key 只有 set 标记)。
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct ProfileListSnapshot {
    pub profiles: std::collections::BTreeMap<String, qaqh_config_api::ProfileSummaryDto>,
}

/// `session.dashboard` 定位单条会话。
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct SessionDashboardParams {
    pub session_id: String,
}

/// `session.dashboard` 的单条任务卡(来自 todo 状态投影)。
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct SessionDashboardTask {
    pub id: String,
    pub subject: String,
    pub description: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
}

/// `session.dashboard` 结果:任务卡 + 最近编辑文件(去重截断,最近优先)。
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct SessionDashboard {
    pub tasks: Vec<SessionDashboardTask>,
    pub recent_edits: Vec<String>,
}

/// `skills.list_tools` 无参数;结果为在册工具名字符串数组。
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct SkillsListToolsParams {}

/// `plan.read` 定位单条会话;结果为 `PlanReadItem[]`(无计划文件 = 空数组)。
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct PlanReadParams {
    pub session_id: String,
}

/// `plan.read` 的单条条目;`status` 是规范枚举(pending/approved/rejected/
/// question),不再使用旧 "✓/-/?" 字形(契约裁定 D9)。
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct PlanReadItem {
    pub id: String,
    pub title: String,
    pub status: PlanItemStatusView,
    pub comment: String,
    pub actions: Vec<String>,
}

/// plan 条目状态的公开词表。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
#[serde(rename_all = "snake_case")]
pub enum PlanItemStatusView {
    Pending,
    Approved,
    Rejected,
    Question,
}

/// `todo.list` 参数:`session_id` + 与 todo 工具同词表的可选 status 过滤。
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct TodoListParams {
    pub session_id: String,
    #[serde(default)]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub status: Option<String>,
}

pub use qaqh_ringing_contract::todo::{TodoCountsSnapshot, TodoItemSnapshot, TodoListSnapshot};

/// `todo.status` 参数。
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct TodoStatusParams {
    pub session_id: String,
}

/// `todo.status` 结果;会话无 todo 文件时整个 result 为 `null`。
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct TodoStatusSnapshot {
    pub mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_title: Option<String>,
    pub pending: usize,
    pub in_progress: usize,
    pub completed: usize,
    pub cancelled: usize,
    pub total: usize,
    pub items: Vec<TodoItemSnapshot>,
}

/// A typed descriptor links the method key, the exact wire DTOs and the
/// authorization metadata. Only methods present here are yet exposed as typed
/// ServiceMap calls; the remaining legacy RPCs stay explicitly untyped/pending.
#[derive(Debug, Clone, Copy)]
pub struct TypedMethodInfo {
    pub method: &'static str,
    pub params_type: &'static str,
    pub result_type: &'static str,
    pub info: MethodInfo,
    pub validate_params: fn(&Value) -> Result<(), String>,
    pub validate_result: fn(&Value) -> Result<(), String>,
}

/// `TypedMethodInfo` 需要的两种校验函数指针，都只是对泛型 [`validate_params`] /
/// [`validate_result`] 的单态化。此前这里是 31 对手写转发函数（每加一个方法要
/// 抄两行、漏抄就编译失败），改为宏一次性生成：类型清单本身就是文档。
macro_rules! validators {
    (
        params: $($pname:ident => $pty:ty),* $(,)? ;
        result: $($rname:ident => $rty:ty),* $(,)?
    ) => {
        $(
            fn $pname(value: &Value) -> Result<(), String> {
                validate_params::<$pty>(value)
            }
        )*
        $(
            fn $rname(value: &Value) -> Result<(), String> {
                validate_result::<$rty>(value)
            }
        )*
    };
}

validators! {
    params:
        validate_daemon_version_params => DaemonVersionParams,
        validate_fs_list_params => FsListParams,
        validate_fs_read_params => FsReadParams,
        validate_session_list_params => SessionListParams,
        validate_session_meta_params => SessionMetaParams,
        validate_session_activity_params => SessionActivityParams,
        validate_session_get_activity_params => SessionGetActivityParams,
        validate_exec_session_list_params => ExecSessionListParams,
        validate_session_turn_params => SessionTurnParams,
        validate_skills_catalog_params => SkillsCatalogParams,
        validate_workspace_get_params => WorkspaceGetParams,
        validate_workspace_list_params => WorkspaceListParams,
        validate_config_load_params => ConfigLoadParams,
        validate_diagnostics_memory_snapshot_params => DiagnosticsMemorySnapshotParams,
        validate_profile_list_params => ProfileListParams,
        validate_session_dashboard_params => SessionDashboardParams,
        validate_skills_list_tools_params => SkillsListToolsParams,
        validate_todo_list_params => TodoListParams,
        validate_todo_status_params => TodoStatusParams,
        validate_plan_read_params => PlanReadParams,
        ;
    result:
        validate_string_result => String,
        validate_fs_list_result => Vec<FsEntry>,
        validate_fs_read_result => FsReadResult,
        validate_session_list_result => Vec<qaqh_types::SessionListEntry>,
        validate_session_meta_result => Option<qaqh_types::SessionListEntry>,
        validate_session_activity_result => Vec<qaqh_ringing_contract::SessionActivity>,
        validate_session_get_activity_result => Vec<SessionToolActivity>,
        validate_exec_session_list_result => ExecSessionListResult,
        validate_session_turn_result => Option<qaqh_ringing_contract::SessionTurnSnapshot>,
        validate_skills_catalog_result => SkillsCatalogResult,
        validate_workspace_list_result => Vec<qaqh_ringing_contract::WorkspaceEntry>,
        validate_config_dto_result => qaqh_config_api::ConfigDto,
        validate_memory_snapshot_result => qaqh_memwatch::MemorySnapshot,
        validate_profile_document_result => ProfileListSnapshot,
        validate_session_dashboard_result => SessionDashboard,
        validate_string_list_result => Vec<String>,
        validate_todo_list_result => TodoListSnapshot,
        validate_todo_status_result => Option<TodoStatusSnapshot>,
        validate_plan_read_result => Vec<PlanReadItem>,
}

pub const TYPED_METHODS: &[TypedMethodInfo] = &[
    TypedMethodInfo {
        method: "daemon.version",
        params_type: "DaemonVersionParams",
        result_type: "string",
        info: MethodInfo {
            kind: MethodKind::Read,
            requires_session: false,
            admin_only: false,
        },
        validate_params: validate_daemon_version_params,
        validate_result: validate_string_result,
    },
    TypedMethodInfo {
        method: "skills.catalog",
        params_type: "SkillsCatalogParams",
        result_type: "SkillsCatalogResult",
        info: MethodInfo {
            kind: MethodKind::Read,
            requires_session: false,
            admin_only: false,
        },
        validate_params: validate_skills_catalog_params,
        validate_result: validate_skills_catalog_result,
    },
    TypedMethodInfo {
        method: "fs.list",
        params_type: "FsListParams",
        result_type: "FsEntry[]",
        info: MethodInfo {
            kind: MethodKind::Read,
            requires_session: false,
            admin_only: false,
        },
        validate_params: validate_fs_list_params,
        validate_result: validate_fs_list_result,
    },
    TypedMethodInfo {
        method: "fs.read",
        params_type: "FsReadParams",
        result_type: "FsReadResult",
        info: MethodInfo {
            kind: MethodKind::Read,
            requires_session: false,
            admin_only: false,
        },
        validate_params: validate_fs_read_params,
        validate_result: validate_fs_read_result,
    },
    TypedMethodInfo {
        method: "session.list",
        params_type: "SessionListParams",
        // 条目即前端契约 G2 的 SessionListEntry（SessionMeta flatten + 运行期字段）。
        result_type: "SessionListEntry[]",
        info: MethodInfo {
            kind: MethodKind::Read,
            requires_session: false,
            admin_only: false,
        },
        validate_params: validate_session_list_params,
        validate_result: validate_session_list_result,
    },
    TypedMethodInfo {
        method: "session.meta",
        params_type: "SessionMetaParams",
        // meta 缺失时整个 result 是 null（不是缺键）——`?` 后缀由 exporter
        // 映射为 `SessionListEntry | null`。
        result_type: "SessionListEntry?",
        info: MethodInfo {
            kind: MethodKind::Read,
            requires_session: true,
            admin_only: false,
        },
        validate_params: validate_session_meta_params,
        validate_result: validate_session_meta_result,
    },
    TypedMethodInfo {
        method: "session.activity",
        params_type: "SessionActivityParams",
        // 条目为 qaqh_ringing_contract::SessionActivity：与 daemon /activity 观测端点
        // 同一个序列化载体，无 per-method 镜像。
        result_type: "SessionActivity[]",
        info: MethodInfo {
            kind: MethodKind::Read,
            requires_session: false,
            admin_only: false,
        },
        validate_params: validate_session_activity_params,
        validate_result: validate_session_activity_result,
    },
    TypedMethodInfo {
        method: "session.get_activity",
        params_type: "SessionGetActivityParams",
        result_type: "SessionToolActivity[]",
        info: MethodInfo {
            kind: MethodKind::Read,
            requires_session: true,
            admin_only: false,
        },
        validate_params: validate_session_get_activity_params,
        validate_result: validate_session_get_activity_result,
    },
    TypedMethodInfo {
        method: "workspace.get",
        params_type: "WorkspaceGetParams",
        // 未绑定工作区的会话返回空串（不是 null）。
        result_type: "string",
        info: MethodInfo {
            kind: MethodKind::Read,
            requires_session: true,
            admin_only: false,
        },
        validate_params: validate_workspace_get_params,
        validate_result: validate_string_result,
    },
    TypedMethodInfo {
        method: "workspace.list",
        params_type: "WorkspaceListParams",
        // 条目来自 SQLite workspace 行与实际目录状态（meta + missing_dir）。
        result_type: "WorkspaceEntry[]",
        info: MethodInfo {
            kind: MethodKind::Read,
            requires_session: false,
            admin_only: false,
        },
        validate_params: validate_workspace_list_params,
        validate_result: validate_workspace_list_result,
    },
    TypedMethodInfo {
        method: "config.load",
        params_type: "ConfigLoadParams",
        // ConfigDto(camelCase)与 config.save 的读形状同源。
        result_type: "ConfigDto",
        info: MethodInfo {
            kind: MethodKind::Read,
            requires_session: false,
            admin_only: false,
        },
        validate_params: validate_config_load_params,
        validate_result: validate_config_dto_result,
    },
    TypedMethodInfo {
        method: "profile.list",
        params_type: "ProfileListParams",
        // legacy profiles 目录摘要(裁定 D3):持久化 schema 是
        // PersistentProfiles,与新 provider 目录分立。
        result_type: "ProfileListSnapshot",
        info: MethodInfo {
            kind: MethodKind::Read,
            requires_session: false,
            admin_only: false,
        },
        validate_params: validate_profile_list_params,
        validate_result: validate_profile_document_result,
    },
    TypedMethodInfo {
        method: "session.dashboard",
        params_type: "SessionDashboardParams",
        result_type: "SessionDashboard",
        info: MethodInfo {
            kind: MethodKind::Read,
            requires_session: true,
            admin_only: false,
        },
        validate_params: validate_session_dashboard_params,
        validate_result: validate_session_dashboard_result,
    },
    TypedMethodInfo {
        method: "exec_session.list",
        params_type: "ExecSessionListParams",
        result_type: "ExecSessionListResult",
        info: MethodInfo {
            kind: MethodKind::Read,
            requires_session: true,
            admin_only: false,
        },
        validate_params: validate_exec_session_list_params,
        validate_result: validate_exec_session_list_result,
    },
    TypedMethodInfo {
        method: "session.turn",
        params_type: "SessionTurnParams",
        result_type: "SessionTurnSnapshot",
        info: MethodInfo {
            kind: MethodKind::Read,
            requires_session: true,
            admin_only: false,
        },
        validate_params: validate_session_turn_params,
        validate_result: validate_session_turn_result,
    },
    TypedMethodInfo {
        method: "skills.list_tools",
        params_type: "SkillsListToolsParams",
        result_type: "string[]",
        info: MethodInfo {
            kind: MethodKind::Read,
            requires_session: false,
            admin_only: false,
        },
        validate_params: validate_skills_list_tools_params,
        validate_result: validate_string_list_result,
    },
    TypedMethodInfo {
        method: "todo.list",
        params_type: "TodoListParams",
        result_type: "TodoListSnapshot",
        info: MethodInfo {
            kind: MethodKind::Read,
            requires_session: true,
            admin_only: false,
        },
        validate_params: validate_todo_list_params,
        validate_result: validate_todo_list_result,
    },
    TypedMethodInfo {
        method: "todo.status",
        params_type: "TodoStatusParams",
        // 会话无 todo 文件时整个 result 为 null(不是缺键)。
        result_type: "TodoStatusSnapshot?",
        info: MethodInfo {
            kind: MethodKind::Read,
            requires_session: true,
            admin_only: false,
        },
        validate_params: validate_todo_status_params,
        validate_result: validate_todo_status_result,
    },
    TypedMethodInfo {
        method: "plan.read",
        params_type: "PlanReadParams",
        result_type: "PlanReadItem[]",
        info: MethodInfo {
            kind: MethodKind::Read,
            requires_session: true,
            admin_only: false,
        },
        validate_params: validate_plan_read_params,
        validate_result: validate_plan_read_result,
    },
    TypedMethodInfo {
        method: "diagnostics.memory.snapshot",
        params_type: "DiagnosticsMemorySnapshotParams",
        result_type: "MemorySnapshot",
        info: MethodInfo {
            kind: MethodKind::Read,
            requires_session: false,
            admin_only: true,
        },
        validate_params: validate_diagnostics_memory_snapshot_params,
        validate_result: validate_memory_snapshot_result,
    },
];

fn validate_params<T: DeserializeOwned>(params: &Value) -> Result<(), String> {
    serde_json::from_value::<T>(params.clone())
        .map(|_| ())
        .map_err(|error| format!("invalid typed service params: {error}"))
}

fn validate_result<T: DeserializeOwned>(result: &Value) -> Result<(), String> {
    serde_json::from_value::<T>(result.clone())
        .map(|_| ())
        .map_err(|error| format!("typed service result does not match descriptor: {error}"))
}

/// Apply the DTO boundary for methods declared in [`TYPED_METHODS`].
/// Unknown methods return `Ok(None)` for the legacy dispatch path.
pub fn validate_typed_params(method: &str, params: &Value) -> Result<bool, String> {
    let Some(descriptor) = TYPED_METHODS
        .iter()
        .find(|descriptor| descriptor.method == method)
    else {
        return Ok(false);
    };
    (descriptor.validate_params)(params)?;
    Ok(true)
}

pub fn validate_typed_result(method: &str, result: &Value) -> Result<bool, String> {
    let Some(descriptor) = TYPED_METHODS
        .iter()
        .find(|descriptor| descriptor.method == method)
    else {
        return Ok(false);
    };
    (descriptor.validate_result)(result)?;
    Ok(true)
}

/// 服务面会话参数键。
pub const SESSION_PARAM: &str = "session_id";

/// 从服务面 params 取会话键。
pub fn session_param_value(params: &Value) -> Option<&str> {
    params.get(SESSION_PARAM).and_then(Value::as_str)
}

/// 取会话键并要求存在，缺失时报 `missing string parameter: session_id`。
pub fn session_param(params: &Value) -> Result<String, String> {
    session_param_value(params)
        .map(str::to_string)
        .ok_or_else(|| format!("missing string parameter: {SESSION_PARAM}"))
}

/// fs 远端作用域键。
pub fn scope_session_param_value(params: &Value) -> Option<&str> {
    params.get("scope_session_id").and_then(Value::as_str)
}

const READ: MethodInfo = MethodInfo {
    kind: MethodKind::Read,
    requires_session: false,
    admin_only: false,
};
const READ_ADMIN: MethodInfo = MethodInfo {
    kind: MethodKind::Read,
    requires_session: false,
    admin_only: true,
};
const READ_SEEDED: MethodInfo = MethodInfo {
    kind: MethodKind::Read,
    requires_session: true,
    admin_only: false,
};
const WRITE_ADMIN: MethodInfo = MethodInfo {
    kind: MethodKind::Write,
    requires_session: false,
    admin_only: true,
};

/// 方法表**本体**（单一真源）。
///
/// 之所以是「一个可遍历的数组」而不是 `match`：穷举闸必须能**从表反查清单**
/// （「表里留了一条没人实现的死条目 / 清单里漏了一条」两种漂移才都能被机械发现）。
/// 2026-10 之前这里是一个 `match`，两段所谓「反向」测试遍历的都是各 crate 的
/// `methods::ALL`——方向相同，等于空转；`dsaccount.verify` 就是那样漏掉的。
///
/// 顺序无语义（[`lookup`] 线性扫描，条目数几十，一次 RPC 一次查找）。
pub const METHODS: &[(&str, MethodInfo)] = &[
    // daemon / 会话只读
    ("daemon.version", READ),
    ("session.list", READ),
    ("session.meta", READ_SEEDED),
    ("session.activity", READ),
    ("session.dashboard", READ_SEEDED),
    ("session.get_activity", READ_SEEDED),
    ("session.turn", READ_SEEDED),
    ("exec_session.list", READ_SEEDED),
    // workspace
    ("workspace.get", READ_SEEDED),
    ("workspace.list", READ),
    // fs
    ("fs.list", READ),
    ("fs.read", READ),
    // config / profile
    ("config.load", READ),
    ("profile.list", READ),
    // Sign in with ChatGPT（套餐用量）。**全部 admin_only**：配对出来的设备
    // （view/interact）绝不得触发登录 / 切账户 / 登出——那等于让远端设备把
    // 新账户接进用户机器并改写本机凭据。方法名的单一真源在 qaqh_siwc::methods。
    ("siwc.status", READ_ADMIN),
    ("siwc.profiles", READ_ADMIN),
    ("siwc.models", READ_ADMIN),
    ("siwc.sign_in.start", WRITE_ADMIN),
    ("siwc.sign_in.status", READ_ADMIN),
    ("siwc.sign_in.cancel", WRITE_ADMIN),
    ("siwc.profile.select", WRITE_ADMIN),
    ("siwc.sign_out", WRITE_ADMIN),
    // DeepSeek 账号（账号余额）。与 siwc 同一档待遇：**全部 admin_only**——
    // 配对设备不得触发登录 / 登出 / 读余额。方法名真源在 qaqh_dsaccount::methods。
    ("dsaccount.status", READ_ADMIN),
    ("dsaccount.balance", READ_ADMIN),
    ("dsaccount.bonuses", READ_ADMIN),
    ("dsaccount.sign_in.start", WRITE_ADMIN),
    ("dsaccount.sign_in.status", READ_ADMIN),
    ("dsaccount.sign_in.cancel", WRITE_ADMIN),
    ("dsaccount.sign_out", WRITE_ADMIN),
    ("dsaccount.bonus.ack", WRITE_ADMIN),
    ("dsaccount.models", READ_ADMIN),
    // 可能顺手清凭据，所以按写类报错（`action_failed`）而不是只读。
    ("dsaccount.verify", WRITE_ADMIN),
    // skills
    ("skills.catalog", READ),
    ("skills.list_tools", READ),
    // todo / plan / stats
    ("todo.status", READ_SEEDED),
    // todo CLI 路线（daemon HTTP 直访；与 LLM 工具分发表共用 exec 核心）
    ("todo.list", READ_SEEDED),
    ("plan.read", READ_SEEDED),
    // Process-wide memory view is intentionally admin-scope only.
    ("diagnostics.memory.snapshot", READ_ADMIN),
    // subagent
];

/// 方法表：未列出的名字返回 `None`（HTTP 404）。
pub fn lookup(method: &str) -> Option<MethodInfo> {
    METHODS
        .iter()
        .find(|(name, _)| *name == method)
        .map(|(_, info)| *info)
}

/// 统一错误响应形状（daemon HTTP 层使用），按方法类别区分 code。
///
/// 拒绝/越权由 [`QaqhError::is_forbidden`] 判定——此前是
/// `message.starts_with("FORBIDDEN")` 的字符串嗅探，语义与文案耦合。
/// 其余错误码保持既有契约（`query_failed` / `action_failed`）。
pub fn error_response(kind: MethodKind, error: &crate::error::QaqhError) -> Value {
    let code = if error.is_forbidden() {
        "forbidden"
    } else {
        match kind {
            MethodKind::Read => "query_failed",
            MethodKind::Write => "action_failed",
        }
    };
    serde_json::json!({ "code": code, "message": error.to_string() })
}

/// 服务分发：方法表校验后的唯一入口。
///
/// 账号面（siwc/dsaccount）先走 [`accounts::dispatch`] 的逐方法 DTO 收口
/// （params 解码 → 调用管理器 → DTO 投影），其余方法经 `service.handle`
/// 执行并按 [`TYPED_METHODS`] 做边界校验。
pub fn dispatch(
    service: &QaqhService,
    method: &str,
    params: &Value,
) -> crate::error::QaqhResult<Value> {
    if let Some(result) = accounts::dispatch(service, method, params) {
        return result.map_err(crate::error::QaqhError::message);
    }
    // params 只解码一次：`QaqhService::handle` → `Command::decode` 用的就是**同一批
    // DTO**（`command.rs` 的 `*Args` 全是本文件 `*Params` 的别名），严格反序列化
    // 等价于 `validate_typed_params`。此前这里先校验一遍再让 handle 解一遍，同一份
    // JSON 被解两次、方法表被查两次。`validate_typed_params` 仍保留：契约导出与单测
    // 直接用它断言 DTO 边界。
    let result = service.handle(method, params)?;
    // 结果侧不能省：handler 产出的是 `Value`，没有 DTO 边界兜底（非 typed 方法
    // 返回 `Ok(false)`，等价于旧行为的 `typed == false` 分支）。
    validate_typed_result(method, &result).map_err(crate::error::QaqhError::message)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 「登录/凭据」类方法一律 admin-only，且方法表与 qaqh-siwc 的清单**逐一对齐**。
    ///
    /// 这条测试的意义不在覆盖率，而在**越权面**：任何新增的 `siwc.*` 方法
    /// 只要漏了 admin 档，就会被这里挡住（`ALL` 是那份清单的真源）。
    ///
    /// **两向都遍历**：`ALL → 表`（漏项）与 `表 → ALL`（死条目）。第二向必须从
    /// [`METHODS`] 出发——遍历 `ALL` 再查表与正向是同一个方向，等于空转。
    #[test]
    fn siwc_methods_are_admin_only_and_fully_enumerated() {
        for method in qaqh_siwc::methods::ALL {
            let info = lookup(method).unwrap_or_else(|| panic!("方法表缺少 {method}"));
            assert!(
                info.admin_only,
                "{method} 必须 admin_only：配对设备不得改写本机凭据"
            );
        }
        // 反向（真反向）：表里的 siwc.* 一个都不能多。
        for (method, info) in METHODS {
            if method.starts_with("siwc.") {
                assert!(
                    qaqh_siwc::methods::ALL.contains(method),
                    "{method} 在方法表里，却不在 qaqh_siwc::methods::ALL"
                );
                assert!(info.admin_only, "{method} 必须 admin_only");
            }
        }
    }

    /// DeepSeek 账号：与 siwc 同一档（全部 admin_only + 清单全覆盖）。
    ///
    /// 越权面的意义与上面那条相同：账号凭据同样是「本机身份」，配对设备
    /// 不得碰它——尤其 `sign_in.start`（会打开浏览器并把新账号接进本机）。
    ///
    /// 这条的**反向**臂是 R5 的验收项：当初 `dsaccount.verify` 在表里、
    /// 不在 `ALL` 里，而旧的「反向」遍历的仍是 `ALL`，所以从来没红过。
    #[test]
    fn dsaccount_methods_are_admin_only_and_fully_enumerated() {
        assert!(!qaqh_dsaccount::methods::ALL.is_empty());
        for method in qaqh_dsaccount::methods::ALL {
            let info = lookup(method).unwrap_or_else(|| panic!("方法表缺少 {method}"));
            assert!(
                info.admin_only,
                "{method} 必须 admin_only：配对设备不得触碰本机账号凭据"
            );
        }
        // 反向（真反向）：表里的 dsaccount.* 一个都不能多。
        for (method, info) in METHODS {
            if method.starts_with("dsaccount.") {
                assert!(
                    qaqh_dsaccount::methods::ALL.contains(method),
                    "{method} 在方法表里，却不在 qaqh_dsaccount::methods::ALL"
                );
                assert!(info.admin_only, "{method} 必须 admin_only");
            }
        }
    }

    /// 方法表可遍历之后，重名条目也该被挡住：`lookup` 只命中第一条，
    /// 多出来的那条等于「写了但永远不生效」——旧 `match` 形态会直接编译失败，
    /// 换成数组之后必须由测试补上这条闸。
    #[test]
    fn method_table_has_no_duplicate_names() {
        let mut names: Vec<&str> = METHODS.iter().map(|(name, _)| *name).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "方法表里有重名条目");
    }

    #[test]
    fn typed_descriptors_match_runtime_method_metadata() {
        // 两份 descriptor 清单（本文件与账号面）合起来看：逐条对上方法表，
        // 且跨清单不得重复声明同一方法。
        let combined: Vec<&TypedMethodInfo> = TYPED_METHODS
            .iter()
            .chain(accounts::TYPED_METHODS.iter())
            .collect();
        for descriptor in &combined {
            let runtime = lookup(descriptor.method).unwrap_or_else(|| {
                panic!(
                    "typed descriptor has no runtime method: {}",
                    descriptor.method
                )
            });
            assert_eq!(
                runtime, descriptor.info,
                "metadata mismatch for {}",
                descriptor.method
            );
        }
        let mut methods: Vec<_> = combined
            .iter()
            .map(|descriptor| descriptor.method)
            .collect();
        let count = methods.len();
        methods.sort_unstable();
        methods.dedup();
        assert_eq!(methods.len(), count, "typed descriptor method duplicated");
    }

    #[test]
    fn typed_method_params_fail_before_runtime_dispatch() {
        assert_eq!(
            validate_typed_params("session.list", &serde_json::json!({})).expect("typed"),
            true
        );
        assert!(
            validate_typed_params("session.meta", &serde_json::json!({"session_id": "s1"}))
                .expect("typed")
        );
        assert!(
            validate_typed_params("session.meta", &serde_json::json!({})).is_err(),
            "session.meta 必须带 session_id"
        );
        assert!(
            validate_typed_params("session.activity", &serde_json::json!({}))
                .expect("activity takes no params")
        );
        assert!(
            validate_typed_params("session.get_activity", &serde_json::json!({})).is_err(),
            "session.get_activity 必须带 session_id"
        );
        assert!(
            validate_typed_params("workspace.list", &serde_json::json!({}))
                .expect("list takes no params")
        );
        assert!(
            validate_typed_params("workspace.get", &serde_json::json!({})).is_err(),
            "workspace.get 必须带 session_id"
        );
        assert!(
            !validate_typed_params("workspace.create", &serde_json::json!({"path": "/tmp"}))
                .expect("not a service query")
        );
        assert!(
            validate_typed_params(
                "fs.list",
                &serde_json::json!({ "path": 3, "scope_session_id": "s1" })
            )
            .is_err()
        );
        assert!(
            validate_typed_params(
                "fs.read",
                &serde_json::json!({ "path": "/tmp/a", "max_bytes": "large" })
            )
            .is_err()
        );
        assert!(
            validate_typed_params(
                "fs.read",
                &serde_json::json!({ "path": "/tmp/a", "scope_session_id": "s1" })
            )
            .expect("optional max_bytes and scope are accepted")
        );
    }

    #[test]
    fn typed_method_results_match_descriptors() {
        assert!(
            validate_typed_result("daemon.version", &serde_json::json!("2.0.0"))
                .expect("valid result")
        );
        assert!(validate_typed_result("daemon.version", &serde_json::json!({})).is_err());
        assert!(
            validate_typed_result("session.list", &serde_json::json!([])).expect("typed result")
        );
        assert!(
            validate_typed_result("session.list", &serde_json::json!([{}])).is_err(),
            "空对象不是 SessionListEntry（G2 形状必须响亮失败）"
        );
        assert!(
            validate_typed_result("session.meta", &Value::Null)
                .expect("meta 缺失 → null 是合法结果")
        );
        assert!(
            validate_typed_result("session.meta", &serde_json::json!([])).is_err(),
            "数组不是 session.meta 的结果形状"
        );
        assert!(
            validate_typed_result(
                "session.activity",
                &serde_json::json!([{
                    "session_id": "s1",
                    "state": "running",
                    "turn_id": "t1",
                    "seq": 3,
                    "updated_at": 1759000000000u64
                }])
            )
            .expect("valid activity entry")
        );
        assert!(
            validate_typed_result(
                "session.activity",
                &serde_json::json!([{
                    "session_id": "s1",
                    "state": "hibernating",
                    "seq": 1,
                    "updated_at": 1u64
                }])
            )
            .is_err(),
            "state 是 Ringing ActivityState 闭集枚举"
        );
        assert!(
            validate_typed_result(
                "session.get_activity",
                &serde_json::json!([{
                    "tool_name": "fs_read",
                    "summary": "read file",
                    "status": "ok",
                    "finished_at_ms": 1_759_000_042_000i64,
                    "args": "{\"path\":\"/a\"}"
                }])
            )
            .expect("valid tool activity")
        );
        assert!(
            validate_typed_result(
                "session.get_activity",
                &serde_json::json!([{"tool_name": "fs_read", "status": "done"}])
            )
            .is_err(),
            "summary/finished_at_ms/args 必填，status 是闭集枚举"
        );
        assert!(
            validate_typed_result(
                "workspace.list",
                &serde_json::json!([{
                    "id": "ws-1",
                    "path": "/synthetic/workspace",
                    "title": "workspace",
                    "order": 0,
                    "session_ids": ["0199a0f0-0000-7000-8000-000000000001"],
                    "missing_dir": false
                }])
            )
            .expect("valid workspace entry")
        );
        assert!(
            validate_typed_result(
                "workspace.list",
                &serde_json::json!([{
                    "id": "ws-1",
                    "path": "/synthetic/workspace",
                    "title": "workspace",
                    "order": 0,
                    "session_ids": []
                }])
            )
            .is_err(),
            "missing_dir 是 list 条目的必备键（与 create/rename 形状区分）"
        );
        assert!(
            validate_typed_result("workspace.get", &serde_json::json!(""))
                .expect("unbound workspace is an empty string, not null")
        );
        assert!(validate_typed_result(
            "fs.list",
            &serde_json::json!([{ "name": "a", "path": "/a", "is_dir": false, "is_file": true, "size": 1, "modified_ms": null }])
        )
        .expect("valid listing"));
        assert!(validate_typed_result("fs.list", &serde_json::json!([{ "name": "a" }])).is_err());
        assert!(
            validate_typed_result(
                "fs.read",
                &serde_json::json!({ "path": "/a", "size": 1, "truncated": false, "content": "a" })
            )
            .expect("valid read")
        );
        assert!(validate_typed_result(
            "fs.read",
            &serde_json::json!({ "path": "/a", "size": "1", "truncated": false, "content": "a" })
        )
        .is_err());
    }

    /// 客户端服务面白名单（R1）必须与方法表**两向**对齐：
    /// - 两份 `methods::ALL` ⊆ `qaqh_client::SERVICE_METHODS`（不漏）；
    /// - 白名单里没有表外的死条目（不虚）。
    ///
    /// 这条闸放在 runtime 而不是 client：只有这里同时看得见方法表、
    /// 两家 `methods` 与客户端白名单。
    #[test]
    fn client_service_whitelist_mirrors_account_methods() {
        let declared: Vec<&str> = qaqh_siwc::methods::ALL
            .iter()
            .chain(qaqh_dsaccount::methods::ALL.iter())
            .copied()
            .collect();
        for method in &declared {
            assert!(
                qaqh_client::service::is_allowed(method),
                "{method} 在 methods::ALL 里，但客户端白名单没有放行"
            );
            assert!(
                lookup(method).is_some(),
                "{method} 在 methods::ALL 里，但 daemon 方法表没有它"
            );
        }
        // 反向：白名单里的每条都得是表里真实存在的 admin 方法。
        for method in qaqh_client::service::SERVICE_METHODS {
            let info = lookup(method)
                .unwrap_or_else(|| panic!("客户端白名单里的 {method} 不在 daemon 方法表"));
            assert!(info.admin_only, "{method} 应是 admin_only（账号凭据面）");
            assert!(
                declared.contains(method),
                "客户端白名单里的 {method} 不在任何 methods::ALL 里（死条目）"
            );
        }
    }

    /// 服务面会话键只认 `session_id`：legacy `seed` 已随 Phase E 退场。
    #[test]
    fn session_param_only_accepts_session_id() {
        let modern = serde_json::json!({ "session_id": "new" });
        assert_eq!(session_param_value(&modern), Some("new"));
        assert_eq!(session_param(&modern).unwrap(), "new");

        // 旧键不再被接受。
        let legacy = serde_json::json!({ "seed": "old" });
        assert_eq!(session_param_value(&legacy), None);
        assert!(session_param(&legacy).is_err());

        // 两个都在时也只取 `session_id`。
        let both = serde_json::json!({ "session_id": "new", "seed": "old" });
        assert_eq!(session_param_value(&both), Some("new"));

        assert_eq!(session_param_value(&serde_json::json!({})), None);
        assert!(session_param(&serde_json::json!({})).is_err());
    }

    /// fs 作用域键同款：只认 `scope_session_id`。
    #[test]
    fn scope_session_param_only_accepts_scope_session_id() {
        assert_eq!(
            scope_session_param_value(&serde_json::json!({ "scope_session_id": "new" })),
            Some("new")
        );
        assert_eq!(
            scope_session_param_value(&serde_json::json!({ "scope_seed": "old" })),
            None
        );
        assert_eq!(
            scope_session_param_value(&serde_json::json!({
                "scope_session_id": "new",
                "scope_seed": "old"
            })),
            Some("new")
        );
        assert_eq!(scope_session_param_value(&serde_json::json!({})), None);
    }

    // SessionManager 是全局单例，同一测试进程只能 init 一次；
    // 用 OnceLock 共享一个 service 实例（并行测试也不会重复初始化）。
    static SERVICE: std::sync::OnceLock<QaqhService> = std::sync::OnceLock::new();

    fn service() -> &'static QaqhService {
        SERVICE.get_or_init(|| {
            // 隔离 data root：lib 单测不读/不写真实用户根（sessions/、
            // workspace SQLite rows），与其他测试二进制同一纪律。
            static ISOLATED_DATA_ROOT: std::sync::OnceLock<tempfile::TempDir> =
                std::sync::OnceLock::new();
            let tmp = ISOLATED_DATA_ROOT.get_or_init(|| {
                let tmp = tempfile::tempdir().expect("isolated data root tempdir");
                // SAFETY: OnceLock 初始化窗口内的单次进程级设置，此后只读。
                unsafe {
                    std::env::set_var("QAQH_DATA_DIR", tmp.path().join(".qaqh"));
                    std::env::set_var("HOME", tmp.path());
                    std::env::set_var("USERPROFILE", tmp.path());
                }
                tmp
            });
            let _ = tmp;
            QaqhService::init(std::sync::Arc::new(qaqh_session::SessionManager::new(
                qaqh_types::platform::data_dir(),
            )))
        })
    }

    #[test]
    fn todo_mutations_are_commands_and_queries_stay_read_only() {
        let list = lookup("todo.list").expect("todo.list registered");
        assert_eq!(list.kind, MethodKind::Read);
        assert!(list.requires_session);
        for method in ["todo.set", "todo.cancel", "workspace.set", "subagent.spawn"] {
            assert!(lookup(method).is_none(), "{method} uses RingingV2 commands");
        }
        for removed in [
            "session.set_profile",
            "session.set_effort",
            "session.set_tool_mode",
        ] {
            assert!(
                lookup(removed).is_none(),
                "{removed} moved to RingingCommand"
            );
        }
    }

    #[test]
    fn session_list_returns_array() {
        let result = dispatch(service(), "session.list", &serde_json::json!({})).expect("list");
        assert!(result.is_array());
    }

    /// **G2 回归闸（产出边界）**：`session.list` 的条目必须能被权威类型吃下，
    /// 且类型化后再序列化与原 wire **逐键相同**（不多键、不少键）。
    ///
    /// 注意覆盖面：本机数据目录为空时这个循环退化为空——条目形状本身由
    /// `qaqh-types::session::tests::session_list_entry_wire_keys_are_locked`
    /// 锁住（那份不依赖任何数据）。这里补的是「产出方真的发的是那个形状」。
    #[test]
    fn session_list_entries_are_typed_at_the_boundary() {
        let wire = dispatch(service(), "session.list", &serde_json::json!({})).expect("list");
        let entries: Vec<qaqh_types::SessionListEntry> =
            serde_json::from_value(wire.clone()).expect("G2：条目必须能被权威类型解析");
        assert_eq!(entries.len(), wire.as_array().unwrap().len());
        for (typed, raw) in entries.iter().zip(wire.as_array().unwrap()) {
            assert_eq!(
                &serde_json::to_value(typed).expect("serialize"),
                raw,
                "类型化往返改了 wire 形状"
            );
        }
    }

    /// 产出边界：`workspace.list` 的条目必须能被 `WorkspaceEntry` 吃下且
    /// 往返逐键相同；目录存活标记真实参与计算（存在 → `missing_dir=false`）。
    /// SQLite remains the authoritative workspace directory.
    #[test]
    fn workspace_list_entries_are_typed_at_the_boundary() {
        let dir = tempfile::tempdir().expect("workspace dir");
        let created = service()
            .execute_ringing_command(
                &qaqh_ringing::CommandTarget::Global,
                &qaqh_ringing::RingingCommand::WorkspaceCreate {
                    path: dir.path().to_string_lossy().into_owned(),
                },
            )
            .expect("create workspace");
        let wire = dispatch(service(), "workspace.list", &serde_json::json!({})).expect("list");
        let entries: Vec<qaqh_ringing_contract::WorkspaceEntry> =
            serde_json::from_value(wire.clone()).expect("条目必须能被权威类型解析");
        assert_eq!(entries.len(), wire.as_array().unwrap().len());
        assert_eq!(
            entries[0].id,
            created["id"].as_str().expect("created id"),
            "新建工作区应按 order 出现在列表首位"
        );
        assert!(
            !entries[0].missing_dir,
            "目录存在时 missing_dir 必须为 false"
        );
        for (typed, raw) in entries.iter().zip(wire.as_array().unwrap()) {
            assert_eq!(
                &serde_json::to_value(typed).expect("serialize"),
                raw,
                "类型化往返改了 wire 形状"
            );
        }
    }

    #[test]
    fn read_methods_carry_read_kind() {
        let info = lookup("session.list").expect("listed");
        assert_eq!(info.kind, MethodKind::Read);
        assert!(!info.requires_session);
        let info = lookup("session.meta").expect("listed");
        assert!(info.requires_session);
    }

    #[test]
    fn lifecycle_methods_are_deliberately_absent() {
        // 会话生命周期只走 command envelope（N5），服务面不收。
        assert!(lookup("session.new").is_none());
        assert!(lookup("session.create").is_none());
        assert!(lookup("session.resume").is_none());
        assert!(lookup("skills.activate").is_none());
        assert!(lookup("todo.cancel").is_none());
        assert!(lookup("plan.action").is_none());
    }

    #[test]
    fn slash_alias_is_no_longer_accepted() {
        // 旧双别名（"session/list"）已拆除：单一规范形态 `module.method`。
        assert!(lookup("session/list").is_none());
    }

    #[test]
    fn error_response_shape_differs_by_kind() {
        assert_eq!(
            error_response(MethodKind::Read, &crate::error::QaqhError::invalid("x"))["code"],
            serde_json::json!("query_failed")
        );
        assert_eq!(
            error_response(MethodKind::Write, &crate::error::QaqhError::invalid("x"))["code"],
            serde_json::json!("action_failed")
        );
    }

    /// T-2-1：白名单拒绝必须成 `forbidden` 码，而不是伪装成 `query_failed`
    /// （与 IO 失败不可区分）。判定依据是错误**变体**，不是文案前缀。
    #[test]
    fn forbidden_error_maps_to_forbidden_code() {
        let forbidden = crate::error::QaqhError::Forbidden {
            message: "fs.read /etc/passwd: path is outside the allowed roots".to_string(),
        };
        for kind in [MethodKind::Read, MethodKind::Write] {
            let value = error_response(kind, &forbidden);
            assert_eq!(value["code"], serde_json::json!("forbidden"));
            assert!(
                value["message"]
                    .as_str()
                    .unwrap_or_default()
                    .starts_with("FORBIDDEN")
            );
        }
    }
}
