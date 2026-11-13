//! NOTE: Disconnected from dsx-agent — dsx-agent uses its own stubs in health.rs.
//! This module is used by the dsx-hp binary internally. Wire up via IPC later.
//!
//! Health report rendering: format health assessment for display and AI consumption.

use std::sync::Mutex;

use dsx_types::ToolDef;

use crate::platform::DsAgentsHealthPlatform;

static LAST_HEALTH_REPORT: Mutex<String> = Mutex::new(String::new());

impl DsAgentsHealthPlatform {
    /// Render a compact health report (for the `health` tool).
    pub fn render_health(&self) -> String {
        let success_rate = if self.last_10_success.is_empty() {
            1.0
        } else {
            self.last_10_success
                .iter()
                .filter(|&&b| b)
                .count() as f64
                / self.last_10_success.len() as f64
        };
        let emotion = self.infer_emotion();

        let mut top_cats: Vec<String> = self
            .error_counts
            .iter()
            .map(|(k, v)| format!("{:?}:{}", k, v))
            .collect();
        top_cats.sort_by_key(|s| std::cmp::Reverse(s.len()));

        let recent: Vec<String> = self
            .recent_errors
            .iter()
            .rev()
            .take(5)
            .map(|e| {
                format!(
                    "  T{} {} [{:?}] {}",
                    e.turn,
                    e.tool,
                    e.category,
                    truncate_msg(&e.message, 80)
                )
            })
            .collect();

        format!(
            "level:{:?} emotion:{} {} turn:{} success_rate:{:.0}% consecutive_failures:{}\n\
             tool_loop:{} tool_only_turns:{}\n\
             tier:{:?} context:{:?} tokens:{}/{} empty_responses:{}\n\
             api_retries:{} transport_errors:{}\n\
             exec_pending:{} timeouts:{} sudo_failures:{}\n\
             error_categories: {}\n\
             recent_errors:\n{}",
            self.overall_level,
            emotion.emoji(),
            emotion.label(),
            self.turn,
            success_rate * 100.0,
            self.consecutive_failures,
            self.tool_loop_count,
            self.consecutive_tool_only_turns,
            self.context_tier,
            self.context_pressure,
            self.context_tokens,
            self.context_limit,
            self.empty_responses,
            self.api_retries,
            self.transport_errors,
            self.exec_pending,
            self.exec_timeouts,
            self.sudo_failures,
            top_cats.join(" "),
            recent.join("\n"),
        )
    }
}

fn truncate_msg(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max])
    }
}

// ── Health tool def + exec ──

#[allow(dead_code)]
pub fn health_def() -> ToolDef {
    ToolDef {
        call_type: "function".into(),
        function: dsx_types::ToolFunction {
            name: "health".into(),
            description: "Query system health: error trends, context pressure, advice.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {},
                "required": [],
                "additionalProperties": false
            }),
        },
    }
}

/// Called by TUI after assess() to make the report available to the health tool.
pub fn update_health_report(report: String) {
    if let Ok(mut r) = LAST_HEALTH_REPORT.lock() {
        *r = report;
    }
}

pub fn exec_health(_args: &str) -> String {
    match LAST_HEALTH_REPORT.lock() {
        Ok(r) if !r.is_empty() => format!("[OK] Health report:\n{}", *r),
        _ => "[OK] Health platform not yet assessed. This is normal if no turns have completed yet."
            .into(),
    }
}
