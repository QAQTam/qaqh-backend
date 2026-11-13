//! visualize 工具（widget-directive C2：结构化 RingingV2 指令）。
//!
//! 模型把 fragment 写进 widgets_dir 后调用本工具声明渲染意图。args 仅
//! `path/title/mode`，全程无 PUA 码位——免疫云传输对私用区"隐形字符"的
//! 双向清洗（Qoder/qwen 实测剥 U+E200/E201/E202）。前端从 ToolStep.argsJson
//! 挂 iframe 沙箱（host/bridge/widget-states 不变）；本工具只校验形状并回
//! ack，渲染由前端完成，状态回流仍走既有 `<widget-state>` reminder 通道。

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ToolRisk;
use crate::tool_api::{
    ToolBody, ToolCallContext, ToolContentBlock, ToolDisplay, ToolExecutionError, ToolHeader,
    ToolMeta, ToolProjection, TypedTool,
};

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VisualizeArgs {
    /// Absolute forward-slash path of the widget fragment (`.html`) under widgets_dir.
    pub path: String,
    /// Short title for the widget card; defaults to the fragment basename.
    pub title: Option<String>,
    /// Optional layout mode; `"wide"` requests full-width.
    pub mode: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct VisualizeOutput {
    pub status: String,
    pub path: String,
    pub title: String,
    pub mode: Option<String>,
}

impl ToolProjection for VisualizeOutput {
    fn model_blocks(&self) -> Vec<ToolContentBlock> {
        vec![ToolContentBlock::Text {
            text: serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string()),
        }]
    }

    fn display(&self, _args: &Value) -> ToolDisplay {
        ToolDisplay::new(
            ToolHeader::Other {
                label: "visualize".to_string(),
            },
            ToolBody::None,
        )
        .with_summary(format!("widget: {}", self.title))
    }
}

pub struct VisualizeTool;

impl TypedTool for VisualizeTool {
    type Args = VisualizeArgs;
    type Output = VisualizeOutput;

    fn meta(&self) -> ToolMeta {
        ToolMeta::new(
            "visualize",
            "Render an interactive in-chat widget from an HTML fragment you wrote into widgets_dir. Call this after writing the fragment; args carry only path/title/mode (no special characters). The host mounts a sandboxed iframe at this spot. Load qaqh-visualize for the fragment contract and host bridge.",
            crate::permission::ToolCategory::Read,
            ToolRisk::ReadOnly,
            std::time::Duration::ZERO,
        )
    }

    #[allow(clippy::result_large_err)] // ToolExecutionError is the frozen typed boundary.
    fn run(
        &self,
        _ctx: &ToolCallContext,
        args: Self::Args,
    ) -> Result<Self::Output, ToolExecutionError> {
        let path = args.path.trim();
        if path.is_empty() || !path.to_ascii_lowercase().ends_with(".html") {
            return Err(ToolExecutionError::Recoverable(
                crate::tool_api::ToolError::invalid_arguments(
                    "visualize: `path` must be a non-empty absolute .html path under widgets_dir"
                        .to_string(),
                )
                .with_retryable(true)
                .with_hint(
                    "Write the fragment first, then call visualize with its forward-slash absolute .html path.",
                ),
            ));
        }
        let title = args.title.unwrap_or_else(|| {
            path.rsplit(['/', '\\'])
                .next()
                .unwrap_or("widget")
                .to_string()
        });
        Ok(VisualizeOutput {
            status: "ok".to_string(),
            path: path.to_string(),
            title,
            mode: args.mode,
        })
    }
}

pub fn register(mgr: &mut crate::ToolManager) {
    mgr.register_typed(VisualizeTool);
}