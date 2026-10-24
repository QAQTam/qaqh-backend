//! Artifact integrity and import-closure checks; no TypeScript toolchain required.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path};

use serde::Deserialize;
use sha2::{Digest, Sha256};

#[derive(Deserialize, serde::Serialize)]
struct FileHash {
    path: String,
    sha256: String,
}

#[derive(Deserialize)]
struct Manifest {
    files: Vec<FileHash>,
    contract_hash: String,
}

pub(super) fn validate(root: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let paths = super::list_files(root)?;
    let manifest: Manifest = serde_json::from_slice(&fs::read(root.join("manifest.json"))?)?;
    let declared: Vec<_> = manifest
        .files
        .iter()
        .map(|item| item.path.clone())
        .collect();
    let actual: Vec<_> = paths
        .iter()
        .filter(|path| path.as_str() != "manifest.json")
        .cloned()
        .collect();
    if declared != actual {
        return Err("manifest file set is missing, unsorted, duplicated or stale".into());
    }
    for entry in &manifest.files {
        let bytes = fs::read(root.join(&entry.path))?;
        if entry.sha256 != hex::encode(Sha256::digest(bytes)) {
            return Err(format!("manifest digest mismatch: {}", entry.path).into());
        }
    }
    if manifest.contract_hash != hex::encode(Sha256::digest(serde_json::to_vec(&manifest.files)?)) {
        return Err("manifest contract_hash mismatch".into());
    }
    let all: BTreeSet<_> = paths.iter().cloned().collect();
    for path in paths.iter().filter(|path| path.ends_with(".ts")) {
        let source = fs::read_to_string(root.join(path))?;
        // ts-rs emits one-line import/export clauses with quoted module names.
        for line in source.lines().filter(|line| {
            line.trim_start().starts_with("import ") || line.trim_start().starts_with("export ")
        }) {
            let Some((_, module)) = line.split_once(" from \"") else {
                continue;
            };
            let Some((module, _)) = module.split_once('"') else {
                return Err(format!("unterminated TypeScript module import: {path}").into());
            };
            if !module.starts_with('.') {
                return Err(
                    format!("external dependency in generated contract: {path}: {module}").into(),
                );
            }
            let mut relative = Path::new(path)
                .parent()
                .unwrap_or(Path::new(""))
                .to_path_buf();
            for component in Path::new(module).components() {
                match component {
                    Component::CurDir => {}
                    Component::Normal(part) => relative.push(part),
                    Component::ParentDir if relative.pop() => {}
                    _ => {
                        return Err(
                            format!("module escapes contract directory: {path}: {module}").into(),
                        );
                    }
                }
            }
            let name = format!("{}.ts", relative.to_string_lossy().replace('\\', "/"));
            if !all.contains(&name) {
                return Err(format!("unresolved TypeScript import: {path}: {module}").into());
            }
        }
    }
    let public_index = fs::read_to_string(root.join("index.ts"))?;
    if public_index.contains("DaemonDiscovery") {
        return Err("host credential DTO must only be exported from node.ts".into());
    }
    Ok(())
}
