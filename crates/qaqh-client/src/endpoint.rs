//! Typed read-only service queries. Mutations use `RingingCommand` and the
//! shared RingingV2 command envelope.

use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryRequest {
    SessionList,
    SessionActivity,
    ConfigLoad,
    WorkspaceList,
    SkillsListTools,
    SkillsCatalog {
        cwd: Option<String>,
    },
    FsList {
        path: String,
    },
    FsRead {
        path: String,
        max_bytes: Option<u64>,
    },
    SessionDashboard {
        session_id: String,
    },
    TodoStatus {
        session_id: String,
    },
    SessionMeta {
        session_id: String,
    },
    PlanRead {
        session_id: String,
    },
    /// 按需取回被逐出的单个回合（exec 会话卡片回源）。
    SessionTurn {
        session_id: String,
        turn_id: String,
    },
    DaemonVersion,
    SessionGetActivity {
        session_id: String,
    },
    WorkspaceGet {
        session_id: String,
    },
    ProfileList,
    /// `todo.list` 透传查询参数（`session_id` 必带，其余 flatten 原样转发）。
    TodoList {
        params: Value,
    },
    DiagnosticsMemorySnapshot {
        after_sequence: Option<u64>,
    },
}

impl QueryRequest {
    /// 该查询对应的服务面方法名（`service_methods` 清单与调用方白名单共用）。
    pub fn method_name(&self) -> &'static str {
        self.clone().into_parts().0
    }

    /// 封闭枚举覆盖的全部服务面方法名（与 [`Self::try_from_service`] 同源）。
    /// 绑定层用它生成调用方白名单，避免再抄一份字面量。
    pub fn query_service_methods() -> &'static [&'static str] {
        &[
            "daemon.version",
            "session.list",
            "session.activity",
            "config.load",
            "workspace.list",
            "skills.list_tools",
            "profile.list",
            "skills.catalog",
            "fs.list",
            "fs.read",
            "session.dashboard",
            "todo.status",
            "session.meta",
            "plan.read",
            "session.turn",
            "session.get_activity",
            "workspace.get",
            "todo.list",
            "diagnostics.memory.snapshot",
        ]
    }

    /// 服务面 method+params 的封闭枚举入口：命中返回变体，未命中返回 `None`
    /// 交由调用方走账号面 `service_call` 白名单。
    pub fn try_from_service(method: &str, params: &Value) -> Option<Self> {
        let session_id = || {
            params
                .get("session_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        Some(match method {
            "daemon.version" => Self::DaemonVersion,
            "session.list" => Self::SessionList,
            "session.activity" => Self::SessionActivity,
            "config.load" => Self::ConfigLoad,
            "workspace.list" => Self::WorkspaceList,
            "skills.list_tools" => Self::SkillsListTools,
            "profile.list" => Self::ProfileList,
            "skills.catalog" => Self::SkillsCatalog {
                cwd: params
                    .get("cwd")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            },
            "fs.list" => Self::FsList {
                path: params.get("path")?.as_str()?.to_string(),
            },
            "fs.read" => Self::FsRead {
                path: params.get("path")?.as_str()?.to_string(),
                max_bytes: params.get("max_bytes").and_then(Value::as_u64),
            },
            "session.dashboard" => Self::SessionDashboard { session_id: session_id()? },
            "todo.status" => Self::TodoStatus { session_id: session_id()? },
            "session.meta" => Self::SessionMeta { session_id: session_id()? },
            "plan.read" => Self::PlanRead { session_id: session_id()? },
            "session.turn" => Self::SessionTurn {
                session_id: session_id()?,
                turn_id: params.get("turn_id")?.as_str()?.to_string(),
            },
            "session.get_activity" => Self::SessionGetActivity { session_id: session_id()? },
            "workspace.get" => Self::WorkspaceGet { session_id: session_id()? },
            "todo.list" => {
                let mut forwarded = params.clone();
                forwarded["session_id"] = json!(session_id()?);
                Self::TodoList { params: forwarded }
            }
            "diagnostics.memory.snapshot" => Self::DiagnosticsMemorySnapshot {
                after_sequence: params.get("after_sequence").and_then(Value::as_u64),
            },
            _ => return None,
        })
    }

    pub(crate) fn into_parts(self) -> (&'static str, Value) {
        match self {
            Self::SessionList => ("session.list", json!({})),
            Self::SessionActivity => ("session.activity", json!({})),
            Self::ConfigLoad => ("config.load", json!({})),
            Self::WorkspaceList => ("workspace.list", json!({})),
            Self::SkillsListTools => ("skills.list_tools", json!({})),
            Self::ProfileList => ("profile.list", json!({})),
            Self::DaemonVersion => ("daemon.version", json!({})),
            Self::SessionGetActivity { session_id } => {
                ("session.get_activity", json!({ "session_id": session_id }))
            }
            Self::WorkspaceGet { session_id } => {
                ("workspace.get", json!({ "session_id": session_id }))
            }
            Self::TodoList { params } => ("todo.list", params),
            Self::DiagnosticsMemorySnapshot { after_sequence } => {
                let mut params = json!({});
                if let Some(after_sequence) = after_sequence {
                    params["after_sequence"] = json!(after_sequence);
                }
                ("diagnostics.memory.snapshot", params)
            }
            Self::SkillsCatalog { cwd } => {
                let mut params = json!({});
                if let Some(cwd) = cwd {
                    params["cwd"] = json!(cwd);
                }
                ("skills.catalog", params)
            }
            Self::FsList { path } => ("fs.list", json!({ "path": path })),
            Self::FsRead { path, max_bytes } => {
                let mut params = json!({ "path": path });
                if let Some(max_bytes) = max_bytes {
                    params["max_bytes"] = json!(max_bytes);
                }
                ("fs.read", params)
            }
            Self::SessionDashboard { session_id } => {
                ("session.dashboard", json!({ "session_id": session_id }))
            }
            Self::TodoStatus { session_id } => ("todo.status", json!({ "session_id": session_id })),
            Self::SessionMeta { session_id } => {
                ("session.meta", json!({ "session_id": session_id }))
            }
            Self::PlanRead { session_id } => ("plan.read", json!({ "session_id": session_id })),
            Self::SessionTurn { session_id, turn_id } => {
                ("session.turn", json!({ "session_id": session_id, "turn_id": turn_id }))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::QueryRequest;
    use serde_json::{Value, json};

    #[test]
    fn query_encodes_service_method_and_canonical_session_key() {
        let (name, params) = QueryRequest::SessionMeta {
            session_id: "s1".into(),
        }
        .into_parts();
        assert_eq!(name, "session.meta");
        assert_eq!(params, json!({ "session_id": "s1" }));
        assert!(params.get("seed").is_none());
    }

    #[test]
    fn service_roundtrip_covers_desktop_shell_surface() {
        // 命中封闭枚举的入口必须 method_name ↔ try_from_service 往返一致。
        let cases: Vec<(&str, Value)> = vec![
            ("daemon.version", json!({})),
            ("session.list", json!({})),
            ("session.activity", json!({})),
            ("config.load", json!({})),
            ("workspace.list", json!({})),
            ("profile.list", json!({})),
            ("skills.list_tools", json!({})),
            ("skills.catalog", json!({ "cwd": "/tmp" })),
            ("fs.list", json!({ "path": "/tmp" })),
            (
                "fs.read",
                json!({ "path": "/tmp", "max_bytes": 64 }),
            ),
            ("session.dashboard", json!({ "session_id": "s1" })),
            ("todo.status", json!({ "session_id": "s1" })),
            ("session.meta", json!({ "session_id": "s1" })),
            ("plan.read", json!({ "session_id": "s1" })),
            ("session.get_activity", json!({ "session_id": "s1" })),
            ("workspace.get", json!({ "session_id": "s1" })),
            (
                "todo.list",
                json!({ "session_id": "s1", "status": "open" }),
            ),
            ("diagnostics.memory.snapshot", json!({ "after_sequence": 9 })),
        ];
        for (method, params) in cases {
            let request = QueryRequest::try_from_service(method, &params)
                .unwrap_or_else(|| panic!("{method} 应命中封闭枚举"));
            assert_eq!(request.method_name(), method);
            let (_, encoded) = request.into_parts();
            if method == "todo.list" {
                // todo.list 透传额外查询参数，且 session_id 必在。
                assert_eq!(encoded["session_id"], json!("s1"));
                assert_eq!(encoded["status"], json!("open"));
            } else {
                assert_eq!(encoded, params, "{method} 参数应原样编码");
            }
        }
    }

    #[test]
    fn service_roundtrip_rejects_missing_session_and_unknown_methods() {
        assert!(QueryRequest::try_from_service("session.meta", &json!({})).is_none());
        assert!(QueryRequest::try_from_service("siwc.status", &json!({})).is_none());
        assert!(QueryRequest::try_from_service("unknown.method", &json!({})).is_none());
        // todo.list 缺 session_id 同样拒绝（透传不等于缺参会话键）。
        assert!(QueryRequest::try_from_service("todo.list", &json!({ "status": "open" })).is_none());
    }
}
