//! Agent Skill activation tool.
//!
//! The public `skills` action surface remains `activate/list/resource/validate`.
//! The implementation is a typed tool so model/display/service projections
//! derive from one output value and skill activation travels as a trusted
//! typed effect rather than being parsed back out of tool text.

#![allow(clippy::result_large_err)] // TypedTool's frozen public error boundary.

use std::path::{Path, PathBuf};
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::tool_api::{
    OutputBudget, ToolCallContext, ToolContentBlock, ToolDescriptor, ToolDisplay, ToolError,
    ToolErrorCode, ToolErrorKind, ToolExecutionError, ToolExposure, ToolName, ToolProjection,
    ToolSource, TypedTool,
};
use crate::{ToolEffect, ToolRisk};

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SkillsArgs {
    action: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    path: Option<String>,
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
pub struct SkillsActivateOutput {
    status: &'static str,
    skill: String,
    resources: Vec<String>,
    content: String,
    #[serde(skip)]
    #[schemars(skip)]
    activation: qaqh_skills::SkillActivation,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SkillsListOutput {
    skills: Vec<SkillListEntry>,
    diagnostics: Vec<SkillDiagnosticEntry>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(transparent)]
pub struct SkillsResourceOutput(String);

#[derive(Debug, Serialize, JsonSchema)]
pub struct SkillsValidateOutput {
    name: String,
    source: PathBuf,
    valid: bool,
    errors: Vec<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum SkillsOutput {
    Activate(Box<SkillsActivateOutput>),
    List(SkillsListOutput),
    Resource(SkillsResourceOutput),
    Validate(SkillsValidateOutput),
}

impl ToolProjection for SkillsOutput {
    fn model_blocks(&self) -> Vec<ToolContentBlock> {
        vec![ToolContentBlock::Text {
            text: model_text(self),
        }]
    }

    fn summary(&self) -> Option<String> {
        match self {
            Self::Activate(output) => Some(output.content.clone()),
            Self::List(output) => Some(format!(
                "{} skill(s), {} diagnostic(s)",
                output.skills.len(),
                output.diagnostics.len()
            )),
            Self::Resource(output) => output.0.lines().next().map(str::to_string),
            Self::Validate(output) if output.valid => {
                Some(format!("skill '{}' is valid", output.name))
            }
            Self::Validate(output) => Some(format!(
                "skill '{}' has {} validation error(s)",
                output.name,
                output.errors.len()
            )),
        }
    }

    fn display(&self, args: &serde_json::Value) -> ToolDisplay {
        crate::display::project_skills(args, &model_text(self))
    }

    fn effects(&self) -> Vec<ToolEffect> {
        match self {
            Self::Activate(output) => vec![ToolEffect::Skill(qaqh_skills::SkillEffect::Activate(
                output.activation.clone(),
            ))],
            _ => Vec::new(),
        }
    }
}

fn model_text(output: &SkillsOutput) -> String {
    match output {
        SkillsOutput::Resource(resource) => resource.0.clone(),
        _ => serde_json::to_string(output).unwrap_or_default(),
    }
}

pub struct SkillsTool;

impl TypedTool for SkillsTool {
    type Args = SkillsArgs;
    type Output = SkillsOutput;

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: ToolName::new("skills").expect("valid skills tool name"),
            display_name: None,
            description: "Skills: activate/list/resource/validate. activate injects envelope as trailing system message.".into(),
            input_schema: skills_schema(),
            output_schema: serde_json::to_value(schemars::schema_for!(SkillsOutput))
                .expect("skills output schema"),
            category: crate::permission::ToolCategory::Read,
            risk: ToolRisk::ReadOnly,
            default_timeout: Duration::from_secs(15),
            exposure: ToolExposure::Direct,
            source: ToolSource::Builtin,
            output_budget: OutputBudget::default(),
            capabilities: crate::tool_capabilities::builtin_capabilities("skills")
                .unwrap_or_default(),
        }
    }

    fn run(
        &self,
        ctx: &ToolCallContext,
        args: SkillsArgs,
    ) -> Result<Self::Output, ToolExecutionError> {
        let name = args.name.as_deref();
        let path = args.path.as_deref();
        match args.action.as_str() {
            "activate" if name.is_some() && path.is_none() => {
                activate_skill(ctx, name.expect("checked above"))
            }
            "list" if name.is_none() && path.is_none() => list_skills(ctx),
            "resource" if name.is_some() && path.is_some() => read_resource(
                ctx,
                name.expect("checked above"),
                path.expect("checked above"),
            ),
            "validate" if name.is_some() && path.is_none() => {
                validate_skill(ctx, name.expect("checked above"))
            }
            "activate" | "list" | "resource" | "validate" => Err(recoverable(legacy_error(
                ToolErrorKind::InvalidArguments,
                "invalid_arguments",
                "arguments do not match the selected skills action",
            )
            .with_hint(
                "activate and validate require name; list accepts only action; resource requires name and path.",
            ))),
            _ => Err(recoverable(legacy_error(
                ToolErrorKind::InvalidArguments,
                "invalid_action",
                "skills action must be activate, list, resource, or validate",
            )
            .with_hint("Choose the action matching the required skill operation."))),
        }
    }
}

fn activate_skill(ctx: &ToolCallContext, name: &str) -> Result<SkillsOutput, ToolExecutionError> {
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
        "[OK] skill '{skill}' activated. The full instructions are injected as the trailing <skill_context_envelope> system message (authoritative — it replaces all older skill instructions). If the envelope is not visible, call resource to read bundled files on demand."
    );
    Ok(SkillsOutput::Activate(Box::new(SkillsActivateOutput {
        status: "ok",
        skill,
        resources,
        content,
        activation,
    })))
}

fn list_skills(ctx: &ToolCallContext) -> Result<SkillsOutput, ToolExecutionError> {
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
    Ok(SkillsOutput::List(SkillsListOutput {
        skills,
        diagnostics,
    }))
}

fn read_resource(
    ctx: &ToolCallContext,
    name: &str,
    path: &str,
) -> Result<SkillsOutput, ToolExecutionError> {
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
    Ok(SkillsOutput::Resource(SkillsResourceOutput(
        resource.content,
    )))
}

fn validate_skill(ctx: &ToolCallContext, name: &str) -> Result<SkillsOutput, ToolExecutionError> {
    if name.is_empty() {
        return Err(recoverable(
            legacy_error(
                ToolErrorKind::InvalidArguments,
                "missing_name",
                "skill name is required",
            )
            .with_hint("Use an exact name from the current skill catalog."),
        ));
    }
    let catalog = qaqh_skills::discover(&ctx.workspace_root);
    let Some(skill) = catalog.skills.iter().find(|skill| skill.name == name) else {
        return Err(recoverable(
            legacy_error(
                ToolErrorKind::NotFound,
                "skill_not_available",
                format!("unknown skill '{name}'"),
            )
            .with_hint("Use an exact name from the current skill catalog."),
        ));
    };
    let diagnostics = qaqh_skills::validate_file(&skill.path);
    let errors = diagnostics
        .iter()
        .map(|diagnostic| diagnostic.message.clone())
        .collect::<Vec<_>>();
    Ok(SkillsOutput::Validate(SkillsValidateOutput {
        name: name.to_owned(),
        source: skill.path.clone(),
        valid: errors.is_empty(),
        errors,
    }))
}

fn recoverable(error: ToolError) -> ToolExecutionError {
    ToolExecutionError::Recoverable(error)
}

fn legacy_error(kind: ToolErrorKind, code: &str, detail: impl Into<String>) -> ToolError {
    let mut error = ToolError::new(kind, detail);
    error.code = ToolErrorCode::parse_or_builtin(code, kind);
    error
}

fn skills_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "enum": ["activate", "list", "resource", "validate"],
                "description": "Action"
            },
            "name": {
                "type": "string",
                "description": "Skill name"
            },
            "path": {
                "type": "string",
                "description": "Resource path (for resource)"
            }
        },
        "required": ["action"],
        "additionalProperties": false,
        "oneOf": [
            {
                "title": "Activate a skill",
                "properties": {"action": {"const": "activate"}},
                "required": ["action", "name"]
            },
            {
                "title": "List effective skills",
                "properties": {"action": {"const": "list"}},
                "required": ["action"],
                "not": {"anyOf": [{"required": ["name"]}, {"required": ["path"]}]}
            },
            {
                "title": "Read a skill resource",
                "properties": {"action": {"const": "resource"}},
                "required": ["action", "name", "path"]
            },
            {
                "title": "Validate a skill",
                "properties": {"action": {"const": "validate"}},
                "required": ["action", "name"]
            }
        ]
    })
}

pub fn register(mgr: &mut crate::ToolManager) {
    mgr.register_display("skills", crate::display::project_skills);
    mgr.register_typed(SkillsTool);
}
