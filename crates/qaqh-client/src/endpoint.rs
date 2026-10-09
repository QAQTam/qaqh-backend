//! Typed service RPC requests (`POST /ringing/v2/service/{method}`，唯一服务面)。
//!
//! These enums keep service method names and JSON assembly inside the
//! transport crate. Native shells choose a closed Rust variant; they cannot
//! mistype a method name, send a mutation through the read-only query surface,
//! or invent a second renderer-facing protocol. (`QueryRequest` → Read 类，
//! `ActionRequest` → Write 类；会话生命周期命令不在此面，见开发标准 N5。)
//!
//! # 新增一个服务方法（G3 配方，2026-09-15 定案）
//!
//! 前端缺一个方法时**改这里，而不是在壳层里自己拼 HTTP**。理由见契约文档
//! `docs/current/architecture.md`：前后端共进退，
//! 打补丁比留泛型逃生口便宜；逃生口会把「形状对不上」从**编译错误**退回**运行期
//! 错误**，而那正是 2419 行镜像时代的老毛病。
//!
//! 四处改动，约十行：
//!
//! 1. **本文件**：加一个变体（读走 `QueryRequest`，写走 `ActionRequest`——
//!    放进哪个枚举**就是**这条方法的读写契约声明，别放错）。
//! 2. **本文件** `into_parts`：补一条映射。漏了是**编译错误**（match 穷举）。
//! 3. **`qaqh-runtime/src/ringing/service_methods.rs`**：在 `lookup` 表里登记
//!    方法名与 `MethodKind`（`READ`/`READ_SEEDED`/`WRITE`/`WRITE_SEEDED`）。
//!    漏了 → 客户端拿到 **HTTP 404 `unknown_method`**（响亮，不静默）。
//!    `kind` 要与第 1 步选的枚举一致，否则失败时的错误码会撒谎。
//! 4. **`qaqh-runtime/src/service.rs`** 的 `QaqhService::handle`：补上实现分支。
//!    漏了 → `Err("unknown method: …")`（同样响亮）。
//!
//! 第 3、4 步**没有编译期保护**（daemon 侧的表与客户端枚举之间没有共享真相），
//! 但两者都是首次调用即失败、且报文直指方法名——不是静默漂移，故未上机械闸。
//! 本文件的 `all_query_requests` / `all_action_requests` 两条穷举闸保证第 1、2 步
//! 不会漏掉路由检查。

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
        session_id: String,
    },
    /// 会话待办状态（daemon `todo.status`）。
    TodoStatus {
        session_id: String,
    },
    /// 单会话 meta（daemon `session.meta`）。
    SessionMeta {
        session_id: String,
    },
    /// 读取当前会话 plan（daemon `plan.read`）。
    PlanRead {
        session_id: String,
    },
    /// Git working tree 状态（daemon `git.diff`）。
    GitDiff {
        session_id: String,
    },
    /// 当前 Git 分支（daemon `git.branch`）。
    GitBranch {
        session_id: String,
    },
    /// Git 分支列表（daemon `git.branches`）。
    GitBranches {
        session_id: String,
    },
    /// 单文件 Git diff（daemon `git.file_diff`）。
    GitFileDiff {
        session_id: String,
        file_path: String,
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
            // 线上参数键统一 `session_id`（daemon 侧 `session_param_value`
            // 只认 `session_id`，legacy `seed` 已退场）。
            Self::SessionDashboard { session_id } => {
                ("session.dashboard", json!({ "session_id": session_id }))
            }
            Self::TodoStatus { session_id } => ("todo.status", json!({ "session_id": session_id })),
            Self::SessionMeta { session_id } => {
                ("session.meta", json!({ "session_id": session_id }))
            }
            Self::PlanRead { session_id } => ("plan.read", json!({ "session_id": session_id })),
            Self::GitDiff { session_id } => ("git.diff", json!({ "session_id": session_id })),
            Self::GitBranch { session_id } => ("git.branch", json!({ "session_id": session_id })),
            Self::GitBranches { session_id } => {
                ("git.branches", json!({ "session_id": session_id }))
            }
            Self::GitFileDiff {
                session_id,
                file_path,
            } => (
                "git.file_diff",
                json!({ "session_id": session_id, "file_path": file_path }),
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ActionRequest {
    SkillsOperation {
        session_id: String,
        operation_id: String,
        action: String,
        name: String,
        expected_revision: u64,
    },
    SkillsReload {
        session_id: String,
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
    /// 只改该会话的 profile（写入 meta.profile 并定向重载，不动全局
    /// `active_profile`，也不影响其它会话）。
    SessionSetProfile {
        session_id: String,
        name: String,
    },
    WorkspaceSet {
        session_id: String,
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
        session_id: String,
        workspace_id: String,
    },
    /// 把会话移出工作区 → 未分组（daemon `workspace.detach`）。
    WorkspaceDetach {
        session_id: String,
    },
    /// 切换会话工具模式（standard/minimal/custom，PLAN-TOOL-MODES.md）。
    /// daemon 侧先持久化 meta.json（persist_tool_mode）再经 Control 频道
    /// 下发 worker 应用（set_allowed_tools + tool_defs 刷新）。
    SessionSetToolMode {
        session_id: String,
        tool_mode: String,
        custom_tools: Vec<String>,
    },
    /// Spawn an isolated subagent worker (daemon `subagent.spawn`). Returns
    /// `{ "session_id": "<uuid>" }`; the caller then attaches that session and
    /// drives it with ordinary Ringing commands/events.
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
    /// 切换 Git 分支（daemon `git.switch_branch`）。
    GitSwitchBranch {
        session_id: String,
        branch: String,
        stash: bool,
    },
    /// 提交当前 Git working tree（daemon `git.commit`）。
    GitCommit {
        session_id: String,
        message: String,
    },
}

impl ActionRequest {
    pub(crate) fn into_parts(self) -> (&'static str, Value) {
        match self {
            Self::SkillsOperation {
                session_id,
                operation_id,
                action,
                name,
                expected_revision,
            } => (
                "skills.operation",
                json!({
                    "session_id": session_id,
                    "operationId": operation_id,
                    "action": action,
                    "name": name,
                    "expectedRevision": expected_revision,
                }),
            ),
            Self::SkillsReload { session_id } => {
                ("skills.reload", json!({ "session_id": session_id }))
            }
            Self::ConfigSave { fields } => ("config.save", fields),
            Self::ConfigSetPermissionLevel { level } => {
                ("config.set_permission_level", json!({ "level": level }))
            }
            Self::ProfileApply { name } => ("profile.apply", json!({ "name": name })),
            Self::ProfileSaveCurrent { name } => ("profile.save_current", json!({ "name": name })),
            Self::ProfileDelete { name } => ("profile.delete", json!({ "name": name })),
            Self::SessionSetProfile { session_id, name } => (
                "session.set_profile",
                json!({ "session_id": session_id, "name": name }),
            ),
            Self::WorkspaceSet { session_id, path } => (
                "workspace.set",
                json!({ "session_id": session_id, "path": path }),
            ),
            Self::WorkspaceCreate { path } => ("workspace.create", json!({ "path": path })),
            Self::WorkspaceRename { id, title } => {
                ("workspace.rename", json!({ "id": id, "title": title }))
            }
            Self::WorkspaceDelete { id } => ("workspace.delete", json!({ "id": id })),
            Self::WorkspaceMoveSession {
                session_id,
                workspace_id,
            } => (
                "workspace.move_session",
                json!({ "session_id": session_id, "workspace_id": workspace_id }),
            ),
            Self::WorkspaceDetach { session_id } => {
                ("workspace.detach", json!({ "session_id": session_id }))
            }
            Self::SessionSetToolMode {
                session_id,
                tool_mode,
                custom_tools,
            } => (
                "session.set_tool_mode",
                json!({
                    "session_id": session_id,
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
            Self::GitSwitchBranch {
                session_id,
                branch,
                stash,
            } => (
                "git.switch_branch",
                json!({ "session_id": session_id, "branch": branch, "stash": stash }),
            ),
            Self::GitCommit {
                session_id,
                message,
            } => (
                "git.commit",
                json!({ "session_id": session_id, "message": message }),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_set_tool_mode_uses_action_route() {
        let (name, params) = ActionRequest::SessionSetToolMode {
            session_id: "s1".into(),
            tool_mode: "minimal".into(),
            custom_tools: vec!["exec".into(), "edit".into()],
        }
        .into_parts();
        assert_eq!(name, "session.set_tool_mode");
        assert_eq!(params["session_id"], "s1");
        assert_eq!(params["tool_mode"], "minimal");
        assert_eq!(params["custom_tools"][0], "exec");
    }

    #[test]
    fn workspace_set_is_an_action_not_a_query() {
        let (name, params) = ActionRequest::WorkspaceSet {
            session_id: "s1".into(),
            path: "C:/work".into(),
        }
        .into_parts();
        assert_eq!(name, "workspace.set");
        assert_eq!(params["session_id"], "s1");
    }

    #[test]
    fn git_write_variants_are_actions() {
        let (name, params) = ActionRequest::GitSwitchBranch {
            session_id: "s".into(),
            branch: "main".into(),
            stash: true,
        }
        .into_parts();
        assert_eq!(name, "git.switch_branch");
        assert_eq!(
            params,
            json!({ "session_id": "s", "branch": "main", "stash": true })
        );

        let (name, params) = ActionRequest::GitCommit {
            session_id: "s".into(),
            message: "checkpoint".into(),
        }
        .into_parts();
        assert_eq!(name, "git.commit");
        assert_eq!(
            params,
            json!({ "session_id": "s", "message": "checkpoint" })
        );
    }

    #[test]
    fn query_variants_have_no_call_site_method_strings() {
        let (name, params) = QueryRequest::SessionList.into_parts();
        assert_eq!(name, "session.list");
        assert_eq!(params, json!({}));
    }

    /// `session.dashboard` / `todo.status` 是会话域只读方法（服务端早已实现，
    /// 此前客户端封闭枚举缺这两个变体 → TUI 只能自建 `service(method, params)`
    /// 泛型逃生口）。
    #[test]
    fn session_scoped_queries_carry_session_id() {
        let (name, params) = QueryRequest::SessionDashboard {
            session_id: "s1".into(),
        }
        .into_parts();
        assert_eq!(name, "session.dashboard");
        assert_eq!(params, json!({ "session_id": "s1" }));

        let (name, params) = QueryRequest::TodoStatus {
            session_id: "s2".into(),
        }
        .into_parts();
        assert_eq!(name, "todo.status");
        assert_eq!(params, json!({ "session_id": "s2" }));
    }

    /// **Phase D 写端契约**：所有会话域 RPC 只发 `session_id`，不得再发 legacy
    /// `seed`（daemon 侧 `session_param_value` 只认 `session_id`，legacy `seed` 已退场）。
    #[test]
    fn session_scoped_requests_never_emit_legacy_session_key() {
        let requests = [
            QueryRequest::SessionDashboard {
                session_id: "s".into(),
            },
            QueryRequest::TodoStatus {
                session_id: "s".into(),
            },
            QueryRequest::SessionMeta {
                session_id: "s".into(),
            },
            QueryRequest::PlanRead {
                session_id: "s".into(),
            },
            QueryRequest::GitDiff {
                session_id: "s".into(),
            },
            QueryRequest::GitBranch {
                session_id: "s".into(),
            },
            QueryRequest::GitBranches {
                session_id: "s".into(),
            },
            QueryRequest::GitFileDiff {
                session_id: "s".into(),
                file_path: "f".into(),
            },
        ];
        for request in requests {
            let (name, params) = request.into_parts();
            assert_eq!(params["session_id"], "s", "{name} 必须发 session_id");
            assert!(
                params.get("seed").is_none(),
                "{name} 不得再发 legacy seed 键"
            );
        }

        let actions = [
            ActionRequest::SkillsReload {
                session_id: "s".into(),
            },
            ActionRequest::WorkspaceSet {
                session_id: "s".into(),
                path: "/".into(),
            },
            ActionRequest::WorkspaceMoveSession {
                session_id: "s".into(),
                workspace_id: "w".into(),
            },
            ActionRequest::WorkspaceDetach {
                session_id: "s".into(),
            },
            ActionRequest::SessionSetToolMode {
                session_id: "s".into(),
                tool_mode: "minimal".into(),
                custom_tools: vec![],
            },
            ActionRequest::GitSwitchBranch {
                session_id: "s".into(),
                branch: "main".into(),
                stash: false,
            },
            ActionRequest::GitCommit {
                session_id: "s".into(),
                message: "m".into(),
            },
        ];
        for action in actions {
            let (name, params) = action.into_parts();
            assert_eq!(params["session_id"], "s", "{name} 必须发 session_id");
            assert!(
                params.get("seed").is_none(),
                "{name} 不得再发 legacy seed 键"
            );
        }
    }

    #[test]
    fn tui_service_variants_use_existing_routes() {
        for (request, expected_name) in [
            (
                QueryRequest::SessionMeta {
                    session_id: "s".into(),
                },
                "session.meta",
            ),
            (
                QueryRequest::PlanRead {
                    session_id: "s".into(),
                },
                "plan.read",
            ),
            (
                QueryRequest::GitDiff {
                    session_id: "s".into(),
                },
                "git.diff",
            ),
            (
                QueryRequest::GitBranch {
                    session_id: "s".into(),
                },
                "git.branch",
            ),
            (
                QueryRequest::GitBranches {
                    session_id: "s".into(),
                },
                "git.branches",
            ),
            (
                QueryRequest::GitFileDiff {
                    session_id: "s".into(),
                    file_path: "src/lib.rs".into(),
                },
                "git.file_diff",
            ),
        ] {
            assert_eq!(request.into_parts().0, expected_name);
        }

        let (name, params) = QueryRequest::GitFileDiff {
            session_id: "s".into(),
            file_path: "src/lib.rs".into(),
        }
        .into_parts();
        assert_eq!(name, "git.file_diff");
        assert_eq!(
            params,
            json!({ "session_id": "s", "file_path": "src/lib.rs" })
        );
    }

    /// **变体清单 + 穷举闸（G3）**：Rust 的枚举无法被迭代，清单只能手写。
    /// 这里用一条**穷举 `match`** 把手写变成「编译器盯着的手写」：新增变体时
    /// `into_parts` 先报错，改完再来这里，`match` 不穷举会**再报一次错**，
    /// 于是不可能悄悄漏掉一条路由的检查。
    ///
    /// **已知的窄缝**（实测过，不是推测）：新增变体后只在下面 `match` 里补臂、
    /// 忘了 `vec!` 那一行，本闸**不响**——代价是该路由不被检查（不会产生错误
    /// 结果，只是少查一次）。反方向是关着的：**删除**一行会让末尾的条数断言
    /// 直接失败（删一条后总数断言会立刻报警）。要关掉剩下这条缝需要一个 `EnumIter` 派生
    /// （多一个依赖）或把枚举改成宏生成（可读性代价）。按 G3「流程优先、实现
    /// 保持封闭枚举」暂不引入。
    fn all_query_requests() -> Vec<QueryRequest> {
        let all = vec![
            QueryRequest::SessionList,
            QueryRequest::SessionActivity,
            QueryRequest::ConfigLoad,
            QueryRequest::WorkspaceList,
            QueryRequest::SkillsListTools,
            QueryRequest::FsList { path: "/".into() },
            QueryRequest::FsRead {
                path: "/".into(),
                max_bytes: None,
            },
            QueryRequest::SessionDashboard {
                session_id: "s".into(),
            },
            QueryRequest::TodoStatus {
                session_id: "s".into(),
            },
            QueryRequest::SessionMeta {
                session_id: "s".into(),
            },
            QueryRequest::PlanRead {
                session_id: "s".into(),
            },
            QueryRequest::GitDiff {
                session_id: "s".into(),
            },
            QueryRequest::GitBranch {
                session_id: "s".into(),
            },
            QueryRequest::GitBranches {
                session_id: "s".into(),
            },
            QueryRequest::GitFileDiff {
                session_id: "s".into(),
                file_path: "src/lib.rs".into(),
            },
        ];
        // 穷举闸：上面的清单必须覆盖全部变体，否则这里编译失败。
        for q in &all {
            match q {
                QueryRequest::SessionList
                | QueryRequest::SessionActivity
                | QueryRequest::ConfigLoad
                | QueryRequest::WorkspaceList
                | QueryRequest::SkillsListTools
                | QueryRequest::FsList { .. }
                | QueryRequest::FsRead { .. }
                | QueryRequest::SessionDashboard { .. }
                | QueryRequest::TodoStatus { .. }
                | QueryRequest::SessionMeta { .. }
                | QueryRequest::PlanRead { .. }
                | QueryRequest::GitDiff { .. }
                | QueryRequest::GitBranch { .. }
                | QueryRequest::GitBranches { .. }
                | QueryRequest::GitFileDiff { .. } => {}
            }
        }
        all
    }

    /// [`all_query_requests`] 的对偶。见该函数注释。
    fn all_action_requests() -> Vec<ActionRequest> {
        let all = vec![
            ActionRequest::SkillsOperation {
                session_id: "s".into(),
                operation_id: "op".into(),
                action: "activate".into(),
                name: "n".into(),
                expected_revision: 0,
            },
            ActionRequest::SkillsReload {
                session_id: "s".into(),
            },
            ActionRequest::ConfigSave { fields: json!({}) },
            ActionRequest::ConfigSetPermissionLevel { level: 1 },
            ActionRequest::ProfileApply { name: "p".into() },
            ActionRequest::ProfileSaveCurrent { name: "p".into() },
            ActionRequest::ProfileDelete { name: "p".into() },
            ActionRequest::SessionSetProfile {
                session_id: "s".into(),
                name: "p".into(),
            },
            ActionRequest::WorkspaceSet {
                session_id: "s".into(),
                path: "/".into(),
            },
            ActionRequest::WorkspaceCreate { path: "/".into() },
            ActionRequest::WorkspaceRename {
                id: "w".into(),
                title: "t".into(),
            },
            ActionRequest::WorkspaceDelete { id: "w".into() },
            ActionRequest::WorkspaceMoveSession {
                session_id: "s".into(),
                workspace_id: "w".into(),
            },
            ActionRequest::WorkspaceDetach {
                session_id: "s".into(),
            },
            ActionRequest::SessionSetToolMode {
                session_id: "s".into(),
                tool_mode: "minimal".into(),
                custom_tools: vec![],
            },
            ActionRequest::SubagentSpawn {
                tools: vec![],
                model: None,
                base_url: None,
                max_tokens: None,
                workspace: None,
            },
            ActionRequest::GitSwitchBranch {
                session_id: "s".into(),
                branch: "main".into(),
                stash: false,
            },
            ActionRequest::GitCommit {
                session_id: "s".into(),
                message: "checkpoint".into(),
            },
        ];
        // 穷举闸：见 all_query_requests。
        for a in &all {
            match a {
                ActionRequest::SkillsOperation { .. }
                | ActionRequest::SkillsReload { .. }
                | ActionRequest::ConfigSave { .. }
                | ActionRequest::ConfigSetPermissionLevel { .. }
                | ActionRequest::ProfileApply { .. }
                | ActionRequest::ProfileSaveCurrent { .. }
                | ActionRequest::ProfileDelete { .. }
                | ActionRequest::SessionSetProfile { .. }
                | ActionRequest::WorkspaceSet { .. }
                | ActionRequest::WorkspaceCreate { .. }
                | ActionRequest::WorkspaceRename { .. }
                | ActionRequest::WorkspaceDelete { .. }
                | ActionRequest::WorkspaceMoveSession { .. }
                | ActionRequest::WorkspaceDetach { .. }
                | ActionRequest::SessionSetToolMode { .. }
                | ActionRequest::SubagentSpawn { .. }
                | ActionRequest::GitSwitchBranch { .. }
                | ActionRequest::GitCommit { .. } => {}
            }
        }
        all
    }

    /// 路由形状：单一规范形态 `module.method`（服务面已拆 slash 别名，见
    /// daemon 的 `slash_alias_is_no_longer_accepted`）。
    fn assert_route_shape(name: &str) {
        let (module, method) = name.split_once('.').unwrap_or_else(|| {
            panic!("路由 {name:?} 不是 module.method 形态");
        });
        assert!(
            !module.is_empty()
                && !method.is_empty()
                && !method.contains('.')
                && module
                    .chars()
                    .chain(method.chars())
                    .all(|c| c.is_ascii_lowercase() || c == '_'),
            "路由 {name:?} 不是 module.method 形态"
        );
    }

    /// 路由不得重复：`into_parts` 的 match 是逐个手写的，复制粘贴极易让两个
    /// 变体落到同一个方法名上——**此时其中一个方法永远发不出去，且不报错**
    /// （静默失败，正是值得上闸的那一类）。
    ///
    /// 编译器不会替我们看这条：`ActionRequest` 的 `match` 里两个臂返回同一个
    /// 字符串是合法的（daemon 侧 `lookup` 撞名会触发 `unreachable_patterns`，
    /// 客户端这侧不会）。
    #[test]
    fn routes_are_pairwise_distinct_and_well_formed() {
        let mut seen = std::collections::HashSet::new();
        for (kind, name) in all_query_requests()
            .into_iter()
            .map(|q| ("query", q.into_parts().0))
            .chain(
                all_action_requests()
                    .into_iter()
                    .map(|a| ("action", a.into_parts().0)),
            )
        {
            assert_route_shape(name);
            assert!(seen.insert(name), "重复的 {kind} 路由: {name}");
        }
        // 两个枚举**合计** 33 条路由。条数写死是刻意的：它与上面两份清单一起
        // 构成「新增/删除方法必须显式过一次」的检查点。
        // 34 → 35：新增 `session.set_profile`（会话级 profile 选择，走既有
        // catch-all `POST /ringing/v2/service/{method}`，无新路由类型）。
        // 35 → 33：退役 `plan.context_stats` 与 `stats.token_usage`——前者的
        // 估算口径已被每轮 usage 真值取代，后者的 token_stats.jsonl 日报无消费者。
        assert_eq!(seen.len(), 33, "服务面路由总数变了——确认是新增而非改错");
    }
}
