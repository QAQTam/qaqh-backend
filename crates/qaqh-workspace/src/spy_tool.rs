//! `spy` 工具：qaqh-spy 的模型面入口。
//!
//! 恢复闭环的单表面：`journal` 查流水定位 change_id → `undo` 单变更回滚 →
//! `restore` 整库回扫描点 → `cat` 看历史版本内容。全部走
//! `execute_authorized_with_context` 漏斗，undo/restore 自己的写入会被批边界
//! 审计正常记账（内容寻址去重，自指无碍）。
//!
//! 权限分档（当前实现）：工具整体 Write/Write（同 `journal` 先例）；
//! `restore + prune` 为删除性操作，默认 false，true 时在 model_text 显式回显
//! 删除清单——若后续 permission 支持按 action 细分，可对该组合单独拦截。

use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::file_mutate::mutation_error;
use crate::tool_api::{
    ToolBody, ToolCallContext, ToolContentBlock, ToolDisplay, ToolExecutionError, ToolHeader,
    ToolMeta, ToolProjection, TypedTool,
};

/// cat 文本回显的字节上限（blob 本身可能远大于此）。
const CAT_DISPLAY_BYTES: usize = 16 * 1024;

pub struct SpyTool;

/// `spy` 工具参数。所有可选字段按 action 取用，无关字段忽略。
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SpyArgs {
    /// undo=single-change rollback; restore=workspace back to scan point; journal=change flow; cat=historical blob
    pub action: String,
    /// Change id for undo
    pub change_id: Option<String>,
    /// Scan id / mark for restore
    pub scan_id: Option<String>,
    /// undo: skip after-sha guard (default false)
    pub force: Option<bool>,
    /// restore: delete extra files newer than the scan point (default false)
    pub prune: Option<bool>,
    /// journal: max entries (default 50)
    pub limit: Option<usize>,
    /// blob sha for cat
    pub sha: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SpyOutput {
    pub timeis: String,
    pub status: String,
    pub action: String,
    /// 结构化结果（undo/restore 的 outcome 字段；journal/cat 为空）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<Value>,
    pub model_text: String,
}

impl ToolProjection for SpyOutput {
    fn model_blocks(&self) -> Vec<ToolContentBlock> {
        vec![ToolContentBlock::Text {
            text: self.model_text.clone(),
        }]
    }

    fn display(&self, args: &Value) -> ToolDisplay {
        let action = args
            .get("action")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|action| !action.is_empty())
            .unwrap_or("spy");
        ToolDisplay::new(
            ToolHeader::Other {
                label: format!("spy {action}"),
            },
            ToolBody::Text {
                text: self.model_text.clone(),
                truncated: false,
            },
        )
    }
}

impl TypedTool for SpyTool {
    type Args = SpyArgs;
    type Output = SpyOutput;

    fn meta(&self) -> ToolMeta {
        ToolMeta::new(
            "spy",
            "Workspace change audit (qaqh-spy): undo a single change, \
                restore the whole workspace to a scan point, journal the change flow, \
                cat a historical blob.",
            crate::permission::ToolCategory::Write,
            crate::ToolRisk::Write,
            Duration::from_secs(30),
        )
    }

    #[allow(clippy::result_large_err)] // ToolExecutionError is the frozen typed boundary.
    fn run(
        &self,
        ctx: &ToolCallContext,
        args: Self::Args,
    ) -> Result<Self::Output, ToolExecutionError> {
        let root = ctx.workspace_root.to_string_lossy().to_string();
        if root.is_empty() || root == "." {
            return Err(spy_error(
                "no_workspace",
                "spy requires a workspace root (none is set)",
            ));
        }
        let session = qaqh_spy::Session::open(std::path::PathBuf::from(&root), None)
            .map_err(|e| spy_error("session_open_failed", e.to_string()))?;

        match args.action.as_str() {
            "journal" => {
                let limit = args.limit.unwrap_or(50).min(500);
                let journal = session
                    .journal()
                    .map_err(|e| spy_error("journal_failed", e.to_string()))?;
                let start = journal.len().saturating_sub(limit);
                let mut lines = Vec::with_capacity(journal.len() - start + 1);
                lines.push(format!(
                    "[OK] spy journal: {} change(s)（窗口外条目已被 GC 压缩）",
                    journal.len() - start
                ));
                for c in &journal[start..] {
                    let status = match c.status {
                        qaqh_spy::store::ChangeStatus::Added => "added",
                        qaqh_spy::store::ChangeStatus::Modified => "modified",
                        qaqh_spy::store::ChangeStatus::Deleted => "deleted",
                    };
                    lines.push(format!(
                        "{} {} {} ({} → {})",
                        c.id,
                        status,
                        c.path,
                        c.size_before.map(fmt_size).unwrap_or_else(|| "-".into()),
                        c.size_after.map(fmt_size).unwrap_or_else(|| "-".into()),
                    ));
                }
                lines.push(
                    "回滚单条：spy action=undo change_id=<id>；整库回扫描点：spy action=restore scan_id=<mark>"
                        .to_string(),
                );
                Ok(SpyOutput {
                    timeis: crate::now_utc8(),
                    status: "ok".to_string(),
                    action: "journal".to_string(),
                    outcome: None,
                    model_text: lines.join("\n") + "\n",
                })
            }
            "undo" => {
                let change_id = required(&args.change_id, "change_id")?;
                let out = session
                    .undo(change_id, args.force.unwrap_or(false))
                    .map_err(|e| spy_error("undo_failed", e.to_string()))?;
                Ok(SpyOutput {
                    timeis: crate::now_utc8(),
                    status: "ok".to_string(),
                    action: "undo".to_string(),
                    outcome: Some(json!({ "action": out.action, "path": out.path })),
                    model_text: format!(
                        "[OK] spy undo: {} {}（回滚本身会被批边界审计记账；可用 spy action=journal 复核）\n",
                        out.action, out.path
                    ),
                })
            }
            "restore" => {
                let scan_id = required(&args.scan_id, "scan_id")?;
                let prune = args.prune.unwrap_or(false);
                let out = session
                    .restore(&qaqh_spy::ScanId(scan_id.to_string()), prune)
                    .map_err(|e| spy_error("restore_failed", e.to_string()))?;
                let mut text = format!(
                    "[OK] spy restore: 重写 {}，未变 {}，多余 {}（prune 删除 {}）\n",
                    out.written,
                    out.skipped,
                    out.extras.len(),
                    out.pruned
                );
                if !out.extras.is_empty() {
                    text.push_str(&format!("extras: {:?}\n", out.extras));
                }
                if prune && out.pruned > 0 {
                    text.push_str("⚠ prune 已删除文件；如需找回请评估后再操作\n");
                }
                Ok(SpyOutput {
                    timeis: crate::now_utc8(),
                    status: "ok".to_string(),
                    action: "restore".to_string(),
                    outcome: Some(json!({
                        "written": out.written,
                        "skipped": out.skipped,
                        "extras": out.extras,
                        "pruned": out.pruned,
                    })),
                    model_text: text,
                })
            }
            "cat" => {
                let sha = required(&args.sha, "sha")?;
                let bytes = session
                    .cat(sha)
                    .map_err(|e| spy_error("cat_failed", e.to_string()))?;
                let end = bytes.len().min(CAT_DISPLAY_BYTES);
                let mut text = String::from_utf8_lossy(&bytes[..end]).to_string();
                if bytes.len() > end {
                    text.push_str(&format!(
                        "\n…（截断显示 {end}/{} 字节；二进制内容以 lossy 文本呈现）",
                        bytes.len()
                    ));
                }
                Ok(SpyOutput {
                    timeis: crate::now_utc8(),
                    status: "ok".to_string(),
                    action: "cat".to_string(),
                    outcome: None,
                    model_text: text + "\n",
                })
            }
            other => Err(spy_error(
                "invalid_action",
                format!("invalid spy action {other:?} — use undo, restore, journal, or cat"),
            )),
        }
    }
}

fn required<'a>(value: &'a Option<String>, name: &str) -> Result<&'a str, ToolExecutionError> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| spy_error(&format!("missing_{name}"), format!("spy requires '{name}'")))
}

fn spy_error(code: &str, message: impl Into<String>) -> ToolExecutionError {
    mutation_error(
        code,
        message,
        None,
        json!({
            "timeis": crate::now_utc8(),
            "status": "error",
            "code": code,
        }),
    )
}

fn fmt_size(n: u64) -> String {
    if n < 1024 {
        format!("{n}B")
    } else {
        format!("{:.1}KB", n as f64 / 1024.0)
    }
}

/// Register the `spy` workspace tool.
pub fn register(mgr: &mut crate::ToolManager) {
    mgr.register_typed(SpyTool);
}
