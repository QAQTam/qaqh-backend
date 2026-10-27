//! SDK-independent model tool plan and admission-boundary input adaptation.
//!
//! This module is not wired into the gate or runtime. A caller must build the
//! plan only from the already filtered, authorized tool registry and keep it
//! fixed for the entire tool batch. The SDK remains responsible for assembling
//! stream frames, detecting identity/payload conflicts, and proving completion.
//! No provider-native call may bypass the ordinary permission and dispatch path.

use std::collections::{HashMap, HashSet};
use std::fmt;

use serde_json::{Value, json};

/// Provider-visible tool call kind (not a hosted server tool).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModelToolKind {
    Function,
    Custom,
}

/// Exact provider-facing address. A namespace is not a string prefix.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModelToolAddress {
    pub kind: ModelToolKind,
    pub namespace: Option<String>,
    pub name: String,
}

/// Model input format for a custom tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CustomFormat {
    Text,
    Grammar { syntax: String, definition: String },
}

/// Description retained alongside the unique address, for request projection.
#[derive(Debug, Clone, PartialEq)]
pub enum ModelToolDeclaration {
    Function {
        description: String,
        parameters: Value,
        strict: bool,
    },
    Custom {
        description: String,
        format: CustomFormat,
    },
}

/// Explicit, validated conversion into the existing execution contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputAdapter {
    /// An object-valued JSON function call, preserved without parameter repair.
    FunctionJson,
    /// Raw apply_patch input becomes the canonical apply operation.
    ApplyPatchCustom,
    /// The distinct JSON preview tool becomes canonical apply_patch dry-run.
    ApplyPatchPreview,
}

/// A single registry-derived entry, validated and frozen by [`ModelToolPlan`].
#[derive(Debug, Clone, PartialEq)]
pub struct ModelToolEntry {
    pub address: ModelToolAddress,
    pub declaration: ModelToolDeclaration,
    pub internal_tool_key: String,
    pub adapter: InputAdapter,
}

/// Fixed snapshot of the selected model-facing declarations for one batch.
/// Entries and mappings cannot be changed after construction.
#[derive(Debug, Clone)]
pub struct ModelToolPlan {
    version: String,
    profile: String,
    entries: Vec<ModelToolEntry>,
    lookup: HashMap<ModelToolAddress, usize>,
}

/// Native payload; no implicit conversion between JSON and raw text.
#[derive(Debug, Clone, PartialEq)]
pub enum NativeToolPayload {
    JsonArguments(Value),
    RawText(String),
}

/// Stream completion of a single item, distinct from response termination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeToolCompletion {
    Receiving,
    Completed,
    Invalid,
    Interrupted,
}

/// SDK-supplied native facts, kept independently of the canonical call.
#[derive(Debug, Clone, PartialEq)]
pub struct NativeToolCall {
    pub address: ModelToolAddress,
    pub call_id: String,
    pub item_id: Option<String>,
    pub payload: NativeToolPayload,
    pub completion: NativeToolCompletion,
    pub provider_state: Option<Value>,
}

/// Call for authorization, conflict grouping and execution.
#[derive(Debug, Clone, PartialEq)]
pub struct CanonicalToolCall {
    pub call_id: String,
    pub internal_tool_key: String,
    pub args: Value,
}

/// Both views are returned together so the native facts cannot be lost during
/// conversion. Storing/replaying them is a separate, not-yet-wired concern.
#[derive(Debug, Clone, PartialEq)]
pub struct AdaptedToolCall {
    pub native: NativeToolCall,
    pub canonical: CanonicalToolCall,
}

/// Deliberately excludes payload contents from diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolPlanError {
    InvalidPlan,
    DeclarationConflict,
    InvalidDeclaration,
    InvalidAdapter,
    UnknownToolAddress,
    IncompleteCall,
    InvalidCallIdentity,
    InvalidPayload,
}

impl ToolPlanError {
    pub fn code(self) -> &'static str {
        match self {
            Self::InvalidPlan => "invalid_tool_plan",
            Self::DeclarationConflict => "tool_declaration_conflict",
            Self::InvalidDeclaration => "invalid_tool_declaration",
            Self::InvalidAdapter => "invalid_tool_adapter",
            Self::UnknownToolAddress => "unknown_tool_address",
            Self::IncompleteCall => "tool_call_incomplete",
            Self::InvalidCallIdentity => "invalid_tool_call_identity",
            Self::InvalidPayload => "invalid_tool_payload",
        }
    }
}

impl fmt::Display for ToolPlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

impl std::error::Error for ToolPlanError {}

impl ModelToolPlan {
    /// Validate all declarations before any request is made.
    pub fn new(
        version: impl Into<String>,
        profile: impl Into<String>,
        entries: Vec<ModelToolEntry>,
    ) -> Result<Self, ToolPlanError> {
        let version = version.into();
        let profile = profile.into();
        if version.trim().is_empty() || profile.trim().is_empty() {
            return Err(ToolPlanError::InvalidPlan);
        }
        let mut lookup = HashMap::new();
        let mut exposed_names = HashSet::new();
        for (index, entry) in entries.iter().enumerate() {
            validate_entry(entry)?;
            // Kind does not distinguish two names in the same namespace.
            if !exposed_names.insert((&entry.address.namespace, &entry.address.name))
                || lookup.insert(entry.address.clone(), index).is_some()
            {
                return Err(ToolPlanError::DeclarationConflict);
            }
        }
        Ok(Self {
            version,
            profile,
            entries,
            lookup,
        })
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn profile(&self) -> &str {
        &self.profile
    }

    pub fn entries(&self) -> &[ModelToolEntry] {
        &self.entries
    }

    /// Adapt a complete native call before authorization or write-conflict
    /// grouping. Completion is supplied by the SDK's authoritative state
    /// machine; this function never infers it from patch content.
    pub fn adapt(&self, native: NativeToolCall) -> Result<AdaptedToolCall, ToolPlanError> {
        if native.completion != NativeToolCompletion::Completed {
            return Err(ToolPlanError::IncompleteCall);
        }
        if native.call_id.trim().is_empty()
            || native
                .item_id
                .as_ref()
                .is_some_and(|id| id.trim().is_empty())
        {
            return Err(ToolPlanError::InvalidCallIdentity);
        }
        let entry = self
            .lookup
            .get(&native.address)
            .and_then(|index| self.entries.get(*index))
            .ok_or(ToolPlanError::UnknownToolAddress)?;
        let args = match (entry.adapter, &native.payload) {
            (InputAdapter::FunctionJson, NativeToolPayload::JsonArguments(value))
                if value.is_object() =>
            {
                value.clone()
            }
            (InputAdapter::ApplyPatchCustom, NativeToolPayload::RawText(patch)) => {
                json!({"patch": patch, "dry_run": false})
            }
            (InputAdapter::ApplyPatchPreview, NativeToolPayload::JsonArguments(value)) => {
                let object = value.as_object().ok_or(ToolPlanError::InvalidPayload)?;
                if object.len() != 1 {
                    return Err(ToolPlanError::InvalidPayload);
                }
                let patch = object
                    .get("patch")
                    .and_then(Value::as_str)
                    .ok_or(ToolPlanError::InvalidPayload)?;
                json!({"patch": patch, "dry_run": true})
            }
            _ => return Err(ToolPlanError::InvalidPayload),
        };
        Ok(AdaptedToolCall {
            canonical: CanonicalToolCall {
                call_id: native.call_id.clone(),
                internal_tool_key: entry.internal_tool_key.clone(),
                args,
            },
            native,
        })
    }
}

fn validate_entry(entry: &ModelToolEntry) -> Result<(), ToolPlanError> {
    let address = &entry.address;
    if address.name.trim().is_empty()
        || address
            .namespace
            .as_ref()
            .is_some_and(|s| s.trim().is_empty())
        || entry.internal_tool_key.trim().is_empty()
    {
        return Err(ToolPlanError::InvalidDeclaration);
    }
    match (&entry.declaration, address.kind) {
        (
            ModelToolDeclaration::Function {
                description,
                parameters,
                ..
            },
            ModelToolKind::Function,
        ) if !description.trim().is_empty() && parameters.is_object() => {}
        (
            ModelToolDeclaration::Custom {
                description,
                format,
            },
            ModelToolKind::Custom,
        ) if !description.trim().is_empty()
            && match format {
                CustomFormat::Text => true,
                CustomFormat::Grammar { syntax, definition } => {
                    !syntax.trim().is_empty() && !definition.trim().is_empty()
                }
            } => {}
        _ => return Err(ToolPlanError::InvalidDeclaration),
    }
    match (entry.adapter, address.kind) {
        (InputAdapter::FunctionJson, ModelToolKind::Function) => Ok(()),
        (InputAdapter::ApplyPatchCustom, ModelToolKind::Custom)
            if address.name == "apply_patch" && entry.internal_tool_key == "apply_patch" =>
        {
            Ok(())
        }
        (InputAdapter::ApplyPatchPreview, ModelToolKind::Function)
            if address.name == "apply_patch_preview"
                && entry.internal_tool_key == "apply_patch" =>
        {
            Ok(())
        }
        _ => Err(ToolPlanError::InvalidAdapter),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PATCH: &str =
        "*** Begin Patch\n*** Add File: 示例.txt\n+你好 \"x\" \\ path\n+\n*** End Patch\n";

    fn entry(
        kind: ModelToolKind,
        namespace: Option<&str>,
        name: &str,
        internal: &str,
        adapter: InputAdapter,
    ) -> ModelToolEntry {
        ModelToolEntry {
            address: ModelToolAddress {
                kind,
                namespace: namespace.map(str::to_owned),
                name: name.into(),
            },
            declaration: match kind {
                ModelToolKind::Function => ModelToolDeclaration::Function {
                    description: "fixture".into(),
                    parameters: json!({"type": "object"}),
                    strict: false,
                },
                ModelToolKind::Custom => ModelToolDeclaration::Custom {
                    description: "fixture".into(),
                    format: CustomFormat::Text,
                },
            },
            internal_tool_key: internal.into(),
            adapter,
        }
    }

    fn native(entry: &ModelToolEntry, payload: NativeToolPayload) -> NativeToolCall {
        NativeToolCall {
            address: entry.address.clone(),
            call_id: "call_1".into(),
            item_id: Some("ctc_1".into()),
            payload,
            completion: NativeToolCompletion::Completed,
            provider_state: Some(json!({"opaque": true})),
        }
    }

    #[test]
    fn custom_and_preview_adapt_before_dispatch_without_losing_native_facts() {
        let custom = entry(
            ModelToolKind::Custom,
            Some("functions"),
            "apply_patch",
            "apply_patch",
            InputAdapter::ApplyPatchCustom,
        );
        let preview = entry(
            ModelToolKind::Function,
            Some("functions"),
            "apply_patch_preview",
            "apply_patch",
            InputAdapter::ApplyPatchPreview,
        );
        let plan = ModelToolPlan::new("v1", "siwc-opt-in", vec![custom.clone(), preview.clone()])
            .expect("plan");
        let call = native(&custom, NativeToolPayload::RawText(PATCH.into()));
        let result = plan.adapt(call.clone()).expect("custom");
        assert_eq!(result.native, call);
        assert_eq!(
            result.canonical.args,
            json!({"patch": PATCH, "dry_run": false})
        );
        assert_eq!(result.canonical.call_id, "call_1");
        assert_eq!(result.canonical.internal_tool_key, "apply_patch");
        let result = plan
            .adapt(native(
                &preview,
                NativeToolPayload::JsonArguments(json!({"patch": PATCH})),
            ))
            .expect("preview");
        assert_eq!(
            result.canonical.args,
            json!({"patch": PATCH, "dry_run": true})
        );
        assert_eq!(plan.version(), "v1");
        assert_eq!(plan.profile(), "siwc-opt-in");
    }

    #[test]
    fn duplicate_names_even_across_kind_are_rejected_per_namespace() {
        let custom = entry(
            ModelToolKind::Custom,
            Some("functions"),
            "apply_patch",
            "apply_patch",
            InputAdapter::ApplyPatchCustom,
        );
        let function = entry(
            ModelToolKind::Function,
            Some("functions"),
            "apply_patch",
            "apply_patch",
            InputAdapter::FunctionJson,
        );
        assert_eq!(
            ModelToolPlan::new("v1", "p", vec![custom, function]).unwrap_err(),
            ToolPlanError::DeclarationConflict
        );
        let a = entry(
            ModelToolKind::Function,
            Some("a"),
            "read",
            "read",
            InputAdapter::FunctionJson,
        );
        let b = entry(
            ModelToolKind::Function,
            Some("b"),
            "read",
            "mcp__b__read",
            InputAdapter::FunctionJson,
        );
        assert!(ModelToolPlan::new("v1", "p", vec![a, b]).is_ok());
    }

    #[test]
    fn incomplete_unknown_and_invalid_payloads_fail_closed() {
        let custom = entry(
            ModelToolKind::Custom,
            None,
            "apply_patch",
            "apply_patch",
            InputAdapter::ApplyPatchCustom,
        );
        let preview = entry(
            ModelToolKind::Function,
            None,
            "apply_patch_preview",
            "apply_patch",
            InputAdapter::ApplyPatchPreview,
        );
        let plan =
            ModelToolPlan::new("v1", "p", vec![custom.clone(), preview.clone()]).expect("plan");
        for completion in [
            NativeToolCompletion::Receiving,
            NativeToolCompletion::Invalid,
            NativeToolCompletion::Interrupted,
        ] {
            let mut call = native(&custom, NativeToolPayload::RawText(PATCH.into()));
            call.completion = completion;
            assert_eq!(plan.adapt(call).unwrap_err(), ToolPlanError::IncompleteCall);
        }
        let mut call = native(&custom, NativeToolPayload::RawText(PATCH.into()));
        call.address.namespace = Some("unknown".into());
        assert_eq!(
            plan.adapt(call).unwrap_err(),
            ToolPlanError::UnknownToolAddress
        );
        let mut call = native(&custom, NativeToolPayload::RawText(PATCH.into()));
        call.call_id = " ".into();
        assert_eq!(
            plan.adapt(call).unwrap_err(),
            ToolPlanError::InvalidCallIdentity
        );
        assert_eq!(
            plan.adapt(native(
                &custom,
                NativeToolPayload::JsonArguments(json!({"patch": PATCH}))
            ))
            .unwrap_err(),
            ToolPlanError::InvalidPayload
        );
        for args in [
            json!({"patch": PATCH, "dry_run": true}),
            json!({"input": PATCH}),
            json!({"patch": 42}),
            json!(null),
        ] {
            assert_eq!(
                plan.adapt(native(&preview, NativeToolPayload::JsonArguments(args)))
                    .unwrap_err(),
                ToolPlanError::InvalidPayload
            );
        }
    }

    #[test]
    fn declarations_cannot_redirect_special_adapters_or_mismatch_kind() {
        let mut custom = entry(
            ModelToolKind::Custom,
            None,
            "apply_patch",
            "delete",
            InputAdapter::ApplyPatchCustom,
        );
        assert_eq!(
            ModelToolPlan::new("v1", "p", vec![custom.clone()]).unwrap_err(),
            ToolPlanError::InvalidAdapter
        );
        custom.internal_tool_key = "apply_patch".into();
        custom.address.name = "other".into();
        assert_eq!(
            ModelToolPlan::new("v1", "p", vec![custom.clone()]).unwrap_err(),
            ToolPlanError::InvalidAdapter
        );
        custom.address.name = "apply_patch".into();
        custom.declaration = ModelToolDeclaration::Function {
            description: "fixture".into(),
            parameters: json!({}),
            strict: false,
        };
        assert_eq!(
            ModelToolPlan::new("v1", "p", vec![custom]).unwrap_err(),
            ToolPlanError::InvalidDeclaration
        );
    }

    #[test]
    fn legacy_function_passes_object_without_repair() {
        let legacy = entry(
            ModelToolKind::Function,
            None,
            "apply_patch",
            "apply_patch",
            InputAdapter::FunctionJson,
        );
        let plan = ModelToolPlan::new("v1", "legacy", vec![legacy.clone()]).expect("plan");
        let args = json!({"input": PATCH});
        let result = plan
            .adapt(native(
                &legacy,
                NativeToolPayload::JsonArguments(args.clone()),
            ))
            .expect("legacy");
        assert_eq!(result.canonical.args, args);
        assert_eq!(
            plan.adapt(native(
                &legacy,
                NativeToolPayload::JsonArguments(json!(null))
            ))
            .unwrap_err(),
            ToolPlanError::InvalidPayload
        );
    }

    #[test]
    fn canonical_patch_views_have_identical_permission_and_conflict_targets() {
        use qaqh_permission::conflict::{file_write_paths, resolve_write_conflicts};
        use qaqh_permission::permission::{
            PermissionDecision, PermissionLevel, ToolCategory, needs_permission,
        };
        use std::collections::HashSet;

        let dir = tempfile::tempdir().expect("tempdir");
        let ws = dir.path().join("workspace");
        std::fs::create_dir(&ws).expect("workspace");
        let patch = "*** Begin Patch\n*** Add File: add.txt\n+add\n*** Update File: old.txt\n*** Move to: ../outside/moved.txt\n@@\n-old\n+new\n*** Delete File: delete.txt\n*** End Patch\n";
        let custom = entry(
            ModelToolKind::Custom,
            None,
            "apply_patch",
            "apply_patch",
            InputAdapter::ApplyPatchCustom,
        );
        let preview = entry(
            ModelToolKind::Function,
            None,
            "apply_patch_preview",
            "apply_patch",
            InputAdapter::ApplyPatchPreview,
        );
        let legacy = entry(
            ModelToolKind::Function,
            None,
            "legacy_patch",
            "apply_patch",
            InputAdapter::FunctionJson,
        );
        let plan = ModelToolPlan::new(
            "v1",
            "p",
            vec![custom.clone(), preview.clone(), legacy.clone()],
        )
        .expect("plan");
        let calls = [
            plan.adapt(native(&custom, NativeToolPayload::RawText(patch.into())))
                .expect("custom")
                .canonical,
            plan.adapt(native(
                &preview,
                NativeToolPayload::JsonArguments(json!({"patch":patch})),
            ))
            .expect("preview")
            .canonical,
            plan.adapt(native(
                &legacy,
                NativeToolPayload::JsonArguments(json!({"patch":patch, "dry_run":false})),
            ))
            .expect("legacy")
            .canonical,
        ];
        let expected = vec!["add.txt", "old.txt", "../outside/moved.txt", "delete.txt"];
        for call in &calls {
            assert_eq!(call.internal_tool_key, "apply_patch");
            assert_eq!(
                file_write_paths(&call.internal_tool_key, &call.args),
                expected
            );
            let paths =
                qaqh_permission::extract_target_paths_in(&call.internal_tool_key, &call.args, &ws);
            assert_eq!(paths.len(), 4);
            assert!(paths[2].starts_with(dir.path().join("outside")));
            let decision = needs_permission(
                PermissionLevel::WorkspaceWrite,
                &call.internal_tool_key,
                &call.args,
                &ws,
                &HashSet::new(),
                ToolCategory::Write,
            );
            assert!(
                matches!(decision, PermissionDecision::AskUser { .. }),
                "outside target must be approved: {decision:?}"
            );
        }
        let pending: Vec<_> = calls
            .iter()
            .map(|call| (call.internal_tool_key.clone(), call.args.clone()))
            .collect();
        let (groups, serial_after) = resolve_write_conflicts(&pending);
        assert_eq!(groups, vec![vec![0, 1, 2]]);
        assert_eq!(serial_after, HashSet::from([1, 2]));
    }

    #[cfg(unix)]
    #[test]
    fn canonical_custom_path_resolves_external_symlink_before_permission() {
        use qaqh_permission::permission::{
            PermissionDecision, PermissionLevel, ToolCategory, needs_permission,
        };
        use std::collections::HashSet;

        let root = tempfile::tempdir().expect("tempdir");
        let ws = root.path().join("ws");
        let outside = root.path().join("outside");
        std::fs::create_dir(&ws).expect("workspace");
        std::fs::create_dir(&outside).expect("outside");
        std::os::unix::fs::symlink(&outside, ws.join("link")).expect("symlink");
        let patch = "*** Begin Patch\n*** Add File: link/new.txt\n+new\n*** End Patch\n";
        let custom = entry(
            ModelToolKind::Custom,
            None,
            "apply_patch",
            "apply_patch",
            InputAdapter::ApplyPatchCustom,
        );
        let plan = ModelToolPlan::new("v1", "p", vec![custom.clone()]).expect("plan");
        let call = plan
            .adapt(native(&custom, NativeToolPayload::RawText(patch.into())))
            .expect("adapt")
            .canonical;
        let paths =
            qaqh_permission::extract_target_paths_in(&call.internal_tool_key, &call.args, &ws);
        assert_eq!(paths, vec![outside.join("new.txt")]);
        let decision = needs_permission(
            PermissionLevel::WorkspaceWrite,
            &call.internal_tool_key,
            &call.args,
            &ws,
            &HashSet::new(),
            ToolCategory::Write,
        );
        assert!(matches!(decision, PermissionDecision::AskUser { .. }));
    }
}
