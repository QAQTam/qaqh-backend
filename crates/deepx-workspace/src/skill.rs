//! Agent Skill 激活工具。
//!
//! 与通用 `read` 不同，本工具按发现目录中的 skill 名解析并返回完整正文，
//! 不执行 200 行截断，也不允许模型传入任意文件路径。

use std::path::Path;

use crate::{JsonArgs, ToolHandler, ToolResult, ToolRisk};

fn current_workspace() -> String {
    crate::CURRENT_WORKSPACE
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .clone()
}

pub(crate) fn load_activation(
    args: &serde_json::Value,
) -> Result<deepx_skills::SkillActivation, String> {
    let name = args.s("name");
    if name.is_empty() {
        return Err("skill name is required".into());
    }
    let workspace = current_workspace();
    deepx_skills::load_named(Path::new(&workspace), &name)
}

fn load_skill_resource(
    args: &serde_json::Value,
) -> Result<String, (&'static str, String, &'static str)> {
    let name = args.s("name");
    let path = args.s("path");
    if name.is_empty() || path.is_empty() {
        return Err((
            "MISSING_ARGUMENT",
            "skill resource requires name and path".into(),
            "Use an exact skill name and a relative path from its resource manifest.",
        ));
    }
    let workspace = current_workspace();
    match deepx_skills::read_resource(Path::new(&workspace), &name, Path::new(&path)) {
        Ok(resource) => Ok(resource.content),
        Err(error) => Err((
            "SKILL_RESOURCE_UNAVAILABLE",
            error,
            "Use a relative file path listed by the activated skill.",
        )),
    }
}

fn handle_skill(ctx: crate::ToolCallCtx) -> ToolResult {
    match load_activation(&ctx.args) {
        Ok(activation) => {
            let name = activation.metadata.name.clone();
            ctx.push_skill_effect(deepx_skills::SkillEffect::Activate(activation));
            ToolResult::ok(serde_json::json!({
                "status": "ok",
                "skill": name,
                "content": format!("[OK] skill activated. The skill instructions are available in the context above. Use them directly.")
            }).to_string())
        }
        Err(error) => ToolResult {
            success: false,
            content: crate::json_err(
                "SKILL_NOT_AVAILABLE",
                error,
                "Use an exact name from the current skill catalog.",
            ),
        },
    }
}

fn handle_skill_resource(ctx: crate::ToolCallCtx) -> ToolResult {
    match load_skill_resource(&ctx.args) {
        Ok(content) => ToolResult::ok(content),
        Err((code, message, hint)) => ToolResult {
            success: false,
            content: crate::json_err(code, message, hint),
        },
    }
}

fn handle_lifecycle(ctx: crate::ToolCallCtx, retain: bool) -> ToolResult {
    let name = ctx.args.s("name");
    if name.is_empty() {
        return ToolResult::error("skill name is required");
    }
    let effect = if retain {
        deepx_skills::SkillEffect::Retain { name: name.clone() }
    } else {
        deepx_skills::SkillEffect::Release { name: name.clone() }
    };
    ctx.push_skill_effect(effect);
    ToolResult::ok(serde_json::json!({"status":"ok", "skill":name}).to_string())
}

fn handle_skills_list(_ctx: crate::ToolCallCtx) -> ToolResult {
    let workspace = current_workspace();
    let catalog = deepx_skills::discover(Path::new(&workspace));
    let skills = catalog
        .skills
        .iter()
        .map(|skill| {
            serde_json::json!({
                "name": skill.name,
                "scope": match skill.scope {
                    deepx_skills::SkillScope::Project => "project",
                    deepx_skills::SkillScope::User => "user",
                },
                "source": skill.path,
            })
        })
        .collect::<Vec<_>>();
    let diagnostics = catalog
        .diagnostics
        .iter()
        .map(|diagnostic| {
            serde_json::json!({
                "severity": match diagnostic.severity {
                    deepx_skills::DiagnosticSeverity::Warning => "warning",
                    deepx_skills::DiagnosticSeverity::Error => "error",
                },
                "source": diagnostic.path,
                "message": diagnostic.message,
            })
        })
        .collect::<Vec<_>>();
    ToolResult::ok(serde_json::json!({"skills": skills, "diagnostics": diagnostics}).to_string())
}

fn handle_skill_validate(ctx: crate::ToolCallCtx) -> ToolResult {
    let name = ctx.args.s("name");
    if name.is_empty() {
        return ToolResult {
            success: false,
            content: crate::json_err(
                "MISSING_NAME",
                "skill name is required",
                "Use an exact name from the skill catalog.",
            ),
        };
    }
    let workspace = current_workspace();
    let catalog = deepx_skills::discover(Path::new(&workspace));
    let Some(skill) = catalog.skills.iter().find(|skill| skill.name == name) else {
        return ToolResult {
            success: false,
            content: crate::json_err(
                "SKILL_NOT_AVAILABLE",
                format!("unknown skill '{name}'"),
                "Use an exact name from the current skill catalog.",
            ),
        };
    };
    let diagnostics = deepx_skills::validate_file(&skill.path);
    let errors = diagnostics
        .iter()
        .map(|diagnostic| diagnostic.message.as_str())
        .collect::<Vec<_>>();
    ToolResult::ok(
        serde_json::json!({
            "name": name,
            "source": skill.path,
            "valid": errors.is_empty(),
            "errors": errors,
        })
        .to_string(),
    )
}

fn handle_skills(ctx: crate::ToolCallCtx) -> ToolResult {
    let action = ctx.args.s("action");
    let has_name = ctx.args.get("name").is_some();
    let has_path = ctx.args.get("path").is_some();
    match action.as_str() {
        "activate" if has_name && !has_path => handle_skill(ctx),
        "retain" if has_name && !has_path => handle_lifecycle(ctx, true),
        "release" if has_name && !has_path => handle_lifecycle(ctx, false),
        "list" if !has_name && !has_path => handle_skills_list(ctx),
        "resource" if has_name && has_path => handle_skill_resource(ctx),
        "validate" if has_name && !has_path => handle_skill_validate(ctx),
        "activate" | "retain" | "release" | "list" | "resource" | "validate" => ToolResult {
            success: false,
            content: crate::json_err(
                "INVALID_ARGUMENTS",
                format!("arguments do not match skills action '{action}'"),
                "activate and validate require name; list accepts only action; resource requires name and path.",
            ),
        },
        _ => ToolResult {
            success: false,
            content: crate::json_err(
                "INVALID_ACTION",
                "skills action must be activate, retain, release, list, resource, or validate",
                "Choose the action matching the required skill operation.",
            ),
        },
    }
}

pub fn register(mgr: &mut crate::ToolManager) {
    mgr.register(ToolHandler {
        key: "skills".to_string(),
        description: "Manage Agent Skills through one fixed typed interface. Use activate before acting when a task matches the catalog; retain or release when review is due; list for effective sources and diagnostics; resource for contained bundled files; validate for portability checks. Skill metadata never bypasses DeepX permissions.",
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["activate", "retain", "release", "list", "resource", "validate"],
                    "description": "activate: load instructions; retain: renew a lease; release: unload; list: inspect catalog diagnostics; resource: read a bundled resource; validate: validate one SKILL.md"
                },
                "name": {
                    "type": "string",
                    "description": "Exact skill name from the injected catalog. Required for activate, resource, and validate; forbidden for list"
                },
                "path": {
                    "type": "string",
                    "description": "Skill-directory-relative resource path from the activation manifest. Required only for resource; absolute paths and parent traversal are rejected"
                }
            },
            "required": ["action"],
            "additionalProperties": false,
            "oneOf": [
                {
                    "title": "Activate a skill",
                    "properties": {"action": {"const": "activate"}},
                    "required": ["action", "name"],
                    "not": {"required": ["path"]}
                },
                {
                    "title": "Retain an active skill",
                    "properties": {"action": {"const": "retain"}},
                    "required": ["action", "name"],
                    "not": {"required": ["path"]}
                },
                {
                    "title": "Release an active skill",
                    "properties": {"action": {"const": "release"}},
                    "required": ["action", "name"],
                    "not": {"required": ["path"]}
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
                    "required": ["action", "name"],
                    "not": {"required": ["path"]}
                }
            ]
        }),
        handler: handle_skills,
        risk: ToolRisk::ReadOnly,
        default_timeout: std::time::Duration::from_secs(15),
    });
}
