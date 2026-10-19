use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use sha2::{Digest, Sha256};

use crate::{
    adapters::{compute_normalization_hash, normalize_claude},
    capture::{hex, manifest_hash},
    error::{ImportError, Result, io_error},
    model::{
        CapturedSource, ImportDiagnostic, ImportPhase, ImportProgress, OperationContext,
        PACKAGE_SCHEMA, PackageManifest, PrepareRequest, PreparedImport, VerificationReport,
    },
    path_identity::validate_encoded_relative,
};

/// Re-open a package directory from its manifest after process restart.
pub fn open_package(package_dir: impl AsRef<Path>) -> Result<CapturedSource> {
    let package_dir = package_dir.as_ref().to_path_buf();
    let manifest_path = package_dir.join("manifest.json");
    let manifest: PackageManifest = read_json(&manifest_path)?;
    let captured = CapturedSource {
        package_dir,
        manifest,
    };
    let report = verify(&captured)?;
    if !report.valid {
        return Err(ImportError::CorruptPackage(report.errors.join("; ")));
    }
    Ok(captured)
}

pub fn prepare(request: PrepareRequest) -> Result<PreparedImport> {
    prepare_with_context(request, &OperationContext::default())
}

pub fn prepare_with_context(
    request: PrepareRequest,
    context: &OperationContext,
) -> Result<PreparedImport> {
    context.check_cancelled()?;
    let report = verify_with_context(&request.captured, context)?;
    if !report.valid {
        return Err(ImportError::CorruptPackage(report.errors.join("; ")));
    }
    if request.captured.manifest.schema != PACKAGE_SCHEMA {
        return Err(ImportError::CorruptPackage(format!(
            "unsupported package schema {}",
            request.captured.manifest.schema
        )));
    }
    let (mut prepared, mut diagnostics) = normalize_claude(&request.captured, context)?;
    let diagnostic_path = request
        .captured
        .package_dir
        .join("discovery-diagnostics.json");
    if diagnostic_path.exists() {
        let discovery_diagnostics: Vec<ImportDiagnostic> = read_json(&diagnostic_path)?;
        diagnostics.splice(0..0, discovery_diagnostics.clone());
        let unresolved = discovery_diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code == "unresolved_attachment_reference")
            .map(|diagnostic| diagnostic.message.clone())
            .collect::<Vec<_>>();
        prepared.diagnostics.splice(0..0, discovery_diagnostics);
        if !unresolved.is_empty() {
            prepared.status = crate::model::ImportStatus::Incomplete;
            prepared.completeness.messages_complete = false;
            prepared.completeness.context_complete = crate::model::CompletenessState::Incomplete;
            for item in unresolved {
                if !prepared.completeness.missing_items.contains(&item) {
                    prepared.completeness.missing_items.push(item);
                }
            }
        }
        prepared.normalization_hash.clear();
        prepared.normalization_hash = compute_normalization_hash(&prepared)?;
    }
    let output = request.captured.package_dir.join("prepared.json");
    atomic_write_json(&output, &prepared)?;
    let report = verify_with_context(&request.captured, context)?;
    if !report.valid || !report.normalization_verified {
        return Err(ImportError::CorruptPackage(report.errors.join("; ")));
    }
    Ok(prepared)
}

pub fn verify(captured: &CapturedSource) -> Result<VerificationReport> {
    verify_with_context(captured, &OperationContext::default())
}

pub fn verify_with_context(
    captured: &CapturedSource,
    context: &OperationContext,
) -> Result<VerificationReport> {
    context.check_cancelled()?;
    let manifest_path = captured.package_dir.join("manifest.json");
    let disk_manifest: PackageManifest = read_json(&manifest_path)?;
    let mut report = VerificationReport {
        valid: true,
        package_hash: disk_manifest.package_hash.clone(),
        verified_artifacts: 0,
        verified_chunks: 0,
        normalization_verified: false,
        errors: Vec::new(),
    };
    if disk_manifest.schema != PACKAGE_SCHEMA {
        report
            .errors
            .push(format!("unsupported schema {}", disk_manifest.schema));
    }
    if disk_manifest.package_hash != captured.manifest.package_hash
        || serde_json::to_vec(&disk_manifest).ok() != serde_json::to_vec(&captured.manifest).ok()
    {
        report
            .errors
            .push("in-memory manifest differs from package manifest".into());
    }
    match manifest_hash(&disk_manifest) {
        Ok(expected) if expected == disk_manifest.package_hash => {}
        Ok(_) => report.errors.push("manifest package hash mismatch".into()),
        Err(error) => report.errors.push(error.to_string()),
    }

    for artifact in &disk_manifest.artifacts {
        context.check_cancelled()?;
        if artifact.capture.file_identity.is_empty()
            || artifact.capture.second_pass_sha256 != artifact.sha256
            || artifact.capture.modified_before_unix_ns != artifact.capture.modified_after_unix_ns
        {
            report.errors.push(format!(
                "artifact {} has an incomplete capture observation",
                artifact.artifact_id
            ));
        }
        if let Err(error) = validate_encoded_relative(&artifact.relative_path) {
            report
                .errors
                .push(format!("artifact path rejected: {error}"));
            continue;
        }
        let mut artifact_hasher = Sha256::new();
        let mut expected_offset = 0_u64;
        let mut artifact_length = 0_u64;
        let mut artifact_ok = true;
        for (index, chunk) in artifact.chunks.iter().enumerate() {
            context.check_cancelled()?;
            if chunk.ordinal != index as u64 || chunk.offset != expected_offset {
                report.errors.push(format!(
                    "artifact {} has non-contiguous chunk order at {}",
                    artifact.artifact_id, chunk.ordinal
                ));
                artifact_ok = false;
            }
            if !valid_hash(&chunk.sha256) {
                report
                    .errors
                    .push(format!("invalid chunk hash in {}", artifact.artifact_id));
                artifact_ok = false;
                continue;
            }
            let object_path = captured.package_dir.join("objects").join(&chunk.sha256);
            let metadata = match fs::symlink_metadata(&object_path) {
                Ok(metadata) => metadata,
                Err(error) => {
                    report
                        .errors
                        .push(format!("missing chunk {}: {error}", chunk.sha256));
                    artifact_ok = false;
                    continue;
                }
            };
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                report.errors.push(format!(
                    "chunk object is not a regular file: {}",
                    chunk.sha256
                ));
                artifact_ok = false;
                continue;
            }
            let (object_hash, object_length) = hash_file(&object_path, context)?;
            if object_hash != chunk.sha256 || object_length != chunk.byte_length {
                report.errors.push(format!(
                    "chunk object failed hash/length check: {}",
                    chunk.sha256
                ));
                artifact_ok = false;
                continue;
            }
            let mut file =
                File::open(&object_path).map_err(|error| io_error(&object_path, error))?;
            let mut buffer = [0_u8; 128 * 1024];
            loop {
                let count = file
                    .read(&mut buffer)
                    .map_err(|error| io_error(&object_path, error))?;
                if count == 0 {
                    break;
                }
                artifact_hasher.update(&buffer[..count]);
            }
            expected_offset = expected_offset.saturating_add(chunk.byte_length);
            artifact_length = artifact_length.saturating_add(chunk.byte_length);
            report.verified_chunks += 1;
            context.report(ImportProgress {
                phase: ImportPhase::Verify,
                artifact_id: Some(artifact.artifact_id.clone()),
                current_bytes: artifact_length,
                total_bytes: Some(artifact.byte_length),
            });
        }
        if artifact_length != artifact.byte_length || artifact.captured_len != artifact.byte_length
        {
            report.errors.push(format!(
                "artifact {} has an inconsistent byte length",
                artifact.artifact_id
            ));
            artifact_ok = false;
        }
        if hex(&artifact_hasher.finalize()) != artifact.sha256 {
            report.errors.push(format!(
                "artifact {} failed SHA-256 check",
                artifact.artifact_id
            ));
            artifact_ok = false;
        }
        if artifact_ok {
            report.verified_artifacts += 1;
        }
    }

    let prepared_path = captured.package_dir.join("prepared.json");
    if prepared_path.is_file() {
        match read_json::<PreparedImport>(&prepared_path) {
            Ok(prepared) => match compute_normalization_hash(&prepared) {
                Ok(hash)
                    if hash == prepared.normalization_hash
                        && prepared.package_hash == disk_manifest.package_hash =>
                {
                    report.normalization_verified = true;
                }
                Ok(_) => report
                    .errors
                    .push("normalized content hash or package binding mismatch".into()),
                Err(error) => report.errors.push(error.to_string()),
            },
            Err(error) => report
                .errors
                .push(format!("prepared import is invalid: {error}")),
        }
    }
    report.valid =
        report.errors.is_empty() && report.verified_artifacts == disk_manifest.artifacts.len();
    Ok(report)
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let file = File::open(path).map_err(|error| io_error(path, error))?;
    serde_json::from_reader(file).map_err(|error| ImportError::CorruptPackage(error.to_string()))
}

fn atomic_write_json(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| ImportError::CorruptPackage(error.to_string()))?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let temp = parent.join(format!(
        ".prepared-{}-{}.tmp",
        std::process::id(),
        epoch_nanos()
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(|error| io_error(&temp, error))?;
        file.write_all(&bytes)
            .map_err(|error| io_error(&temp, error))?;
        file.sync_all().map_err(|error| io_error(&temp, error))?;
        if path.exists() {
            fs::remove_file(path).map_err(|error| io_error(path, error))?;
        }
        fs::rename(&temp, path).map_err(|error| io_error(path, error))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn hash_file(path: &Path, context: &OperationContext) -> Result<(String, u64)> {
    let mut file = File::open(path).map_err(|error| io_error(path, error))?;
    let mut hasher = Sha256::new();
    let mut length = 0_u64;
    let mut buffer = [0_u8; 128 * 1024];
    loop {
        context.check_cancelled()?;
        let count = file
            .read(&mut buffer)
            .map_err(|error| io_error(path, error))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
        length = length.saturating_add(count as u64);
    }
    Ok((hex(&hasher.finalize()), length))
}

fn valid_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn epoch_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
}
