//! Safety verdicts for tool invocation.

use std::path::{Path, PathBuf};

use crate::ToolRisk;

#[derive(Debug, Clone)]
pub enum SafetyVerdict {
    Allow,
    Block(String),
}

pub struct SafetyPolicy;

impl SafetyPolicy {
    pub fn evaluate(risk: ToolRisk, in_workspace: bool) -> SafetyVerdict {
        match (risk, in_workspace) {
            (ToolRisk::ReadOnly, _) => SafetyVerdict::Allow,
            (ToolRisk::Write, _) => SafetyVerdict::Allow,
            (ToolRisk::Destructive, false) => {
                SafetyVerdict::Block("Destructive operation outside workspace is blocked".into())
            }
            (ToolRisk::Destructive, true) => SafetyVerdict::Allow,
            (ToolRisk::Administrative, _) => SafetyVerdict::Allow,
        }
    }

    pub fn evaluate_tool_call(
        tool_name: &str,
        args: &serde_json::Value,
        workspace_root: &Path,
        risk: ToolRisk,
        in_workspace: bool,
    ) -> SafetyVerdict {
        if let SafetyVerdict::Block(reason) = Self::evaluate(risk, in_workspace) {
            return SafetyVerdict::Block(reason);
        }
        if tool_name == "apply_patch"
            && !workspace_root.as_os_str().is_empty()
            && workspace_root != Path::new(".")
            && let Some(patch) = args
                .get("input")
                .or_else(|| args.get("patch"))
                .and_then(|v| v.as_str())
            && let Ok(parsed) = crate::apply_patch_engine::parse_patch(patch)
        {
            let ws_norm = crate::permission::normalize_lexically(workspace_root);
            for hunk in &parsed.hunks {
                if let crate::apply_patch_engine::Hunk::DeleteFile { path } = hunk {
                    let abs_path = if path.is_absolute() {
                        PathBuf::from(path)
                    } else {
                        workspace_root.join(path)
                    };
                    let path_norm = crate::permission::normalize_lexically(&abs_path);
                    if !path_norm.starts_with(&ws_norm) {
                        return SafetyVerdict::Block(
                            "Destructive operation outside workspace is blocked".into(),
                        );
                    }
                }
            }
        }
        SafetyVerdict::Allow
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_patch_delete_outside_workspace_is_blocked_while_add_stays_allowed() {
        let ws = Path::new("/tmp/qaqh-safety-ws");
        let del_rel = serde_json::json!({
            "input": "*** Begin Patch\n*** Delete File: ../escape.txt\n*** End Patch"
        });
        assert!(matches!(
            SafetyPolicy::evaluate_tool_call("apply_patch", &del_rel, ws, ToolRisk::Write, true),
            SafetyVerdict::Block(_)
        ));

        let del_abs = serde_json::json!({
            "input": "*** Begin Patch\n*** Delete File: /tmp/other/escape.txt\n*** End Patch",
            "dry_run": true
        });
        assert!(matches!(
            SafetyPolicy::evaluate_tool_call("apply_patch", &del_abs, ws, ToolRisk::Write, true),
            SafetyVerdict::Block(_)
        ));

        let del_in = serde_json::json!({
            "input": "*** Begin Patch\n*** Delete File: src/inside.txt\n*** End Patch"
        });
        assert!(matches!(
            SafetyPolicy::evaluate_tool_call("apply_patch", &del_in, ws, ToolRisk::Write, true),
            SafetyVerdict::Allow
        ));

        let add_out = serde_json::json!({
            "input": "*** Begin Patch\n*** Add File: ../outside.txt\n+ok\n*** End Patch"
        });
        assert!(matches!(
            SafetyPolicy::evaluate_tool_call("apply_patch", &add_out, ws, ToolRisk::Write, true),
            SafetyVerdict::Allow
        ));
    }
}
