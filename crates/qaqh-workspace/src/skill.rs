//! Agent Skill 工具（单工具）：`skill_list`。
//!
//! 无激活语义：不带 `name` 列技能目录（仅元数据）；带 `name` 列该技能的
//! 内部资源——磁盘技能给绝对路径，模型用标准 `read` 直读；bundled 嵌入
//! 技能内容随响应直接返回（read 够不到二进制内嵌文件）。

#![allow(clippy::result_large_err)] // TypedTool's frozen public error boundary.

use std::path::PathBuf;
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::tool_api::{
    ToolBody, ToolCallContext, ToolContentBlock, ToolDisplay, ToolError, ToolErrorKind,
    ToolErrorCode, ToolExecutionError, ToolHeader, ToolMeta, ToolProjection, TypedTool,
};
use crate::{permission::ToolCategory, ToolRisk};

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
/// List skills, or list one skill's internal files.
pub struct SkillListArgs {
    /// Skill name (exact match from the current catalog). Omit to list the whole catalog.
    pub name: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SkillListEntry {
    name: String,
    description: String,
    scope: &'static str,
    origin: &'static str,
    source: PathBuf,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SkillDiagnosticEntry {
    severity: &'static str,
    source: PathBuf,
    message: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SkillResourceEntry {
    relative_path: String,
    /// Disk skill: absolute path for the read tool.
    #[serde(skip_serializing_if = "Option::is_none")]
    absolute_path: Option<String>,
    /// Bundled skill: content returned inline (not reachable from disk).
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
/// Catalog (no `name`) or one skill's resource listing (`name`).
pub struct SkillListOutput {
    #[serde(skip_serializing_if = "Option::is_none")]
    skills: Option<Vec<SkillListEntry>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    diagnostics: Option<Vec<SkillDiagnosticEntry>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    skill: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    resources: Option<Vec<SkillResourceEntry>>,
}

impl ToolProjection for SkillListOutput {
    fn model_blocks(&self) -> Vec<ToolContentBlock> {
        vec![ToolContentBlock::Text {
            text: serde_json::to_string(self).unwrap_or_default(),
        }]
    }

    fn display(&self, _args: &serde_json::Value) -> ToolDisplay {
        let summary = if let Some(resources) = &self.resources {
            format!(
                "{} resources of {}",
                resources.len(),
                self.skill.as_deref().unwrap_or("?")
            )
        } else {
            format!(
                "listed {} skills · {} diagnostics",
                self.skills.as_ref().map_or(0, Vec::len),
                self.diagnostics.as_ref().map_or(0, Vec::len)
            )
        };
        ToolDisplay::new(
            ToolHeader::Other {
                label: "skills list".into(),
            },
            ToolBody::None,
        )
        .with_summary(summary)
    }
}

pub struct SkillListTool;

impl TypedTool for SkillListTool {
    type Args = SkillListArgs;
    type Output = SkillListOutput;

    fn meta(&self) -> ToolMeta {
        ToolMeta::new(
            qaqh_skills::SKILL_LIST_TOOL,
            "List skills, or list one skill's internal files. Without `name`: the skill catalog (names + descriptions). With `name`: that skill's internal resources — disk skills return absolute paths to read with the read tool, bundled skills return content inline.",
            ToolCategory::Read,
            ToolRisk::ReadOnly,
            Duration::from_secs(15),
        )
    }

    fn run(
        &self,
        ctx: &ToolCallContext,
        args: SkillListArgs,
    ) -> Result<Self::Output, ToolExecutionError> {
        match args.name.as_deref() {
            None => Ok(list_catalog(&ctx.workspace_root)),
            Some(name) => list_resources(&ctx.workspace_root, name),
        }
    }
}

fn recoverable(error: ToolError) -> ToolExecutionError {
    ToolExecutionError::Recoverable(error)
}

fn legacy_error(kind: ToolErrorKind, code: &str, detail: impl Into<String>) -> ToolError {
    let mut error = ToolError::new(kind, detail);
    error.code = ToolErrorCode::parse_or_builtin(code, kind);
    error
}

fn list_catalog(workspace_root: &std::path::Path) -> SkillListOutput {
    let catalog = qaqh_skills::discover(workspace_root);
    SkillListOutput {
        skills: Some(
            catalog
                .skills
                .iter()
                .map(|skill| SkillListEntry {
                    name: skill.name.clone(),
                    description: skill.description.clone(),
                    scope: match skill.scope {
                        qaqh_skills::SkillScope::Project => "project",
                        qaqh_skills::SkillScope::User => "user",
                    },
                    origin: match skill.origin {
                        qaqh_skills::SkillOrigin::Project => "project",
                        qaqh_skills::SkillOrigin::User => "user",
                        qaqh_skills::SkillOrigin::Bundled => "bundled",
                    },
                    source: skill.path.clone(),
                })
                .collect(),
        ),
        diagnostics: Some(
            catalog
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
                .collect(),
        ),
        skill: None,
        resources: None,
    }
}

fn list_resources(workspace_root: &std::path::Path, name: &str) -> Result<SkillListOutput, ToolExecutionError> {
    let resources = qaqh_skills::skill_resources(workspace_root, name).map_err(|error| {
        recoverable(
            legacy_error(ToolErrorKind::NotFound, "skill_not_available", error)
                .with_hint("Use an exact name from the skill catalog (call skill_list without arguments)."),
        )
    })?;
    Ok(SkillListOutput {
        skills: None,
        diagnostics: None,
        skill: Some(name.to_string()),
        resources: Some(
            resources
                .into_iter()
                .map(|resource| {
                    let (absolute_path, content) = match resource.source {
                        qaqh_skills::SkillResourceSource::Disk { absolute_path } => {
                            (Some(absolute_path.to_string_lossy().into_owned()), None)
                        }
                        qaqh_skills::SkillResourceSource::Embedded { content } => (None, Some(content)),
                    };
                    SkillResourceEntry {
                        relative_path: resource.relative_path,
                        absolute_path,
                        content,
                    }
                })
                .collect(),
        ),
    })
}

pub fn register(mgr: &mut crate::ToolManager) {
    mgr.register_typed(SkillListTool);
}
