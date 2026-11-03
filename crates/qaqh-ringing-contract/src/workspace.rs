use serde::{Deserialize, Serialize};

/// Persistent UI workspace grouping returned by `workspace.create` and rename.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct WorkspaceMeta {
    pub id: String,
    pub path: String,
    pub title: String,
    pub order: u32,
    pub session_ids: Vec<String>,
}

/// `workspace.list` entry, including whether its registered directory exists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
pub struct WorkspaceEntry {
    pub id: String,
    pub path: String,
    pub title: String,
    pub order: u32,
    pub session_ids: Vec<String>,
    pub missing_dir: bool,
}

impl WorkspaceMeta {
    pub fn entry(&self, missing_dir: bool) -> WorkspaceEntry {
        WorkspaceEntry {
            id: self.id.clone(),
            path: self.path.clone(),
            title: self.title.clone(),
            order: self.order,
            session_ids: self.session_ids.clone(),
            missing_dir,
        }
    }
}
