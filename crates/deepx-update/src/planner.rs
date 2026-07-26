use crate::{
    Artifact, ArtifactKind, Catalog, InstalledState, Result, UpdateAction, UpdateError, UpdateMode,
    UpdatePlan,
};

pub fn plan_update(state: Option<&InstalledState>, catalog: &Catalog) -> Result<UpdatePlan> {
    catalog.validate()?;
    let Some(state) = state else {
        return full_plan(catalog, UpdateMode::Install);
    };

    let runtime_changed = component_changed(state, catalog, "runtime");
    let frontend_changed = component_changed(state, catalog, "frontend");
    let backend_changed = component_changed(state, catalog, "backend");

    if !runtime_changed && !frontend_changed && !backend_changed {
        return Ok(UpdatePlan {
            operation_id: operation_id(&catalog.release_id, &[]),
            release_id: catalog.release_id.clone(),
            mode: UpdateMode::Current,
            artifacts: Vec::new(),
            actions: Vec::new(),
        });
    }

    if runtime_changed {
        if let Some(runtime) = artifact(catalog, ArtifactKind::Runtime) {
            return component_plan(catalog, vec![runtime], UpdateMode::Upgrade);
        }
        return full_plan(catalog, UpdateMode::Upgrade);
    }

    let target_protocol = catalog
        .components
        .get("backend")
        .and_then(|component| component.control_protocol);
    let current_frontend_protocol = state
        .components
        .get("frontend")
        .and_then(|component| component.protocol);
    let current_backend_protocol = state
        .components
        .get("backend")
        .and_then(|component| component.protocol);
    let target_frontend_protocol = catalog
        .components
        .get("frontend")
        .and_then(|component| component.control_protocol);

    if frontend_changed && backend_changed {
        if target_frontend_protocol != target_protocol {
            return full_plan(catalog, UpdateMode::Upgrade);
        }
        if let (Some(frontend), Some(backend)) = (
            artifact(catalog, ArtifactKind::Frontend),
            artifact(catalog, ArtifactKind::Backend),
        ) {
            return component_plan(catalog, vec![frontend, backend], UpdateMode::Update);
        }
        return full_plan(catalog, UpdateMode::Upgrade);
    }

    if backend_changed {
        if target_protocol != current_frontend_protocol {
            return full_plan(catalog, UpdateMode::Upgrade);
        }
        if let Some(backend) = artifact(catalog, ArtifactKind::Backend) {
            return component_plan(catalog, vec![backend], UpdateMode::Update);
        }
    }

    if frontend_changed {
        if target_frontend_protocol != current_backend_protocol {
            return full_plan(catalog, UpdateMode::Upgrade);
        }
        if let Some(frontend) = artifact(catalog, ArtifactKind::Frontend) {
            return component_plan(catalog, vec![frontend], UpdateMode::Update);
        }
    }

    full_plan(catalog, UpdateMode::Upgrade)
}

fn component_changed(state: &InstalledState, catalog: &Catalog, name: &str) -> bool {
    let Some(target) = catalog.components.get(name) else {
        return false;
    };
    let current = state
        .components
        .get(name)
        .map(|component| &component.current);
    current != Some(&target.build_id)
}

fn artifact(catalog: &Catalog, kind: ArtifactKind) -> Option<&Artifact> {
    catalog
        .artifacts
        .iter()
        .find(|artifact| artifact.kind == kind)
}

fn full_plan(catalog: &Catalog, mode: UpdateMode) -> Result<UpdatePlan> {
    let full = artifact(catalog, ArtifactKind::Full)
        .ok_or_else(|| UpdateError("catalog has no applicable artifact or full fallback".into()))?;
    component_plan(catalog, vec![full], mode)
}

fn component_plan(
    catalog: &Catalog,
    artifacts: Vec<&Artifact>,
    mode: UpdateMode,
) -> Result<UpdatePlan> {
    let ids = artifacts
        .iter()
        .map(|artifact| artifact.id.clone())
        .collect::<Vec<_>>();
    let mut actions = vec![UpdateAction::Stage];
    for artifact in artifacts {
        match artifact.kind {
            ArtifactKind::Backend => actions.extend([
                UpdateAction::PrepareBackend,
                UpdateAction::ApplyBackend,
                UpdateAction::RestartBackend,
                UpdateAction::VerifyBackend,
            ]),
            ArtifactKind::Frontend | ArtifactKind::Shell => actions.extend([
                UpdateAction::PrepareFrontend,
                UpdateAction::ApplyFrontend,
                UpdateAction::RestartElectron,
            ]),
            ArtifactKind::Renderer => {
                actions.extend([UpdateAction::PrepareFrontend, UpdateAction::ApplyFrontend])
            }
            ArtifactKind::Runtime => {
                actions.extend([UpdateAction::PrepareFrontend, UpdateAction::ApplyRuntime])
            }
            ArtifactKind::Full => actions.push(UpdateAction::ApplyFull),
        }
    }
    actions.extend([UpdateAction::VerifyInstallation, UpdateAction::Commit]);
    Ok(UpdatePlan {
        operation_id: operation_id(&catalog.release_id, &ids),
        release_id: catalog.release_id.clone(),
        mode,
        artifacts: ids,
        actions,
    })
}

fn operation_id(release_id: &str, artifacts: &[String]) -> String {
    let suffix = if artifacts.is_empty() {
        "current".into()
    } else {
        artifacts.join("+")
    };
    let raw = format!("op-{release_id}-{suffix}");
    raw.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | '+') {
                character
            } else {
                '-'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::{
        ArtifactPayload, ArtifactRequires, ArtifactStrategy, CatalogComponent, ComponentHealth,
        ComponentState, RestartPolicy,
    };

    fn catalog() -> Catalog {
        Catalog {
            format_version: 1,
            release_id: "release-2".into(),
            channel: "local".into(),
            published_at: "2026-07-27T00:00:00Z".into(),
            components: BTreeMap::from([
                (
                    "runtime".into(),
                    CatalogComponent {
                        build_id: "runtime-1".into(),
                        version: "43".into(),
                        control_protocol: None,
                    },
                ),
                (
                    "frontend".into(),
                    CatalogComponent {
                        build_id: "frontend-2".into(),
                        version: "0.9".into(),
                        control_protocol: Some(1),
                    },
                ),
                (
                    "backend".into(),
                    CatalogComponent {
                        build_id: "backend-2".into(),
                        version: "0.9".into(),
                        control_protocol: Some(1),
                    },
                ),
            ]),
            artifacts: vec![
                artifact("frontend", ArtifactKind::Frontend),
                artifact("backend", ArtifactKind::Backend),
                artifact("full", ArtifactKind::Full),
            ],
        }
    }

    fn artifact(id: &str, kind: ArtifactKind) -> Artifact {
        Artifact {
            id: id.into(),
            kind,
            strategy: ArtifactStrategy::ComponentFull,
            targets: BTreeMap::from([(id.into(), format!("{id}-2"))]),
            requires: ArtifactRequires::default(),
            restart_policy: RestartPolicy::Full,
            payload: ArtifactPayload {
                path: format!("bundles/{id}.zip"),
                size: 1,
                sha256: "a".repeat(64),
            },
        }
    }

    fn state(frontend: &str, backend: &str) -> InstalledState {
        InstalledState {
            format_version: 2,
            installation_id: "installation".into(),
            release_id: "release-1".into(),
            channel: "local".into(),
            components: BTreeMap::from([
                ("runtime".into(), component("runtime-1", None)),
                ("frontend".into(), component(frontend, Some(1))),
                ("backend".into(), component(backend, Some(1))),
            ]),
            last_committed_operation: None,
        }
    }

    fn component(build: &str, protocol: Option<u16>) -> ComponentState {
        ComponentState {
            current: build.into(),
            previous: None,
            version: "0.9".into(),
            protocol,
            health: ComponentHealth::Healthy,
        }
    }

    #[test]
    fn no_state_uses_full_install() {
        let plan = plan_update(None, &catalog()).unwrap();
        assert_eq!(plan.mode, UpdateMode::Install);
        assert_eq!(plan.artifacts, ["full"]);
    }

    #[test]
    fn backend_only_uses_backend_artifact() {
        let plan = plan_update(Some(&state("frontend-2", "backend-1")), &catalog()).unwrap();
        assert_eq!(plan.mode, UpdateMode::Update);
        assert_eq!(plan.artifacts, ["backend"]);
        assert!(plan.actions.contains(&UpdateAction::RestartBackend));
    }

    #[test]
    fn frontend_and_backend_use_two_component_artifacts() {
        let plan = plan_update(Some(&state("frontend-1", "backend-1")), &catalog()).unwrap();
        assert_eq!(plan.artifacts, ["frontend", "backend"]);
    }

    #[test]
    fn protocol_mismatch_falls_back_to_full() {
        let mut next = catalog();
        next.components.get_mut("backend").unwrap().control_protocol = Some(2);
        let plan = plan_update(Some(&state("frontend-2", "backend-1")), &next).unwrap();
        assert_eq!(plan.artifacts, ["full"]);
    }

    #[test]
    fn component_only_catalog_ignores_omitted_components() {
        let mut next = catalog();
        next.components.retain(|name, _| name == "frontend");
        next.artifacts
            .retain(|artifact| artifact.kind == ArtifactKind::Frontend);
        let plan = plan_update(Some(&state("frontend-1", "backend-1")), &next).unwrap();
        assert_eq!(plan.artifacts, ["frontend"]);
        assert!(!plan.actions.contains(&UpdateAction::ApplyFull));
    }
}
