//! Typed auxiliary HTTP endpoints used beside the Ringing event/command plane.
//!
//! These enums keep legacy service method names and JSON assembly inside the
//! transport crate. Native shells choose a closed Rust variant; they cannot
//! mistype a method name, send a mutation through the query route, or invent a
//! second renderer-facing protocol.

use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryRequest {
    SessionList,
    SessionActivity,
    ConfigLoad,
    WorkspaceStatus,
    SkillsListTools,
    WorkspaceDiagnose,
}

impl QueryRequest {
    pub(crate) fn into_parts(self) -> (&'static str, Value) {
        let name = match self {
            Self::SessionList => "session.list",
            Self::SessionActivity => "session.activity",
            Self::ConfigLoad => "config.load",
            Self::WorkspaceStatus => "workspace.status",
            Self::SkillsListTools => "skills.list_tools",
            Self::WorkspaceDiagnose => "workspace.diagnose",
        };
        (name, json!({}))
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
    WorkspaceSetMode {
        mode: String,
    },
    WorkspaceInstallWsl,
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
            Self::WorkspaceSetMode { mode } => ("workspace.set_mode", json!({ "mode": mode })),
            Self::WorkspaceInstallWsl => ("workspace.install_wsl", json!({})),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
