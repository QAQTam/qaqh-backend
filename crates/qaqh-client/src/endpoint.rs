//! Typed service RPC requests (`POST /ringing/v1/service/{method}`，唯一服务面)。
//!
//! These enums keep service method names and JSON assembly inside the
//! transport crate. Native shells choose a closed Rust variant; they cannot
//! mistype a method name, send a mutation through the read-only query surface,
//! or invent a second renderer-facing protocol. (`QueryRequest` → Read 类，
//! `ActionRequest` → Write 类；会话生命周期命令不在此面，见开发标准 N5。)

use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryRequest {
    SessionList,
    SessionActivity,
    ConfigLoad,
    WorkspaceList,
    SkillsListTools,
    /// 列出 daemon 侧目录内容（远端文件选择器数据源）。
    FsList {
        path: String,
    },
    /// 读取 daemon 侧文件内容（文本预览，最多 `max_bytes`）。
    FsRead {
        path: String,
        max_bytes: Option<u64>,
    },
    /// 会话仪表盘：任务清单 + 最近改动（daemon `session.dashboard`）。
    /// 与 [`Self::TodoStatus`] 是同一数据源的两个视图。
    SessionDashboard {
        seed: String,
    },
    /// 会话待办状态（daemon `todo.status`）。
    TodoStatus {
        seed: String,
    },
}

impl QueryRequest {
    pub(crate) fn into_parts(self) -> (&'static str, Value) {
        match self {
            Self::SessionList => ("session.list", json!({})),
            Self::SessionActivity => ("session.activity", json!({})),
            Self::ConfigLoad => ("config.load", json!({})),
            Self::WorkspaceList => ("workspace.list", json!({})),
            Self::SkillsListTools => ("skills.list_tools", json!({})),
            Self::FsList { path } => ("fs.list", json!({ "path": path })),
            Self::FsRead { path, max_bytes } => {
                let mut params = json!({ "path": path });
                if let Some(max_bytes) = max_bytes {
                    params["max_bytes"] = json!(max_bytes);
                }
                ("fs.read", params)
            }
            Self::SessionDashboard { seed } => ("session.dashboard", json!({ "seed": seed })),
            Self::TodoStatus { seed } => ("todo.status", json!({ "seed": seed })),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ActionRequest {
    SkillsOperation {
        seed: String,
        operation_id: String,
        action: String,
        name: String,
        expected_revision: u64,
    },
    SkillsReload {
        seed: String,
    },
    ConfigSave {
        fields: Value,
    },
    ConfigSetPermissionLevel {
        level: u64,
    },
    ProfileApply {
        name: String,
    },
    ProfileSaveCurrent {
        name: String,
    },
    ProfileDelete {
        name: String,
    },
    WorkspaceSet {
        seed: String,
        path: String,
    },
    /// 注册一个目录为 UI 工作区（组织语义；daemon `workspace.create`）。
    WorkspaceCreate {
        path: String,
    },
    /// 重命名工作区（daemon `workspace.rename`）。
    WorkspaceRename {
        id: String,
        title: String,
    },
    /// 删除工作区注册（不删会话；daemon `workspace.delete`）。
    WorkspaceDelete {
        id: String,
    },
    /// 把会话移入指定工作区（daemon `workspace.move_session`）。
    WorkspaceMoveSession {
        seed: String,
        workspace_id: String,
    },
    /// 把会话移出工作区 → 未分组（daemon `workspace.detach`）。
    WorkspaceDetach {
        seed: String,
    },
    /// 切换会话工具模式（standard/minimal/custom，PLAN-TOOL-MODES.md）。
    /// daemon 侧先持久化 meta.json（persist_tool_mode）再经 Control 频道
    /// 下发 worker 应用（set_allowed_tools + tool_defs 刷新）。
    SessionSetToolMode {
        seed: String,
        tool_mode: String,
        custom_tools: Vec<String>,
    },
    /// Spawn an isolated subagent worker (daemon `subagent.spawn`). Returns
    /// `{ "seed": "<8-hex>" }`; the caller then attaches the seed and drives
    /// it with ordinary Ringing commands/events.
    SubagentSpawn {
        /// Tool allowlist (empty = all tools available).
        tools: Vec<String>,
        /// Model override; `None` = inherit parent config.
        model: Option<String>,
        /// API base URL override; `None` = inherit parent config.
        base_url: Option<String>,
        /// Max output tokens override.
        max_tokens: Option<u32>,
        /// Workspace the subagent inherits from the parent agent. Persisted to
        /// the subagent's `SessionMeta.cwd` before the worker starts, so the
        /// subagent resolves relative paths and enforces its permission
        /// boundary against the *parent's* workspace.
        workspace: Option<String>,
    },
}

impl ActionRequest {
    pub(crate) fn into_parts(self) -> (&'static str, Value) {
        match self {
            Self::SkillsOperation {
                seed,
                operation_id,
                action,
                name,
                expected_revision,
            } => (
                "skills.operation",
                json!({
                    "seed": seed,
                    "operationId": operation_id,
                    "action": action,
                    "name": name,
                    "expectedRevision": expected_revision,
                }),
            ),
            Self::SkillsReload { seed } => ("skills.reload", json!({ "seed": seed })),
            Self::ConfigSave { fields } => ("config.save", fields),
            Self::ConfigSetPermissionLevel { level } => {
                ("config.set_permission_level", json!({ "level": level }))
            }
            Self::ProfileApply { name } => ("profile.apply", json!({ "name": name })),
            Self::ProfileSaveCurrent { name } => ("profile.save_current", json!({ "name": name })),
            Self::ProfileDelete { name } => ("profile.delete", json!({ "name": name })),
            Self::WorkspaceSet { seed, path } => {
                ("workspace.set", json!({ "seed": seed, "path": path }))
            }
            Self::WorkspaceCreate { path } => ("workspace.create", json!({ "path": path })),
            Self::WorkspaceRename { id, title } => {
                ("workspace.rename", json!({ "id": id, "title": title }))
            }
            Self::WorkspaceDelete { id } => ("workspace.delete", json!({ "id": id })),
            Self::WorkspaceMoveSession { seed, workspace_id } => (
                "workspace.move_session",
                json!({ "seed": seed, "workspace_id": workspace_id }),
            ),
            Self::WorkspaceDetach { seed } => ("workspace.detach", json!({ "seed": seed })),
            Self::SessionSetToolMode {
                seed,
                tool_mode,
                custom_tools,
            } => (
                "session.set_tool_mode",
                json!({
                    "seed": seed,
                    "tool_mode": tool_mode,
                    "custom_tools": custom_tools,
                }),
            ),
            Self::SubagentSpawn {
                tools,
                model,
                base_url,
                max_tokens,
                workspace,
            } => {
                let mut params = json!({ "tools": tools });
                if let Some(model) = model {
                    params["model"] = json!(model);
                }
                if let Some(base_url) = base_url {
                    params["base_url"] = json!(base_url);
                }
                if let Some(max_tokens) = max_tokens {
                    params["max_tokens"] = json!(max_tokens);
                }
                if let Some(workspace) = workspace {
                    params["workspace"] = json!(workspace);
                }
                ("subagent.spawn", params)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_set_tool_mode_uses_action_route() {
        let (name, params) = ActionRequest::SessionSetToolMode {
            seed: "s1".into(),
            tool_mode: "minimal".into(),
            custom_tools: vec!["exec".into(), "edit".into()],
        }
        .into_parts();
        assert_eq!(name, "session.set_tool_mode");
        assert_eq!(params["seed"], "s1");
        assert_eq!(params["tool_mode"], "minimal");
        assert_eq!(params["custom_tools"][0], "exec");
    }

    #[test]
    fn workspace_set_is_an_action_not_a_query() {
        let (name, params) = ActionRequest::WorkspaceSet {
            seed: "s1".into(),
            path: "C:/work".into(),
        }
        .into_parts();
        assert_eq!(name, "workspace.set");
        assert_eq!(params["seed"], "s1");
    }

    #[test]
    fn query_variants_have_no_call_site_method_strings() {
        let (name, params) = QueryRequest::SessionList.into_parts();
        assert_eq!(name, "session.list");
        assert_eq!(params, json!({}));
    }

    /// `session.dashboard` / `todo.status` 是 seed 域只读方法（服务端早已实现，
    /// 此前客户端封闭枚举缺这两个变体 → TUI 只能自建 `service(method, params)`
    /// 泛型逃生口）。
    #[test]
    fn session_scoped_queries_carry_seed() {
        let (name, params) = QueryRequest::SessionDashboard { seed: "s1".into() }.into_parts();
        assert_eq!(name, "session.dashboard");
        assert_eq!(params, json!({ "seed": "s1" }));

        let (name, params) = QueryRequest::TodoStatus { seed: "s2".into() }.into_parts();
        assert_eq!(name, "todo.status");
        assert_eq!(params, json!({ "seed": "s2" }));
    }

    /// 路由不得重复：`into_parts` 的 match 是逐个手写的，复制粘贴极易让两个
    /// 变体落到同一个方法名上（此时其中一个方法永远发不出去，且不报错）。
    #[test]
    fn query_routes_are_pairwise_distinct() {
        let routes: Vec<&'static str> = vec![
            QueryRequest::SessionList.into_parts().0,
            QueryRequest::SessionActivity.into_parts().0,
            QueryRequest::ConfigLoad.into_parts().0,
            QueryRequest::WorkspaceList.into_parts().0,
            QueryRequest::SkillsListTools.into_parts().0,
            QueryRequest::FsList { path: "/".into() }.into_parts().0,
            QueryRequest::FsRead {
                path: "/".into(),
                max_bytes: None,
            }
            .into_parts()
            .0,
            QueryRequest::SessionDashboard { seed: "s".into() }
                .into_parts()
                .0,
            QueryRequest::TodoStatus { seed: "s".into() }.into_parts().0,
        ];
        let mut seen = std::collections::HashSet::new();
        for route in &routes {
            assert!(seen.insert(*route), "重复的 query 路由: {route}");
        }
    }
}
