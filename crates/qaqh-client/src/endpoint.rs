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
}

impl QueryRequest {
    pub(crate) fn into_parts(self) -> (&'static str, Value) {
        match self {
            Self::SessionList => ("session.list", json!({})),
            Self::SessionActivity => ("session.activity", json!({})),
            Self::ConfigLoad => ("config.load", json!({})),
            Self::WorkspaceList => ("workspace.list", json!({})),
            Self::SkillsListTools => ("skills.list_tools", json!({})),
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::QueryRequest;
    use serde_json::json;

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
}
