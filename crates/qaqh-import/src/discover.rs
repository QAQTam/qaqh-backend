use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use sha2::{Digest, Sha256};

use crate::{
    error::{ImportError, Result, io_error},
    model::{
        ArtifactKind, DiagnosticSeverity, DiscoverRequest, DiscoveredArtifact, Discovery,
        EncodedPath, ImportDiagnostic, ImportPhase, ImportProgress, OperationContext,
        SourceDescriptor, SourceKind, SourceLocation,
    },
    path_identity::{display, encode_relative},
};

pub fn discover(request: DiscoverRequest) -> Result<Discovery> {
    discover_with_context(request, &OperationContext::default())
}

pub fn discover_with_context(
    request: DiscoverRequest,
    context: &OperationContext,
) -> Result<Discovery> {
    context.check_cancelled()?;
    if request.allowed_roots.is_empty() {
        return Err(ImportError::OutsideAllowedRoot(request.input));
    }
    let mut allowed_roots = Vec::with_capacity(request.allowed_roots.len());
    for root in &request.allowed_roots {
        allowed_roots.push(fs::canonicalize(root).map_err(|error| io_error(root, error))?);
    }

    let input_metadata =
        fs::symlink_metadata(&request.input).map_err(|error| io_error(&request.input, error))?;
    if input_metadata.file_type().is_symlink() {
        return Err(ImportError::SymbolicLink(request.input));
    }
    let input =
        fs::canonicalize(&request.input).map_err(|error| io_error(&request.input, error))?;
    if !allowed_roots.iter().any(|root| input.starts_with(root)) {
        return Err(ImportError::OutsideAllowedRoot(input));
    }

    let (source_root, mut candidate_files, transcript_paths, mut diagnostics) = if input.is_file() {
        if !has_extension(&input, "jsonl") {
            return Err(ImportError::UnsupportedSource(
                "a single transcript must be a .jsonl file".into(),
            ));
        }
        let source_root = input
            .parent()
            .ok_or_else(|| ImportError::UnsupportedSource("transcript has no parent".into()))?
            .to_path_buf();
        let mut files = vec![input.clone()];
        let mut diagnostics = Vec::new();
        let session_id = input.file_stem().unwrap_or_default();
        let session_dir = source_root.join(session_id);
        if session_dir.exists() {
            collect_tree(
                &session_dir,
                &source_root,
                &mut files,
                &mut diagnostics,
                context,
            )?;
        }
        let index = source_root.join("sessions-index.json");
        if index.is_file() {
            files.push(index);
        }
        (source_root, files, vec![input], diagnostics)
    } else if input.is_dir() {
        discover_project(&input, context)?
    } else {
        return Err(ImportError::UnsupportedSource(
            "input is neither a regular transcript nor a Claude project directory".into(),
        ));
    };

    let source_root =
        fs::canonicalize(&source_root).map_err(|error| io_error(&source_root, error))?;
    let selected_transcripts = candidate_files
        .iter()
        .filter(|path| has_extension(path, "jsonl"))
        .cloned()
        .collect::<Vec<_>>();
    let attachment_references = collect_attachment_references(
        &selected_transcripts,
        &source_root,
        &allowed_roots,
        &mut diagnostics,
        context,
    )?;
    candidate_files.extend(attachment_references.keys().cloned());
    let mut seen = HashSet::new();
    let mut artifacts = Vec::new();
    for candidate in candidate_files {
        context.check_cancelled()?;
        let canonical =
            fs::canonicalize(&candidate).map_err(|error| io_error(&candidate, error))?;
        let is_attachment = attachment_references.contains_key(&canonical);
        if !canonical.starts_with(&source_root) && !is_attachment {
            diagnostics.push(ImportDiagnostic {
                code: "reference_outside_source_root".into(),
                severity: DiagnosticSeverity::Warning,
                message: format!(
                    "excluded path outside selected source root: {}",
                    display(canonical.as_os_str())
                ),
                artifact_id: None,
                byte_offset: None,
                byte_length: None,
            });
            continue;
        }
        if !allowed_roots.iter().any(|root| canonical.starts_with(root)) {
            diagnostics.push(ImportDiagnostic {
                code: "reference_outside_allowed_root".into(),
                severity: DiagnosticSeverity::Warning,
                message: format!(
                    "excluded path outside allowed roots: {}",
                    display(canonical.as_os_str())
                ),
                artifact_id: None,
                byte_offset: None,
                byte_length: None,
            });
            continue;
        }
        let metadata =
            fs::symlink_metadata(&candidate).map_err(|error| io_error(&candidate, error))?;
        if metadata.file_type().is_symlink() {
            diagnostics.push(ImportDiagnostic {
                code: "symbolic_link_excluded".into(),
                severity: DiagnosticSeverity::Warning,
                message: format!("symbolic link excluded: {}", display(candidate.as_os_str())),
                artifact_id: None,
                byte_offset: None,
                byte_length: None,
            });
            continue;
        }
        if !metadata.is_file() || !seen.insert(canonical.clone()) {
            continue;
        }
        let location_root = allowed_roots
            .iter()
            .filter(|root| canonical.starts_with(root))
            .max_by_key(|root| root.components().count())
            .ok_or_else(|| ImportError::OutsideAllowedRoot(canonical.clone()))?;
        let root_index = allowed_roots
            .iter()
            .position(|root| root == location_root)
            .unwrap_or_default();
        let source_relative = canonical
            .strip_prefix(location_root)
            .map_err(|_| ImportError::OutsideAllowedRoot(canonical.clone()))?;
        let package_relative = if is_attachment {
            PathBuf::from("attachments")
                .join(format!("root-{root_index}"))
                .join(source_relative)
        } else {
            canonical
                .strip_prefix(&source_root)
                .map_err(|_| ImportError::OutsideAllowedRoot(canonical.clone()))?
                .to_path_buf()
        };
        let encoded = encode_relative(&package_relative);
        let artifact_kind = if is_attachment {
            ArtifactKind::Attachment
        } else {
            classify(&canonical, &source_root, &transcript_paths)
        };
        let artifact_id = artifact_id(&encoded);
        let source_location = SourceLocation {
            root: encode_relative(location_root),
            relative_path: encode_relative(source_relative),
        };
        artifacts.push(DiscoveredArtifact {
            artifact_id: artifact_id.clone(),
            kind: artifact_kind,
            relative_path: encoded,
            display_path: relative_display(&package_relative),
            source_location,
            referenced_paths: attachment_references
                .get(&canonical)
                .cloned()
                .unwrap_or_default(),
            source_path: canonical,
        });
        context.report(ImportProgress {
            phase: ImportPhase::Discover,
            artifact_id: Some(artifact_id.clone()),
            current_bytes: 0,
            total_bytes: None,
        });
    }
    artifacts.sort_by(|left, right| {
        (&left.relative_path.units_hex, left.artifact_id.as_str())
            .cmp(&(&right.relative_path.units_hex, right.artifact_id.as_str()))
    });
    if artifacts.is_empty() {
        return Err(ImportError::UnsupportedSource(
            "no eligible Claude transcript or session metadata files found".into(),
        ));
    }
    let source_session_key = (transcript_paths.len() == 1)
        .then(|| transcript_paths[0].file_stem().map(|stem| display(stem)))
        .flatten();
    Ok(Discovery {
        source: SourceDescriptor {
            source_kind: SourceKind::ClaudeCode,
            adapter_version: crate::model::NORMALIZER_VERSION.into(),
            source_program_version: None,
            source_session_key,
        },
        artifacts,
        diagnostics,
        snapshot_boundary: format!("unix-ms:{}", epoch_ms()),
        allowed_roots,
    })
}

fn discover_project(
    root: &Path,
    context: &OperationContext,
) -> Result<(PathBuf, Vec<PathBuf>, Vec<PathBuf>, Vec<ImportDiagnostic>)> {
    let mut candidates = Vec::new();
    let mut transcript_paths = Vec::new();
    let mut diagnostics = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        context.check_cancelled()?;
        let entries = fs::read_dir(&dir).map_err(|error| io_error(&dir, error))?;
        for entry in entries {
            context.check_cancelled()?;
            let entry = entry.map_err(|error| io_error(&dir, error))?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).map_err(|error| io_error(&path, error))?;
            if metadata.file_type().is_symlink() {
                diagnostics.push(ImportDiagnostic {
                    code: "symbolic_link_excluded".into(),
                    severity: DiagnosticSeverity::Warning,
                    message: format!("symbolic link excluded: {}", display(path.as_os_str())),
                    artifact_id: None,
                    byte_offset: None,
                    byte_length: None,
                });
                continue;
            }
            if metadata.is_dir() {
                let canonical = fs::canonicalize(&path).map_err(|error| io_error(&path, error))?;
                if !canonical.starts_with(root) {
                    diagnostics.push(ImportDiagnostic {
                        code: "reparse_point_outside_source_root".into(),
                        severity: DiagnosticSeverity::Warning,
                        message: format!(
                            "directory target outside source root excluded: {}",
                            display(path.as_os_str())
                        ),
                        artifact_id: None,
                        byte_offset: None,
                        byte_length: None,
                    });
                    continue;
                }
                stack.push(path);
                continue;
            }
            if !metadata.is_file() {
                continue;
            }
            if has_extension(&path, "jsonl") {
                candidates.push(path.clone());
                if !path
                    .components()
                    .any(|component| component.as_os_str() == "subagents")
                {
                    transcript_paths.push(path);
                }
            } else if path.parent() == Some(root)
                && path
                    .file_name()
                    .is_some_and(|name| name == "sessions-index.json")
            {
                candidates.push(path);
            }
        }
    }
    if transcript_paths.is_empty() {
        return Err(ImportError::UnsupportedSource(
            "Claude project directory contains no .jsonl transcripts".into(),
        ));
    }
    // Transcript-adjacent session folders hold Claude subagent records and
    // auxiliary metadata. We never walk outside the chosen project root.
    for transcript in &transcript_paths {
        let Some(parent) = transcript.parent() else {
            continue;
        };
        let session_dir = parent.join(transcript.file_stem().unwrap_or_default());
        if session_dir.exists() {
            collect_tree(
                &session_dir,
                root,
                &mut candidates,
                &mut diagnostics,
                context,
            )?;
        }
    }
    Ok((
        root.to_path_buf(),
        candidates,
        transcript_paths,
        diagnostics,
    ))
}

fn collect_tree(
    start: &Path,
    root: &Path,
    files: &mut Vec<PathBuf>,
    diagnostics: &mut Vec<ImportDiagnostic>,
    context: &OperationContext,
) -> Result<()> {
    let canonical_root = fs::canonicalize(root).map_err(|error| io_error(root, error))?;
    let mut stack = vec![start.to_path_buf()];
    while let Some(dir) = stack.pop() {
        context.check_cancelled()?;
        let dir_metadata = fs::symlink_metadata(&dir).map_err(|error| io_error(&dir, error))?;
        if dir_metadata.file_type().is_symlink() {
            diagnostics.push(ImportDiagnostic {
                code: "symbolic_link_excluded".into(),
                severity: DiagnosticSeverity::Warning,
                message: format!("symbolic link excluded: {}", display(dir.as_os_str())),
                artifact_id: None,
                byte_offset: None,
                byte_length: None,
            });
            continue;
        }
        let canonical_dir = fs::canonicalize(&dir).map_err(|error| io_error(&dir, error))?;
        if !canonical_dir.starts_with(&canonical_root) {
            diagnostics.push(ImportDiagnostic {
                code: "reparse_point_outside_source_root".into(),
                severity: DiagnosticSeverity::Warning,
                message: format!(
                    "excluded directory outside source root: {}",
                    display(dir.as_os_str())
                ),
                artifact_id: None,
                byte_offset: None,
                byte_length: None,
            });
            continue;
        }
        let entries = fs::read_dir(&dir).map_err(|error| io_error(&dir, error))?;
        for entry in entries {
            context.check_cancelled()?;
            let entry = entry.map_err(|error| io_error(&dir, error))?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).map_err(|error| io_error(&path, error))?;
            if metadata.file_type().is_symlink() {
                diagnostics.push(ImportDiagnostic {
                    code: "symbolic_link_excluded".into(),
                    severity: DiagnosticSeverity::Warning,
                    message: format!("symbolic link excluded: {}", display(path.as_os_str())),
                    artifact_id: None,
                    byte_offset: None,
                    byte_length: None,
                });
            } else if metadata.is_dir() {
                stack.push(path);
            } else if metadata.is_file() {
                files.push(path);
            }
        }
    }
    Ok(())
}

fn collect_attachment_references(
    transcripts: &[PathBuf],
    source_root: &Path,
    allowed_roots: &[PathBuf],
    diagnostics: &mut Vec<ImportDiagnostic>,
    context: &OperationContext,
) -> Result<HashMap<PathBuf, Vec<String>>> {
    let mut attachments: HashMap<PathBuf, Vec<String>> = HashMap::new();
    for transcript in transcripts {
        context.check_cancelled()?;
        let references = scan_media_references(transcript, context)?;
        for (reference, byte_offset) in references {
            if reference.starts_with("http://") || reference.starts_with("https://") {
                continue;
            }
            if reference.starts_with("file://") || reference.starts_with("~/") {
                unresolved_attachment(&reference, transcript, byte_offset, diagnostics);
                continue;
            }
            let path = PathBuf::from(&reference);
            let path = if path.is_absolute() {
                path
            } else {
                source_root.join(path)
            };
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(_) => {
                    unresolved_attachment(&reference, transcript, byte_offset, diagnostics);
                    continue;
                }
            };
            if metadata.file_type().is_symlink() {
                unresolved_attachment(&reference, transcript, byte_offset, diagnostics);
                continue;
            }
            let canonical = match fs::canonicalize(&path) {
                Ok(path) => path,
                Err(_) => {
                    unresolved_attachment(&reference, transcript, byte_offset, diagnostics);
                    continue;
                }
            };
            if !metadata.is_file() || !allowed_roots.iter().any(|root| canonical.starts_with(root))
            {
                unresolved_attachment(&reference, transcript, byte_offset, diagnostics);
                continue;
            }
            let paths = attachments.entry(canonical).or_default();
            if !paths.contains(&reference) {
                paths.push(reference);
            }
        }
    }
    Ok(attachments)
}

fn scan_media_references(
    transcript: &Path,
    context: &OperationContext,
) -> Result<Vec<(String, u64)>> {
    let mut file = File::open(transcript).map_err(|error| io_error(transcript, error))?;
    let mut line_file = tempfile::tempfile().map_err(|error| io_error(transcript, error))?;
    let mut buffer = [0_u8; 64 * 1024];
    let mut line_offset = 0_u64;
    let mut absolute_offset = 0_u64;
    let mut references = Vec::new();
    loop {
        context.check_cancelled()?;
        let count = file
            .read(&mut buffer)
            .map_err(|error| io_error(transcript, error))?;
        if count == 0 {
            break;
        }
        let mut segment_start = 0;
        for index in 0..count {
            if buffer[index] == b'\n' {
                line_file
                    .write_all(&buffer[segment_start..=index])
                    .map_err(|error| io_error(transcript, error))?;
                absolute_offset += (index - segment_start + 1) as u64;
                scan_reference_line(&mut line_file, line_offset, &mut references)?;
                line_file
                    .set_len(0)
                    .map_err(|error| io_error(transcript, error))?;
                line_file
                    .seek(SeekFrom::Start(0))
                    .map_err(|error| io_error(transcript, error))?;
                line_offset = absolute_offset;
                segment_start = index + 1;
            }
        }
        if segment_start < count {
            line_file
                .write_all(&buffer[segment_start..count])
                .map_err(|error| io_error(transcript, error))?;
            absolute_offset += (count - segment_start) as u64;
        }
    }
    if absolute_offset > line_offset {
        scan_reference_line(&mut line_file, line_offset, &mut references)?;
    }
    Ok(references)
}

fn scan_reference_line(
    line_file: &mut File,
    byte_offset: u64,
    references: &mut Vec<(String, u64)>,
) -> Result<()> {
    line_file
        .seek(SeekFrom::Start(0))
        .map_err(|error| io_error("temporary discovery line", error))?;
    let Ok(value) = serde_json::from_reader::<_, serde_json::Value>(&mut *line_file) else {
        return Ok(());
    };
    collect_media_refs(&value, byte_offset, references);
    Ok(())
}

fn collect_media_refs(
    value: &serde_json::Value,
    byte_offset: u64,
    output: &mut Vec<(String, u64)>,
) {
    let Some(object) = value.as_object() else {
        return;
    };
    let block_type = object.get("type").and_then(serde_json::Value::as_str);
    if matches!(block_type, Some("image" | "document")) {
        let source = object.get("source").unwrap_or(value);
        for key in ["path", "file_path", "filePath"] {
            if let Some(path) = source.get(key).and_then(serde_json::Value::as_str) {
                output.push((path.to_owned(), byte_offset));
            }
        }
    }
    if block_type == Some("tool_result") {
        if let Some(content) = object.get("content") {
            match content {
                serde_json::Value::Array(items) => {
                    for item in items {
                        collect_media_refs(item, byte_offset, output);
                    }
                }
                serde_json::Value::Object(_) => collect_media_refs(content, byte_offset, output),
                _ => {}
            }
        }
    }
    if let Some(message) = object.get("message") {
        if let Some(content) = message.get("content") {
            match content {
                serde_json::Value::Array(items) => {
                    for item in items {
                        collect_media_refs(item, byte_offset, output);
                    }
                }
                serde_json::Value::Object(_) => collect_media_refs(content, byte_offset, output),
                _ => {}
            }
        }
    }
}

fn unresolved_attachment(
    reference: &str,
    transcript: &Path,
    byte_offset: u64,
    diagnostics: &mut Vec<ImportDiagnostic>,
) {
    diagnostics.push(ImportDiagnostic {
        code: "unresolved_attachment_reference".into(),
        severity: DiagnosticSeverity::Warning,
        message: format!("local attachment reference could not be captured: {reference}"),
        artifact_id: Some(artifact_id(&encode_relative(transcript))),
        byte_offset: Some(byte_offset),
        byte_length: None,
    });
}

fn classify(path: &Path, root: &Path, transcripts: &[PathBuf]) -> ArtifactKind {
    if transcripts.iter().any(|transcript| transcript == path) {
        return ArtifactKind::Transcript;
    }
    if has_extension(path, "jsonl") {
        return ArtifactKind::SubagentTranscript;
    }
    let under_session_dir = path.parent().is_some_and(|parent| {
        parent.file_name().is_some_and(|name| {
            transcripts
                .iter()
                .any(|transcript| transcript.file_stem().is_some_and(|stem| stem == name))
        })
    });
    let is_index = path
        .file_name()
        .is_some_and(|name| name == "sessions-index.json");
    let _ = root;
    if under_session_dir || is_index {
        ArtifactKind::SessionMetadata
    } else {
        // No ambient project/workspace files are selected by discovery.
        ArtifactKind::SessionMetadata
    }
}

fn artifact_id(path: &EncodedPath) -> String {
    let mut hash = Sha256::new();
    hash.update([match path.platform {
        crate::model::PathPlatform::UnixBytes => 1,
        crate::model::PathPlatform::WindowsUtf16Le => 2,
    }]);
    hash.update(path.units_hex.as_bytes());
    format!("artifact:{}", hex(&hash.finalize()))
}

fn has_extension(path: &Path, extension: &str) -> bool {
    path.extension()
        .is_some_and(|value| value.to_string_lossy().eq_ignore_ascii_case(extension))
}

fn relative_display(path: &Path) -> String {
    path.components()
        .map(|component| display(component.as_os_str()))
        .collect::<Vec<_>>()
        .join("/")
}

fn epoch_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}
