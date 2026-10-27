//! Build an SDK-independent plan from the currently registered, filtered tools.
//!
//! Not wired to model requests or dispatch. The caller must provide the final
//! profile/execution-capability-filtered canonical names. The manager applies
//! its allowlist and exposure filter again; an empty list means no tools.

use std::collections::{HashMap, HashSet};
use std::fmt;

use qaqh_tool_core::model_tool_plan::{
    InputAdapter, ModelToolAddress, ModelToolDeclaration, ModelToolEntry, ModelToolKind,
    ModelToolPlan, ToolPlanError,
};

pub use qaqh_tool_core::model_tool_plan::CustomFormat;

/// Complete Codex `apply_patch` grammar, vendored with source and license
/// attribution in `assets/apply_patch.lark`.
pub const CODEX_APPLY_PATCH_LARK: &str = include_str!("../assets/apply_patch.lark");

use crate::ToolManager;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolLayout {
    Flat,
    Namespace { name: String, description: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchInput {
    FunctionJson,
    Custom(CustomFormat),
}

/// Explicit profile snapshot; no provider/model names are inferred here.
#[derive(Debug, Clone)]
pub struct ToolPlanProfile {
    pub version: String,
    pub profile: String,
    pub layout: ToolLayout,
    pub patch_input: PatchInput,
    /// Explicit, reversible wire aliases for otherwise ordinary functions.
    pub function_aliases: HashMap<String, String>,
}

/// Immutable snapshot of the plan and the namespace description needed to
/// project its declarations. This wrapper is not an authorization proof.
#[derive(Debug, Clone)]
pub struct WorkspaceToolPlan {
    plan: ModelToolPlan,
    layout: ToolLayout,
}

impl WorkspaceToolPlan {
    pub fn layout(&self) -> &ToolLayout {
        &self.layout
    }
}

impl std::ops::Deref for WorkspaceToolPlan {
    type Target = ModelToolPlan;

    fn deref(&self) -> &Self::Target {
        &self.plan
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildToolPlanError {
    InvalidNamespace,
    InvalidAllowlist,
    UnknownSelectedTool(String),
    DuplicateSelectedTool(String),
    InvalidAlias(String),
    UnsupportedRepresentation(String),
    PreviewConfirmationUnavailable,
    Plan(ToolPlanError),
}

impl fmt::Display for BuildToolPlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidNamespace => f.write_str("invalid namespace"),
            Self::InvalidAllowlist => f.write_str("non-empty allowlist has no registered tools"),
            Self::UnknownSelectedTool(name) => write!(f, "selected tool is not visible: {name}"),
            Self::DuplicateSelectedTool(name) => write!(f, "duplicate selected tool: {name}"),
            Self::InvalidAlias(name) => write!(f, "invalid function alias for {name}"),
            Self::UnsupportedRepresentation(name) => {
                write!(f, "native representation unavailable: {name}")
            }
            Self::PreviewConfirmationUnavailable => {
                f.write_str("native patch preview requires confirm_apply in the selected tools")
            }
            Self::Plan(error) => write!(f, "{error}"),
        }
    }
}
impl std::error::Error for BuildToolPlanError {}

impl From<ToolPlanError> for BuildToolPlanError {
    fn from(error: ToolPlanError) -> Self {
        Self::Plan(error)
    }
}

impl ToolManager {
    /// Construct from explicit final capability-filtered canonical names. This
    /// does not register preview, authorize calls, send requests, or dispatch.
    pub fn build_model_tool_plan(
        &self,
        profile: &ToolPlanProfile,
        selected_names: &[String],
    ) -> Result<WorkspaceToolPlan, BuildToolPlanError> {
        let namespace = match &profile.layout {
            ToolLayout::Flat => None,
            ToolLayout::Namespace { name, description }
                if !name.trim().is_empty() && !description.trim().is_empty() =>
            {
                Some(name.clone())
            }
            ToolLayout::Namespace { .. } => return Err(BuildToolPlanError::InvalidNamespace),
        };
        // Legacy set_allowed falls back to all for an all-unknown nonempty
        // allowlist. Such a fallback cannot safely create a new native plan.
        if self.model_plan_has_invalid_allowlist() {
            return Err(BuildToolPlanError::InvalidAllowlist);
        }
        let visible: HashSet<String> = self
            .filtered_defs()
            .into_iter()
            .map(|def| def.function.name)
            .filter(|name| {
                self.lookup(name).is_some_and(|descriptor| {
                    descriptor.exposure == crate::tool_api::ToolExposure::Direct
                })
            })
            .collect();
        let mut seen = HashSet::new();
        for name in selected_names {
            if !seen.insert(name.as_str()) {
                return Err(BuildToolPlanError::DuplicateSelectedTool(name.clone()));
            }
            if !visible.contains(name) {
                return Err(BuildToolPlanError::UnknownSelectedTool(name.clone()));
            }
        }
        for (name, alias) in &profile.function_aliases {
            if !visible.contains(name) || alias.trim().is_empty() {
                return Err(BuildToolPlanError::InvalidAlias(name.clone()));
            }
        }
        let mut entries = Vec::new();
        for name in selected_names {
            let descriptor = self
                .lookup(name)
                .ok_or_else(|| BuildToolPlanError::UnknownSelectedTool(name.clone()))?;
            if let Some(native) = &descriptor.native_patch
                && let PatchInput::Custom(format) = &profile.patch_input
            {
                if !seen.contains("confirm_apply") {
                    return Err(BuildToolPlanError::PreviewConfirmationUnavailable);
                }
                if profile.function_aliases.contains_key(name) {
                    return Err(BuildToolPlanError::InvalidAlias(name.clone()));
                }
                entries.push(ModelToolEntry {
                    address: ModelToolAddress {
                        kind: ModelToolKind::Custom,
                        namespace: namespace.clone(),
                        name: name.clone(),
                    },
                    declaration: ModelToolDeclaration::Custom {
                        description: native.custom_description.clone(),
                        format: format.clone(),
                    },
                    internal_tool_key: name.clone(),
                    adapter: InputAdapter::ApplyPatchCustom,
                });
                entries.push(ModelToolEntry {
                    address: ModelToolAddress {
                        kind: ModelToolKind::Function,
                        namespace: namespace.clone(),
                        name: "apply_patch_preview".into(),
                    },
                    declaration: ModelToolDeclaration::Function {
                        description: native.preview_description.clone(),
                        parameters: native.preview_parameters.clone(),
                        strict: false,
                    },
                    internal_tool_key: name.clone(),
                    adapter: InputAdapter::ApplyPatchPreview,
                });
            } else {
                // A registration named apply_patch must explicitly declare its
                // native capability; never treat a dynamic substitute as builtin.
                if name == "apply_patch" && matches!(profile.patch_input, PatchInput::Custom(_)) {
                    return Err(BuildToolPlanError::UnsupportedRepresentation(name.clone()));
                }
                entries.push(ModelToolEntry {
                    address: ModelToolAddress {
                        kind: ModelToolKind::Function,
                        namespace: namespace.clone(),
                        name: profile
                            .function_aliases
                            .get(name)
                            .cloned()
                            .unwrap_or_else(|| name.clone()),
                    },
                    declaration: ModelToolDeclaration::Function {
                        description: descriptor.description.clone(),
                        parameters: descriptor.input_schema.clone(),
                        strict: false,
                    },
                    internal_tool_key: name.clone(),
                    adapter: InputAdapter::FunctionJson,
                });
            }
        }
        let plan = ModelToolPlan::new(&profile.version, &profile.profile, entries)?;
        Ok(WorkspaceToolPlan {
            plan,
            layout: profile.layout.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_tool_core::model_tool_plan::{
        NativeToolCall, NativeToolCompletion, NativeToolPayload,
    };
    use serde_json::json;

    fn profile(layout: ToolLayout, patch_input: PatchInput) -> ToolPlanProfile {
        ToolPlanProfile {
            version: "test-v1".into(),
            profile: "test".into(),
            layout,
            patch_input,
            function_aliases: HashMap::new(),
        }
    }
    fn native_profile() -> ToolPlanProfile {
        profile(
            ToolLayout::Namespace {
                name: "functions".into(),
                description: "Workspace tools".into(),
            },
            PatchInput::Custom(CustomFormat::Text),
        )
    }
    fn names(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| (*v).to_owned()).collect()
    }
    fn manager() -> ToolManager {
        crate::registration::build_tool_manager(&[])
    }

    #[test]
    fn vendored_codex_grammar_is_present_and_attributed() {
        assert!(CODEX_APPLY_PATCH_LARK.contains("start: begin_patch"));
        assert!(CODEX_APPLY_PATCH_LARK.contains("Apache-2.0"));
        assert!(CODEX_APPLY_PATCH_LARK.contains("970111f6cb62cc02fe36a82ea7d71b37a2889c97"));
    }

    #[test]
    fn registered_patch_generates_namespace_custom_and_preview_with_exact_internal_mapping() {
        let mgr = manager();
        let original = mgr.filtered_defs();
        let plan = mgr
            .build_model_tool_plan(
                &native_profile(),
                &names(&["read", "apply_patch", "confirm_apply"]),
            )
            .expect("plan");
        assert_eq!(plan.entries().len(), 4);
        assert_eq!(mgr.filtered_defs().len(), original.len());
        let patch = plan
            .entries()
            .iter()
            .find(|e| e.address.name == "apply_patch")
            .expect("patch");
        assert_eq!(
            plan.layout(),
            &ToolLayout::Namespace {
                name: "functions".into(),
                description: "Workspace tools".into()
            }
        );
        assert_eq!(patch.address.kind, ModelToolKind::Custom);
        assert_eq!(patch.address.namespace.as_deref(), Some("functions"));
        assert_eq!(patch.internal_tool_key, "apply_patch");
        let preview = plan
            .entries()
            .iter()
            .find(|e| e.address.name == "apply_patch_preview")
            .expect("preview");
        assert_eq!(preview.address.kind, ModelToolKind::Function);
        assert_eq!(preview.adapter, InputAdapter::ApplyPatchPreview);
        let ModelToolDeclaration::Function { parameters, .. } = &preview.declaration else {
            panic!("preview must be function")
        };
        assert_eq!(parameters["required"], json!(["patch"]));
        assert_eq!(parameters["additionalProperties"], false);
        assert!(parameters["properties"].get("dry_run").is_none());
        let call = NativeToolCall {
            address: preview.address.clone(),
            call_id: "call_preview".into(),
            item_id: Some("fc_item".into()),
            payload: NativeToolPayload::JsonArguments(
                json!({"patch":"*** Begin Patch\n*** End Patch\n"}),
            ),
            completion: NativeToolCompletion::Completed,
            provider_state: None,
        };
        let result = plan.adapt(call).expect("canonical preview");
        assert_eq!(result.canonical.internal_tool_key, "apply_patch");
        assert_eq!(result.canonical.args["dry_run"], true);
        assert!(
            mgr.lookup("apply_patch_preview").is_none(),
            "preview is not registered as an executor"
        );
    }

    #[test]
    fn legacy_profile_keeps_function_schema_and_no_preview() {
        let mgr = manager();
        let plan = mgr
            .build_model_tool_plan(
                &profile(ToolLayout::Flat, PatchInput::FunctionJson),
                &names(&["apply_patch"]),
            )
            .expect("plan");
        assert_eq!(plan.entries().len(), 1);
        let patch = &plan.entries()[0];
        assert_eq!(patch.address.kind, ModelToolKind::Function);
        assert_eq!(patch.address.namespace, None);
        assert_eq!(patch.adapter, InputAdapter::FunctionJson);
        let ModelToolDeclaration::Function { parameters, .. } = &patch.declaration else {
            panic!("function")
        };
        assert!(parameters["properties"].get("dry_run").is_some());
    }

    #[test]
    fn flat_custom_keeps_functions_flat_and_function_alias_reversible() {
        let mgr = manager();
        let mut p = profile(ToolLayout::Flat, PatchInput::Custom(CustomFormat::Text));
        p.function_aliases.insert("read".into(), "read_file".into());
        let plan = mgr
            .build_model_tool_plan(&p, &names(&["read", "apply_patch", "confirm_apply"]))
            .expect("plan");
        assert!(
            plan.entries()
                .iter()
                .all(|entry| entry.address.namespace.is_none())
        );
        let read = plan
            .entries()
            .iter()
            .find(|entry| entry.address.name == "read_file")
            .expect("alias");
        assert_eq!(read.internal_tool_key, "read");
        let call = NativeToolCall {
            address: read.address.clone(),
            call_id: "call_read".into(),
            item_id: None,
            payload: NativeToolPayload::JsonArguments(json!({"path":"a.txt"})),
            completion: NativeToolCompletion::Completed,
            provider_state: None,
        };
        assert_eq!(
            plan.adapt(call).expect("adapt").canonical.internal_tool_key,
            "read"
        );
        assert_eq!(
            plan.entries()
                .iter()
                .filter(|entry| entry.address.kind == ModelToolKind::Custom)
                .count(),
            1
        );
    }

    #[test]
    fn filtered_tools_never_widen_and_empty_selection_stays_empty() {
        let mut mgr = manager();
        mgr.set_allowed(names(&["read", "confirm_apply"]));
        let p = native_profile();
        assert!(matches!(
            mgr.build_model_tool_plan(&p, &names(&["apply_patch"])),
            Err(BuildToolPlanError::UnknownSelectedTool(_))
        ));
        let plan = mgr
            .build_model_tool_plan(&p, &names(&["read"]))
            .expect("read plan");
        assert_eq!(plan.entries().len(), 1);
        assert_eq!(plan.entries()[0].internal_tool_key, "read");
        assert!(
            mgr.build_model_tool_plan(&p, &[])
                .expect("empty plan")
                .entries()
                .is_empty()
        );
        mgr.set_allowed(names(&["removed_or_unknown"]));
        assert!(matches!(
            mgr.build_model_tool_plan(&p, &names(&["read"])),
            Err(BuildToolPlanError::InvalidAllowlist)
        ));
    }

    #[test]
    fn patch_requires_visible_confirm_without_expanding_allowlist() {
        let mut mgr = manager();
        mgr.set_allowed(names(&["apply_patch"]));
        assert!(matches!(
            mgr.build_model_tool_plan(&native_profile(), &names(&["apply_patch"])),
            Err(BuildToolPlanError::PreviewConfirmationUnavailable)
        ));
        assert!(matches!(
            mgr.build_model_tool_plan(&native_profile(), &names(&["apply_patch", "confirm_apply"])),
            Err(BuildToolPlanError::UnknownSelectedTool(_))
        ));
    }

    #[test]
    fn aliases_collisions_and_duplicates_are_rejected() {
        let mgr = manager();
        let mut p = native_profile();
        p.function_aliases
            .insert("read".into(), "apply_patch_preview".into());
        assert!(matches!(
            mgr.build_model_tool_plan(&p, &names(&["read", "apply_patch", "confirm_apply"])),
            Err(BuildToolPlanError::Plan(ToolPlanError::DeclarationConflict))
        ));
        assert!(matches!(
            mgr.build_model_tool_plan(&native_profile(), &names(&["read", "read"])),
            Err(BuildToolPlanError::DuplicateSelectedTool(_))
        ));
        let mut invalid = native_profile();
        invalid.function_aliases.insert("read".into(), "  ".into());
        assert!(matches!(
            mgr.build_model_tool_plan(&invalid, &names(&["read"])),
            Err(BuildToolPlanError::InvalidAlias(_))
        ));
    }

    #[test]
    fn minimal_profile_and_dynamic_tools_preserve_internal_identity() {
        fn dispatch(
            _: &str,
            _: &crate::tool_api::ToolCallContext,
            _: serde_json::Value,
        ) -> Result<crate::tool_api::ToolOutcome, crate::tool_api::FatalToolError> {
            Ok(crate::tool_api::map_tool_result(crate::ToolResult::ok(
                "fixture",
            )))
        }
        let mut mgr = manager();
        let (name, tool) = crate::build_dynamic_tool(
            "demo",
            "read",
            "read fixture",
            json!({"type":"object"}),
            dispatch,
            crate::ToolCategory::Read,
            std::time::Duration::from_secs(10),
        );
        mgr.register_dynamic(name.clone(), tool).expect("register");
        mgr.set_allowed(names(&["read", "mcp__demo__read"]));
        let p = native_profile();
        let plan = mgr
            .build_model_tool_plan(&p, &names(&["read", "mcp__demo__read"]))
            .expect("plan");
        let dynamic = plan
            .entries()
            .iter()
            .find(|e| e.internal_tool_key == name)
            .expect("dynamic");
        assert_eq!(dynamic.address.name, name);
        assert_eq!(dynamic.address.namespace.as_deref(), Some("functions"));
        assert!(
            plan.entries()
                .iter()
                .all(|e| e.address.name != "apply_patch_preview")
        );
        mgr.set_allowed(
            qaqh_types::MINIMAL_TOOLS
                .iter()
                .map(|v| (*v).to_owned())
                .collect(),
        );
        assert!(matches!(
            mgr.build_model_tool_plan(&p, &names(&["apply_patch"])),
            Err(BuildToolPlanError::UnknownSelectedTool(_))
        ));
    }

    #[test]
    fn snapshot_is_immutable_across_allowlist_and_layout_changes() {
        let mut mgr = manager();
        mgr.set_allowed(names(&["apply_patch", "confirm_apply"]));
        let mut profile = native_profile();
        let plan = mgr
            .build_model_tool_plan(&profile, &names(&["apply_patch", "confirm_apply"]))
            .expect("plan");
        mgr.set_allowed(names(&["read"]));
        profile.layout = ToolLayout::Flat;
        assert_eq!(plan.entries().len(), 3);
        assert!(
            plan.entries()
                .iter()
                .all(|entry| entry.address.namespace.as_deref() == Some("functions"))
        );
        assert!(matches!(plan.layout(), ToolLayout::Namespace { name, .. } if name == "functions"));
        assert!(matches!(
            mgr.build_model_tool_plan(&profile, &names(&["apply_patch"])),
            Err(BuildToolPlanError::UnknownSelectedTool(_))
        ));
    }

    #[test]
    fn native_capability_is_not_inferred_from_name_alone() {
        fn dispatch(
            _: &crate::tool_api::ToolCallContext,
            _: serde_json::Value,
        ) -> crate::ToolResult {
            crate::ToolResult::ok("fixture")
        }
        let mut mgr = ToolManager::new();
        mgr.register_probe(crate::probe::ProbeTool {
            key: "apply_patch".into(),
            description: "fixture",
            input_schema: json!({"type": "object"}),
            handler: dispatch,
            risk: crate::ToolRisk::Write,
            category: crate::ToolCategory::Write,
            default_timeout: std::time::Duration::from_secs(5),
        });
        assert!(matches!(
            mgr.build_model_tool_plan(&native_profile(), &names(&["apply_patch"])),
            Err(BuildToolPlanError::UnsupportedRepresentation(_))
        ));
    }
}
