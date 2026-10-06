//! Agent Skill 工具（SDK v2 三件套）。
//!
//! 原 `skills` 聚合工具（action + oneOf 判别）拆为 `skill_activate` /
//! `skill_list` / `skill_resource` 三个单职责工具；`validate` 动作自模型面
//! 退役（`qaqh_skills::validate_file` 保留为宿主侧诊断入口）。skill activation
//! 经 typed effect 传输，不从工具文本反解。

#![allow(clippy::result_large_err)] // TypedTool's frozen public error boundary.

use std::path::{Path, PathBuf};
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::tool_api::{
    ToolBody, ToolCallContext, ToolContentBlock, ToolDisplay, ToolError, ToolErrorCode,
    ToolErrorKind, ToolExecutionError, ToolHeader, ToolMeta, ToolProjection, TypedTool,
    clamp_display_body,
};
use crate::{ToolEffect, ToolRisk};

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
/// Activate a skill by exact catalog name.
pub struct SkillActivateArgs {
    /// Skill name (exact match from the current skill catalog).
    pub name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
/// List the effective skill catalog.
pub struct SkillListArgs {}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
/// Read a bundled skill resource file.
pub struct SkillResourceArgs {
    /// Skill name (exact match from the current skill catalog).
    pub name: String,
    /// Resource path relative to the skill's manifest.
    pub path: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SkillListEntry {
    name: String,
    description: String,
    scope: &'static str,
    source: PathBuf,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SkillDiagnosticEntry {
    severity: &'static str,
    source: PathBuf,
    message: String,
}

#[derive(Debug, Serialize, JsonSchema)]
/// Activation result; instructions arrive via the trailing system envelope.
pub struct SkillActivateOutput {
    status: &'static str,
    skill: String,
    resources: Vec<String>,
    content: String,
    #[serde(skip)]
    #[schemars(skip)]
    activation: qaqh_skills::SkillActivation,
}

#[derive(Debug, Serialize, JsonSchema)]
/// Effective skill catalog with load diagnostics.
pub struct SkillListOutput {
    skills: Vec<SkillListEntry>,
    diagnostics: Vec<SkillDiagnosticEntry>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(transparent)]
/// Bundled skill resource content.
pub struct SkillResourceOutput(String);

impl ToolProjection for SkillActivateOutput {
    fn model_blocks(&self) -> Vec<ToolContentBlock> {
        vec![ToolContentBlock::Text {
            text: serde_json::to_string(self).unwrap_or_default(),
        }]
    }

    fn display(&self, _args: &serde_json::Value) -> ToolDisplay {
        ToolDisplay::new(
            crate::tool_api::ToolHeader::Other {
                label: "skills activate".into(),
            },
            crate::tool_api::ToolBody::None,
        )
        .with_summary(self.content.clone())
    }

    fn effects(&self) -> Vec<ToolEffect> {
        vec![ToolEffect::Skill(qaqh_skills::SkillEffect::Activate(
            self.activation.clone(),
        ))]
    }
}

impl ToolProjection for SkillListOutput {
    fn model_blocks(&self) -> Vec<ToolContentBlock> {
        vec![ToolContentBlock::Text {
            text: serde_json::to_string(self).unwrap_or_default(),
        }]
    }

    fn display(&self, _args: &serde_json::Value) -> ToolDisplay {
        ToolDisplay::new(
            crate::tool_api::ToolHeader::Other {
                label: "skills list".into(),
            },
            crate::tool_api::ToolBody::None,
        )
        .with_summary(format!(
            "listed {} skills · {} diagnostics",
            self.skills.len(),
            self.diagnostics.len()
        ))
    }
}

impl ToolProjection for SkillResourceOutput {
    fn model_blocks(&self) -> Vec<ToolContentBlock> {
        vec![ToolContentBlock::Text {
            text: self.0.clone(),
        }]
    }

    fn display(&self, args: &serde_json::Value) -> ToolDisplay {
        let (text, truncated) = clamp_display_body(&self.0);
        let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
        ToolDisplay::new(
            ToolHeader::Other {
                label: "skills resource".into(),
            },
            ToolBody::Text { text, truncated },
        )
        .with_summary(format!("resource · {name}/{path}"))
    }
}

pub struct SkillActivateTool;

impl TypedTool for SkillActivateTool {
    type Args = SkillActivateArgs;
    type Output = SkillActivateOutput;

    fn meta(&self) -> ToolMeta {
        ToolMeta::new(
            "skill_activate",
            "Activate a skill: injects the full instructions as a trailing <skill_context_envelope> system message (authoritative — it replaces all older skill instructions).",
            crate::permission::ToolCategory::Read,
            ToolRisk::ReadOnly,
            Duration::from_secs(15),
        )
    }

    fn run(
        &self,
        ctx: &ToolCallContext,
        args: SkillActivateArgs,
    ) -> Result<Self::Output, ToolExecutionError> {
        activate_skill(ctx, &args.name)
    }
}

pub struct SkillListTool;

impl TypedTool for SkillListTool {
    type Args = SkillListArgs;
    type Output = SkillListOutput;

    fn meta(&self) -> ToolMeta {
        ToolMeta::new(
            "skill_list",
            "List the effective skill catalog (names, descriptions, scopes) with load diagnostics.",
            crate::permission::ToolCategory::Read,
            ToolRisk::ReadOnly,
            Duration::from_secs(15),
        )
    }

    fn run(
        &self,
        ctx: &ToolCallContext,
        _args: SkillListArgs,
    ) -> Result<Self::Output, ToolExecutionError> {
        list_skills(ctx)
    }
}

pub struct SkillResourceTool;

impl TypedTool for SkillResourceTool {
    type Args = SkillResourceArgs;
    type Output = SkillResourceOutput;

    fn meta(&self) -> ToolMeta {
        ToolMeta::new(
            "skill_resource",
            "Read a bundled skill resource file on demand (relative path from the activated skill's manifest).",
            crate::permission::ToolCategory::Read,
            ToolRisk::ReadOnly,
            Duration::from_secs(15),
        )
    }

    fn run(
        &self,
        ctx: &ToolCallContext,
        args: SkillResourceArgs,
    ) -> Result<Self::Output, ToolExecutionError> {
        read_resource(ctx, &args.name, &args.path)
    }
}

fn activate_skill(
    ctx: &ToolCallContext,
    name: &str,
) -> Result<SkillActivateOutput, ToolExecutionError> {
    let activation = qaqh_skills::load_named(&ctx.workspace_root, name).map_err(|error| {
        recoverable(
            legacy_error(ToolErrorKind::NotFound, "skill_not_available", error)
                .with_hint("Use an exact name from the current skill catalog."),
        )
    })?;
    let skill = activation.metadata.name.clone();
    let resources = activation
        .resources
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    let content = format!(
        "[OK] skill '{skill}' activated. The full instructions are injected as the trailing <skill_context_envelope> system message (authoritative — it replaces all older skill instructions). If the envelope is not visible, call skill_resource to read bundled files on demand."
    );
    Ok(SkillActivateOutput {
        status: "ok",
        skill,
        resources,
        content,
        activation,
    })
}

fn list_skills(ctx: &ToolCallContext) -> Result<SkillListOutput, ToolExecutionError> {
    let catalog = qaqh_skills::discover(&ctx.workspace_root);
    let skills = catalog
        .skills
        .iter()
        .map(|skill| SkillListEntry {
            name: skill.name.clone(),
            description: skill.description.clone(),
            scope: match skill.scope {
                qaqh_skills::SkillScope::Project => "project",
                qaqh_skills::SkillScope::User => "user",
            },
            source: skill.path.clone(),
        })
        .collect();
    let diagnostics = catalog
        .diagnostics
        .iter()
        .map(|diagnostic| SkillDiagnosticEntry {
            severity: match diagnostic.severity {
                qaqh_skills::DiagnosticSeverity::Warning => "warning",
                qaqh_skills::DiagnosticSeverity::Error => "error",
            },
            source: diagnostic.path.clone(),
            message: diagnostic.message.clone(),
        })
        .collect();
    Ok(SkillListOutput {
        skills,
        diagnostics,
    })
}

fn read_resource(
    ctx: &ToolCallContext,
    name: &str,
    path: &str,
) -> Result<SkillResourceOutput, ToolExecutionError> {
    if name.is_empty() || path.is_empty() {
        return Err(recoverable(
            legacy_error(
                ToolErrorKind::InvalidArguments,
                "missing_argument",
                "skill resource requires name and path",
            )
            .with_hint("Use an exact skill name and a relative path from its resource manifest."),
        ));
    }
    let resource = qaqh_skills::read_resource(&ctx.workspace_root, name, Path::new(path)).map_err(
        |error| {
            recoverable(
                legacy_error(
                    ToolErrorKind::Unavailable,
                    "skill_resource_unavailable",
                    error,
                )
                .with_hint("Use a relative file path listed by the activated skill."),
            )
        },
    )?;
    Ok(SkillResourceOutput(resource.content))
}

fn recoverable(error: ToolError) -> ToolExecutionError {
    ToolExecutionError::Recoverable(error)
}

fn legacy_error(kind: ToolErrorKind, code: &str, detail: impl Into<String>) -> ToolError {
    let mut error = ToolError::new(kind, detail);
    error.code = ToolErrorCode::parse_or_builtin(code, kind);
    error
}

pub fn register(mgr: &mut crate::ToolManager) {
    // 旧 `skills` 记录的重建兜底：typed 工具始终自带 display，历史会话
    // （messages.jsonl 中的 "skills" 名字）经此投影。
    mgr.register_display("skills", crate::display::project_skills);
    mgr.register_typed(SkillActivateTool);
    mgr.register_typed(SkillListTool);
    mgr.register_typed(SkillResourceTool);
}
