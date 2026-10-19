use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use sha2::{Digest, Sha256};

use crate::{
    error::{ImportError, Result, io_error},
    model::{
        CaptureObservation, CaptureRequest, CapturedSource, ChunkRef, ImportPhase, ImportProgress,
        MAX_CHUNK_SIZE, PACKAGE_SCHEMA, PackageManifest, SourceArtifact,
    },
};

pub fn capture(request: CaptureRequest) -> Result<CapturedSource> {
    capture_with_context(request, &crate::model::OperationContext::default())
}

pub fn capture_with_context(
    request: CaptureRequest,
    context: &crate::model::OperationContext,
) -> Result<CapturedSource> {
    context.check_cancelled()?;
    if request.options.chunk_size == 0 || request.options.chunk_size > MAX_CHUNK_SIZE {
        return Err(ImportError::ResourceLimit(format!(
            "chunk_size must be between 1 and {MAX_CHUNK_SIZE} bytes"
        )));
    }
    let package_dir = request.package_dir.clone();
    if fs::symlink_metadata(&package_dir).is_ok() {
        return Err(ImportError::PackageExists(package_dir));
    }
    let parent = package_dir.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|error| io_error(parent, error))?;
    let file_name = package_dir
        .file_name()
        .ok_or_else(|| ImportError::UnsupportedSource("package path has no file name".into()))?;
    let temp_name = format!(
        ".qaqh-import-capture-{}-{}-{}",
        std::process::id(),
        sha256(file_name.to_string_lossy().as_bytes()),
        epoch_nanos()
    );
    let working_dir = parent.join(temp_name);
    fs::create_dir(&working_dir).map_err(|error| io_error(&working_dir, error))?;
    let result = capture_into(&request, &working_dir, context);
    match result {
        Ok(manifest) => {
            if let Err(source) = fs::rename(&working_dir, &package_dir) {
                let _ = fs::remove_dir_all(&working_dir);
                return Err(io_error(&package_dir, source));
            }
            Ok(CapturedSource {
                package_dir,
                manifest,
            })
        }
        Err(error) => {
            // This exact path was created by this invocation and contains no
            // user data, so cleanup is confined to our owned temporary folder.
            let _ = fs::remove_dir_all(&working_dir);
            Err(error)
        }
    }
}

fn capture_into(
    request: &CaptureRequest,
    working_dir: &Path,
    context: &crate::model::OperationContext,
) -> Result<PackageManifest> {
    let objects_dir = working_dir.join("objects");
    fs::create_dir(&objects_dir).map_err(|error| io_error(&objects_dir, error))?;
    let mut artifacts = Vec::with_capacity(request.discovery.artifacts.len());
    let mut total_bytes = 0_u64;
    for discovered in &request.discovery.artifacts {
        context.check_cancelled()?;
        let before_meta = fs::symlink_metadata(&discovered.source_path)
            .map_err(|error| io_error(&discovered.source_path, error))?;
        if before_meta.file_type().is_symlink() || !before_meta.is_file() {
            return Err(ImportError::SourceChanged(discovered.source_path.clone()));
        }
        let canonical_before = fs::canonicalize(&discovered.source_path)
            .map_err(|error| io_error(&discovered.source_path, error))?;
        if canonical_before != discovered.source_path
            || !request
                .discovery
                .allowed_roots
                .iter()
                .any(|root| canonical_before.starts_with(root))
        {
            return Err(ImportError::SourceChanged(discovered.source_path.clone()));
        }
        if request
            .options
            .max_file_bytes
            .is_some_and(|limit| before_meta.len() > limit)
        {
            return Err(ImportError::ResourceLimit(format!(
                "file {} exceeds configured maximum",
                discovered.display_path
            )));
        }
        let identity_before = file_identity_path(&discovered.source_path).ok_or_else(|| {
            ImportError::ResourceLimit(format!(
                "filesystem does not expose stable file identity for {}",
                discovered.display_path
            ))
        })?;
        let modified_before = modified_unix_ns(&before_meta).ok_or_else(|| {
            ImportError::ResourceLimit(format!(
                "filesystem does not expose modification time for {}",
                discovered.display_path
            ))
        })?;
        total_bytes = total_bytes.saturating_add(before_meta.len());
        if request
            .options
            .max_total_bytes
            .is_some_and(|limit| total_bytes > limit)
        {
            return Err(ImportError::ResourceLimit(
                "discovered files exceed configured total maximum".into(),
            ));
        }
        let prior_total_bytes = total_bytes.saturating_sub(before_meta.len());
        let (chunks, opened_identity) = capture_file_chunks(
            &discovered.source_path,
            &objects_dir,
            request.options.chunk_size,
            request.options.max_file_bytes,
            request.options.max_total_bytes,
            prior_total_bytes,
            discovered.artifact_id.as_str(),
            context,
        )?;
        let after_meta = fs::metadata(&discovered.source_path)
            .map_err(|error| io_error(&discovered.source_path, error))?;
        let identity_after = file_identity_path(&discovered.source_path)
            .ok_or_else(|| ImportError::SourceChanged(discovered.source_path.clone()))?;
        let modified_after = modified_unix_ns(&after_meta)
            .ok_or_else(|| ImportError::SourceChanged(discovered.source_path.clone()))?;
        let canonical_after = fs::canonicalize(&discovered.source_path)
            .map_err(|error| io_error(&discovered.source_path, error))?;
        let metadata_stable = before_meta.len() == after_meta.len()
            && identity_before == opened_identity
            && identity_before == identity_after
            && modified_before == modified_after
            && canonical_before == canonical_after;
        let captured_len = chunks.iter().map(|chunk| chunk.byte_length).sum::<u64>();
        if request
            .options
            .max_file_bytes
            .is_some_and(|limit| captured_len > limit)
        {
            return Err(ImportError::ResourceLimit(format!(
                "file {} grew beyond configured maximum during capture",
                discovered.display_path
            )));
        }
        if !metadata_stable || captured_len != after_meta.len() {
            return Err(ImportError::SourceChanged(discovered.source_path.clone()));
        }
        let second_hash = hash_file(&discovered.source_path, context)?;
        let final_path_meta = fs::metadata(&discovered.source_path)
            .map_err(|error| io_error(&discovered.source_path, error))?;
        if second_hash.2 != identity_before
            || file_identity_path(&discovered.source_path).as_deref()
                != Some(identity_before.as_str())
            || modified_unix_ns(&final_path_meta) != Some(modified_after)
        {
            return Err(ImportError::SourceChanged(discovered.source_path.clone()));
        }
        let file_sha = combine_chunk_hash(&chunks, &objects_dir, context)?;
        if second_hash.0 != file_sha || second_hash.1 != captured_len {
            return Err(ImportError::SourceChanged(discovered.source_path.clone()));
        }
        artifacts.push(SourceArtifact {
            artifact_id: discovered.artifact_id.clone(),
            kind: discovered.kind,
            relative_path: discovered.relative_path.clone(),
            display_path: discovered.display_path.clone(),
            source_location: discovered.source_location.clone(),
            referenced_paths: discovered.referenced_paths.clone(),
            byte_length: captured_len,
            sha256: file_sha,
            chunks,
            captured_len,
            capture: CaptureObservation {
                file_identity: identity_before,
                modified_before_unix_ns: Some(modified_before),
                modified_after_unix_ns: Some(modified_after),
                second_pass_sha256: second_hash.0,
                captured_at_unix_ms: epoch_ms(),
            },
        });
        context.report(ImportProgress {
            phase: ImportPhase::Capture,
            artifact_id: Some(discovered.artifact_id.clone()),
            current_bytes: total_bytes,
            total_bytes: request.options.max_total_bytes,
        });
    }
    let mut manifest = PackageManifest {
        schema: PACKAGE_SCHEMA.into(),
        source: request.discovery.source.clone(),
        snapshot_boundary: request.discovery.snapshot_boundary.clone(),
        artifacts,
        package_hash: String::new(),
    };
    manifest.package_hash = manifest_hash(&manifest)?;
    let path = working_dir.join("manifest.json");
    write_new_json(&path, &manifest)?;
    if !request.discovery.diagnostics.is_empty() {
        let path = working_dir.join("discovery-diagnostics.json");
        write_new_json(&path, &request.discovery.diagnostics)?;
    }
    Ok(manifest)
}

fn capture_file_chunks(
    path: &Path,
    objects_dir: &Path,
    chunk_size: usize,
    max_file_bytes: Option<u64>,
    max_total_bytes: Option<u64>,
    prior_total_bytes: u64,
    artifact_id: &str,
    context: &crate::model::OperationContext,
) -> Result<(Vec<ChunkRef>, String)> {
    let mut file = File::open(path).map_err(|error| io_error(path, error))?;
    let opened_identity =
        file_identity_file(&file).ok_or_else(|| ImportError::SourceChanged(path.to_path_buf()))?;
    let mut chunks = Vec::new();
    let mut buffer = vec![0_u8; chunk_size];
    let mut offset = 0_u64;
    loop {
        context.check_cancelled()?;
        let mut filled = 0;
        while filled < buffer.len() {
            let count = file
                .read(&mut buffer[filled..])
                .map_err(|error| io_error(path, error))?;
            if count == 0 {
                break;
            }
            filled += count;
        }
        if filled == 0 {
            break;
        }
        let next_offset = offset.saturating_add(filled as u64);
        if max_file_bytes.is_some_and(|limit| next_offset > limit) {
            return Err(ImportError::ResourceLimit(format!(
                "file {} grew beyond configured maximum during capture",
                path.display()
            )));
        }
        if max_total_bytes
            .is_some_and(|limit| prior_total_bytes.saturating_add(next_offset) > limit)
        {
            return Err(ImportError::ResourceLimit(
                "captured files grew beyond configured total maximum".into(),
            ));
        }
        let bytes = &buffer[..filled];
        let hash = sha256(bytes);
        let object = objects_dir.join(&hash);
        if !object.exists() {
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&object)
                .map_err(|error| io_error(&object, error))?;
            output
                .write_all(bytes)
                .map_err(|error| io_error(&object, error))?;
            output
                .sync_all()
                .map_err(|error| io_error(&object, error))?;
        }
        chunks.push(ChunkRef {
            ordinal: chunks.len() as u64,
            offset,
            byte_length: filled as u64,
            sha256: hash,
        });
        offset = next_offset;
        context.report(ImportProgress {
            phase: ImportPhase::Capture,
            artifact_id: Some(artifact_id.to_owned()),
            current_bytes: prior_total_bytes.saturating_add(offset),
            total_bytes: max_total_bytes,
        });
        if filled < buffer.len() {
            break;
        }
    }
    Ok((chunks, opened_identity))
}

fn hash_file(
    path: &Path,
    context: &crate::model::OperationContext,
) -> Result<(String, u64, String)> {
    let mut file = File::open(path).map_err(|error| io_error(path, error))?;
    let identity =
        file_identity_file(&file).ok_or_else(|| ImportError::SourceChanged(path.to_path_buf()))?;
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
    let identity_after =
        file_identity_file(&file).ok_or_else(|| ImportError::SourceChanged(path.to_path_buf()))?;
    if identity != identity_after {
        return Err(ImportError::SourceChanged(path.to_path_buf()));
    }
    Ok((hex(&hasher.finalize()), length, identity))
}

fn combine_chunk_hash(
    chunks: &[ChunkRef],
    objects_dir: &Path,
    context: &crate::model::OperationContext,
) -> Result<String> {
    let mut hasher = Sha256::new();
    for chunk in chunks {
        context.check_cancelled()?;
        let object = objects_dir.join(&chunk.sha256);
        let mut file = File::open(&object).map_err(|error| io_error(&object, error))?;
        let mut buffer = [0_u8; 128 * 1024];
        loop {
            let count = file
                .read(&mut buffer)
                .map_err(|error| io_error(&object, error))?;
            if count == 0 {
                break;
            }
            hasher.update(&buffer[..count]);
        }
    }
    Ok(hex(&hasher.finalize()))
}

pub fn restore_artifact(
    captured: &CapturedSource,
    artifact_id: &str,
    destination: impl AsRef<Path>,
) -> Result<()> {
    let artifact = captured
        .manifest
        .artifacts
        .iter()
        .find(|artifact| artifact.artifact_id == artifact_id)
        .ok_or_else(|| ImportError::MissingReference(artifact_id.to_owned()))?;
    let destination = destination.as_ref();
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .map_err(|error| io_error(destination, error))?;
    let mut hasher = Sha256::new();
    let mut length = 0_u64;
    for chunk in &artifact.chunks {
        let object = captured.package_dir.join("objects").join(&chunk.sha256);
        let bytes = fs::read(&object).map_err(|error| io_error(&object, error))?;
        if bytes.len() as u64 != chunk.byte_length || sha256(&bytes) != chunk.sha256 {
            return Err(ImportError::CorruptPackage(format!(
                "chunk {} failed verification",
                chunk.sha256
            )));
        }
        output
            .write_all(&bytes)
            .map_err(|error| io_error(destination, error))?;
        hasher.update(&bytes);
        length = length.saturating_add(bytes.len() as u64);
    }
    output
        .sync_all()
        .map_err(|error| io_error(destination, error))?;
    if length != artifact.byte_length || hex(&hasher.finalize()) != artifact.sha256 {
        let _ = fs::remove_file(destination);
        return Err(ImportError::CorruptPackage(format!(
            "artifact {} failed verification",
            artifact.artifact_id
        )));
    }
    Ok(())
}

pub(crate) fn manifest_hash(manifest: &PackageManifest) -> Result<String> {
    #[derive(serde::Serialize)]
    struct PackageIdentityArtifact<'a> {
        artifact_id: &'a str,
        kind: crate::model::ArtifactKind,
        relative_path: &'a crate::model::EncodedPath,
        source_relative_path: &'a crate::model::EncodedPath,
        referenced_paths: &'a [String],
        byte_length: u64,
        sha256: &'a str,
    }
    #[derive(serde::Serialize)]
    struct PackageIdentity<'a> {
        source_kind: &'a crate::model::SourceKind,
        source_session_key: &'a Option<String>,
        artifacts: Vec<PackageIdentityArtifact<'a>>,
    }
    let identity = PackageIdentity {
        source_kind: &manifest.source.source_kind,
        source_session_key: &manifest.source.source_session_key,
        artifacts: manifest
            .artifacts
            .iter()
            .map(|artifact| PackageIdentityArtifact {
                artifact_id: &artifact.artifact_id,
                kind: artifact.kind,
                relative_path: &artifact.relative_path,
                source_relative_path: &artifact.source_location.relative_path,
                referenced_paths: &artifact.referenced_paths,
                byte_length: artifact.byte_length,
                sha256: &artifact.sha256,
            })
            .collect(),
    };
    let bytes = serde_json::to_vec(&identity)
        .map_err(|error| ImportError::CorruptPackage(error.to_string()))?;
    Ok(sha256(&bytes))
}

pub(crate) fn sha256(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}

fn write_new_json(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| ImportError::CorruptPackage(error.to_string()))?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| io_error(path, error))?;
    file.write_all(&bytes)
        .map_err(|error| io_error(path, error))?;
    file.sync_all().map_err(|error| io_error(path, error))?;
    Ok(())
}

fn epoch_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
}

fn epoch_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

fn modified_unix_ns(metadata: &fs::Metadata) -> Option<u128> {
    metadata
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|time| time.as_nanos())
}

fn file_identity_path(path: &Path) -> Option<String> {
    let file = File::open(path).ok()?;
    file_identity_file(&file)
}

fn file_identity_file(file: &File) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata().ok()?;
        Some(format!("unix:{}:{}", metadata.dev(), metadata.ino()))
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
        };
        let mut information = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
        // The handle is a live File handle and the API initializes the complete
        // information structure on success.
        let result = unsafe {
            GetFileInformationByHandle(file.as_raw_handle().cast(), information.as_mut_ptr())
        };
        if result == 0 {
            return None;
        }
        let information = unsafe { information.assume_init() };
        let index =
            (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow);
        Some(format!(
            "windows:{}:{index}",
            information.dwVolumeSerialNumber
        ))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = file;
        None
    }
}
