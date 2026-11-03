use serde::{Deserialize, Serialize};

/// Timestamped code-change record used by tool execution and audit logging.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeDeltaRecord {
    pub timestamp: u64,
    pub lines_added: usize,
    pub lines_removed: usize,
    pub files_created: usize,
    pub files_deleted: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
}
