use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::Path,
};

use serde_json::Value;

use crate::{
    capture::sha256,
    error::{ImportError, Result, io_error},
    model::{
        ArtifactKind, AssistantMessageGroup, CompletenessState, DiagnosticSeverity, GraphBranch,
        GraphNode, ImportCompleteness, ImportDiagnostic, ImportPhase, ImportProgress, ImportStatus,
        ImportedBlock, ImportedBlockKind, ImportedMessage, MessageKind, OperationContext,
        PackageManifest, PreparedImport, RecordDisposition, RecordOrigin, SourceArtifact,
        SourceRole, ToolPairing, TranscriptGraph, TranscriptMetadata, TranscriptRecord,
    },
};

pub(super) fn normalize(
    captured: &crate::model::CapturedSource,
    context: &OperationContext,
) -> Result<(PreparedImport, Vec<ImportDiagnostic>)> {
    context.check_cancelled()?;
    let work_dir = captured.package_dir.join(".normalizing");
    fs::create_dir(&work_dir).map_err(|error| io_error(&work_dir, error))?;
    let outcome = normalize_with_workdir(captured, &work_dir, context);
    let _ = fs::remove_dir_all(&work_dir);
    outcome
}

fn normalize_with_workdir(
    captured: &crate::model::CapturedSource,
    work_dir: &Path,
    context: &OperationContext,
) -> Result<(PreparedImport, Vec<ImportDiagnostic>)> {
    let mut records = Vec::new();
    let mut messages = Vec::new();
    let mut diagnostics = Vec::new();
    let mut missing = Vec::new();
    let mut saw_context_boundary = false;
    let mut message_semantics_complete = true;

    for artifact in captured.manifest.artifacts.iter().filter(|artifact| {
        matches!(
            artifact.kind,
            ArtifactKind::Transcript | ArtifactKind::SubagentTranscript
        )
    }) {
        let mut artifact_ordinal = 0_u64;
        context.check_cancelled()?;
        stream_artifact_lines(
            captured,
            artifact,
            work_dir,
            |offset, length, line_path| {
                context.check_cancelled()?;
                let line_ordinal = artifact_ordinal;
                artifact_ordinal = artifact_ordinal.saturating_add(1);
                let mut raw_file =
                    File::open(line_path).map_err(|error| io_error(line_path, error))?;
                let parsed = serde_json::from_reader::<_, Value>(&mut raw_file);
                let record_id =
                    stable_record_id(&artifact.artifact_id, offset, length, line_ordinal);
                let origin = RecordOrigin {
                    artifact_id: artifact.artifact_id.clone(),
                    byte_offset: offset,
                    byte_length: length,
                    ordinal: line_ordinal,
                    content_path: None,
                };
                let value = match parsed {
                    Ok(value) if value.is_object() => value,
                    Ok(_) => {
                        let diagnostic = ImportDiagnostic {
                            code: "record_not_object".into(),
                            severity: DiagnosticSeverity::Warning,
                            message: "transcript record is valid JSON but is not an object".into(),
                            artifact_id: Some(artifact.artifact_id.clone()),
                            byte_offset: Some(offset),
                            byte_length: Some(length),
                        };
                        diagnostics.push(diagnostic);
                        message_semantics_complete = false;
                        records.push(TranscriptRecord {
                            record_id,
                            origin,
                            raw_type: None,
                            disposition: RecordDisposition::Opaque,
                            message_index: None,
                            metadata: None,
                            diagnostics: vec!["record_not_object".into()],
                        });
                        return Ok(());
                    }
                    Err(error) => {
                        let diagnostic = ImportDiagnostic {
                            code: "malformed_jsonl_record".into(),
                            severity: DiagnosticSeverity::Error,
                            message: format!("JSONL record could not be parsed: {error}"),
                            artifact_id: Some(artifact.artifact_id.clone()),
                            byte_offset: Some(offset),
                            byte_length: Some(length),
                        };
                        diagnostics.push(diagnostic);
                        message_semantics_complete = false;
                        records.push(TranscriptRecord {
                            record_id,
                            origin,
                            raw_type: None,
                            disposition: RecordDisposition::Damaged,
                            message_index: None,
                            metadata: None,
                            diagnostics: vec!["malformed_jsonl_record".into()],
                        });
                        return Ok(());
                    }
                };
                let raw_type = string(&value, "type").map(str::to_owned);
                let message_value = value.get("message").filter(|item| item.is_object());
                let disposition;
                let mut message_index = None;
                let mut record_diagnostics = Vec::new();

                if let Some(message_value) = message_value {
                    if let Some(message) = normalize_message(
                        &value,
                        message_value,
                        &artifact.artifact_id,
                        &record_id,
                        &origin,
                        (artifact.kind == ArtifactKind::SubagentTranscript)
                            .then(|| artifact.display_path.clone()),
                        &mut diagnostics,
                    ) {
                        if message.blocks.iter().any(contains_opaque_block) {
                            message_semantics_complete = false;
                            record_diagnostics.push("unknown_content_block".into());
                            diagnostics.push(ImportDiagnostic {
                                code: "unknown_content_block".into(),
                                severity: DiagnosticSeverity::Warning,
                                message: format!(
                                    "message {record_id} contains an unrecognized content block"
                                ),
                                artifact_id: Some(artifact.artifact_id.clone()),
                                byte_offset: Some(origin.byte_offset),
                                byte_length: Some(origin.byte_length),
                            });
                        }
                        if message.is_compact_summary
                            || raw_type.as_deref().is_some_and(is_context_marker)
                            || contains_context_replacement(&value)
                        {
                            saw_context_boundary = true;
                        }
                        if message.blocks.iter().any(|block| {
                            block.kind == ImportedBlockKind::Image
                                || block.kind == ImportedBlockKind::Document
                        }) {
                            for block in message.blocks.iter().filter(|block| {
                                matches!(
                                    block.kind,
                                    ImportedBlockKind::Image | ImportedBlockKind::Document
                                )
                            }) {
                                if let Some(path) = block
                                    .source_view
                                    .get("source")
                                    .and_then(|source| {
                                        source
                                            .get("path")
                                            .or_else(|| source.get("file_path"))
                                            .or_else(|| source.get("filePath"))
                                    })
                                    .and_then(Value::as_str)
                                {
                                    let captured_reference =
                                        captured.manifest.artifacts.iter().any(|artifact| {
                                            artifact.kind == ArtifactKind::Attachment
                                                && artifact
                                                    .referenced_paths
                                                    .iter()
                                                    .any(|reference| reference == path)
                                        });
                                    if !captured_reference {
                                        missing.push(format!(
                                            "unresolved media reference {path} at {record_id}"
                                        ));
                                    }
                                }
                            }
                        }
                        disposition = RecordDisposition::Message;
                        message_index = Some(messages.len());
                        messages.push(message);
                    } else {
                        disposition = RecordDisposition::Opaque;
                        record_diagnostics.push("unrecognized_message_role_or_shape".into());
                        message_semantics_complete = false;
                    }
                } else if known_metadata_type(raw_type.as_deref()) {
                    disposition = RecordDisposition::Metadata;
                } else {
                    let has_message_fields = value.get("uuid").is_some()
                        || value.get("parentUuid").is_some()
                        || value.get("logicalParentUuid").is_some();
                    record_diagnostics.push(if has_message_fields {
                        "unknown_record_may_affect_context".into()
                    } else {
                        "unknown_metadata_record".into()
                    });
                    if has_message_fields {
                        message_semantics_complete = false;
                        saw_context_boundary = true;
                    }
                    disposition = RecordDisposition::Opaque;
                }

                if raw_type.as_deref() == Some("user")
                    && message_index
                        .is_some_and(|index| messages[index].kind == MessageKind::ToolResultOnly)
                {
                    // Kept as an imported message with its source role, but marked
                    // so consumers do not treat it as a new user question.
                    record_diagnostics.push("user_record_is_tool_result".into());
                }
                records.push(TranscriptRecord {
                    record_id,
                    origin,
                    raw_type,
                    disposition,
                    message_index,
                    metadata: if message_index.is_none() {
                        Some(value)
                    } else {
                        None
                    },
                    diagnostics: record_diagnostics,
                });
                Ok(())
            },
            context,
        )?;
    }

    // Preserve physical non-transcript files as metadata records with a
    // source range to their artifact. They are never interpreted as messages.
    for artifact in captured
        .manifest
        .artifacts
        .iter()
        .filter(|artifact| artifact.kind == ArtifactKind::SessionMetadata)
    {
        context.check_cancelled()?;
        let file_ordinal = 0;
        let record_id =
            stable_record_id(&artifact.artifact_id, 0, artifact.byte_length, file_ordinal);
        let metadata_value = if artifact
            .display_path
            .to_ascii_lowercase()
            .ends_with(".json")
        {
            let mut stream = ArtifactStream::new(captured, artifact)?;
            match serde_json::from_reader::<_, Value>(&mut stream) {
                Ok(value) => value,
                Err(error) => {
                    diagnostics.push(ImportDiagnostic {
                        code: "malformed_session_metadata".into(),
                        severity: DiagnosticSeverity::Warning,
                        message: format!("metadata JSON could not be parsed: {error}"),
                        artifact_id: Some(artifact.artifact_id.clone()),
                        byte_offset: None,
                        byte_length: Some(artifact.byte_length),
                    });
                    serde_json::json!({"display_path": artifact.display_path})
                }
            }
        } else {
            serde_json::json!({
                "display_path": artifact.display_path,
                "kind": artifact.kind,
                "sha256": artifact.sha256,
                "byte_length": artifact.byte_length,
            })
        };
        records.push(TranscriptRecord {
            record_id,
            origin: RecordOrigin {
                artifact_id: artifact.artifact_id.clone(),
                byte_offset: 0,
                byte_length: artifact.byte_length,
                ordinal: file_ordinal,
                content_path: None,
            },
            raw_type: Some("session_metadata_file".into()),
            disposition: RecordDisposition::Metadata,
            message_index: None,
            metadata: Some(metadata_value),
            diagnostics: Vec::new(),
        });
    }

    let graph = build_graph(&mut records, &messages, &mut diagnostics);
    let metadata = build_metadata(&records, &messages, &captured.manifest);
    for pair in &graph.tool_pairs {
        if pair.result_record_ids.is_empty() {
            missing.push(format!(
                "tool result missing for invocation {}",
                pair.tool_id
            ));
            diagnostics.push(ImportDiagnostic {
                code: "tool_result_missing".into(),
                severity: DiagnosticSeverity::Warning,
                message: format!(
                    "no result record found for tool invocation {}",
                    pair.tool_id
                ),
                artifact_id: None,
                byte_offset: None,
                byte_length: None,
            });
        }
    }
    if !missing.is_empty() {
        diagnostics.extend(missing.iter().map(|message| ImportDiagnostic {
            code: "unresolved_external_attachment".into(),
            severity: DiagnosticSeverity::Warning,
            message: message.clone(),
            artifact_id: None,
            byte_offset: None,
            byte_length: None,
        }));
    }
    if !missing.is_empty()
        || diagnostics.iter().any(|diagnostic| {
            matches!(
                diagnostic.code.as_str(),
                "parent_cycle"
                    | "dangling_parent_uuid"
                    | "duplicate_parent_uuid"
                    | "duplicate_source_uuid"
            )
        })
    {
        message_semantics_complete = false;
    }
    let has_context_markers = saw_context_boundary
        || records.iter().any(|record| {
            record.raw_type.as_deref().is_some_and(is_context_marker)
                || record
                    .diagnostics
                    .iter()
                    .any(|code| code == "unknown_record_may_affect_context")
        })
        || diagnostics.iter().any(|diagnostic| {
            matches!(
                diagnostic.code.as_str(),
                "parent_cycle"
                    | "dangling_parent_uuid"
                    | "duplicate_parent_uuid"
                    | "duplicate_source_uuid"
                    | "tool_result_missing"
            )
        });
    let completeness = ImportCompleteness {
        // Raw integrity is verified by the package verifier, not inferred from
        // successful JSON parsing.
        raw_bytes_complete: true,
        messages_complete: message_semantics_complete,
        context_complete: if has_context_markers {
            CompletenessState::Unknown
        } else if message_semantics_complete {
            CompletenessState::Complete
        } else {
            CompletenessState::Incomplete
        },
        missing_items: missing,
    };
    let status = if completeness.messages_complete
        && completeness.context_complete == CompletenessState::Complete
    {
        ImportStatus::Complete
    } else {
        ImportStatus::Incomplete
    };
    let mut prepared = PreparedImport {
        status,
        source: captured.manifest.source.clone(),
        package_hash: captured.manifest.package_hash.clone(),
        normalizer_version: crate::model::NORMALIZER_VERSION.into(),
        normalization_hash: String::new(),
        records,
        messages,
        graph,
        metadata,
        completeness,
        diagnostics: diagnostics.clone(),
    };
    prepared.normalization_hash = normalization_hash(&prepared)?;
    Ok((prepared, diagnostics))
}

fn normalize_message(
    record: &Value,
    message: &Value,
    artifact_id: &str,
    record_id: &str,
    origin: &RecordOrigin,
    subagent_participant: Option<String>,
    diagnostics: &mut Vec<ImportDiagnostic>,
) -> Option<ImportedMessage> {
    let role = string(message, "role")?;
    let role = match role {
        "user" => SourceRole::User,
        "assistant" => SourceRole::Assistant,
        "system" => SourceRole::System,
        other => SourceRole::Other(other.to_owned()),
    };
    let content = message.get("content");
    let blocks = match content {
        Some(Value::String(text)) => vec![ImportedBlock {
            kind: ImportedBlockKind::Text,
            origin: block_origin(origin, "message.content"),
            source_type: Some("string".into()),
            text: Some(text.clone()),
            signature: None,
            tool_id: None,
            tool_name: None,
            tool_input: None,
            tool_result_for: None,
            is_error: None,
            children: Vec::new(),
            source_view: Value::String(text.clone()),
        }],
        Some(Value::Array(values)) => values
            .iter()
            .enumerate()
            .map(|(index, block)| normalize_block(block, origin, index))
            .collect(),
        Some(Value::Null) | None => Vec::new(),
        Some(other) => {
            diagnostics.push(ImportDiagnostic {
                code: "unrecognized_content_shape".into(),
                severity: DiagnosticSeverity::Warning,
                message: format!(
                    "message content has unsupported JSON shape at record {record_id}"
                ),
                artifact_id: Some(artifact_id.to_owned()),
                byte_offset: Some(origin.byte_offset),
                byte_length: Some(origin.byte_length),
            });
            vec![ImportedBlock {
                kind: ImportedBlockKind::Opaque,
                origin: block_origin(origin, "message.content"),
                source_type: Some(json_kind(other).into()),
                text: None,
                signature: None,
                tool_id: None,
                tool_name: None,
                tool_input: None,
                tool_result_for: None,
                is_error: None,
                children: Vec::new(),
                source_view: other.clone(),
            }]
        }
    };
    let result_only = !blocks.is_empty()
        && blocks
            .iter()
            .all(|block| block.kind == ImportedBlockKind::ToolResult);
    let kind = if result_only {
        MessageKind::ToolResultOnly
    } else if blocks
        .iter()
        .any(|block| block.kind == ImportedBlockKind::ToolResult)
        && blocks
            .iter()
            .any(|block| block.kind != ImportedBlockKind::ToolResult)
    {
        MessageKind::Mixed
    } else {
        MessageKind::Conversation
    };
    let timestamp_raw = string(record, "timestamp").map(str::to_owned);
    let (timestamp_ms, timestamp_error) =
        timestamp_raw
            .as_deref()
            .map(parse_timestamp)
            .map_or((None, None), |result| match result {
                Ok(value) => (Some(value), None),
                Err(error) => (None, Some(error)),
            });
    Some(ImportedMessage {
        record_id: record_id.to_owned(),
        origin: origin.clone(),
        role,
        kind,
        source_message_id: message.get("id").and_then(Value::as_str).map(str::to_owned),
        source_uuid: string(record, "uuid").map(str::to_owned),
        timestamp_raw,
        timestamp_ms,
        timestamp_error,
        model: string(message, "model").map(str::to_owned),
        stop_reason: string(message, "stop_reason")
            .or_else(|| string(message, "stopReason"))
            .map(str::to_owned),
        request_id: string(message, "requestId")
            .or_else(|| string(message, "request_id"))
            .map(str::to_owned),
        usage: message.get("usage").cloned(),
        parent_uuid: string(record, "parentUuid").map(str::to_owned),
        logical_parent_uuid: string(record, "logicalParentUuid").map(str::to_owned),
        is_sidechain: record
            .get("isSidechain")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        is_meta: record
            .get("isMeta")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        is_visible_in_transcript_only: record
            .get("isVisibleInTranscriptOnly")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        is_compact_summary: record
            .get("isCompactSummary")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        participant: string(record, "agentId")
            .or_else(|| string(record, "agent_id"))
            .map(str::to_owned)
            .or(subagent_participant),
        blocks,
        source_view: record.clone(),
    })
}

fn normalize_block(block: &Value, record_origin: &RecordOrigin, index: usize) -> ImportedBlock {
    normalize_block_at(block, record_origin, &format!("message.content[{index}]"))
}

fn normalize_block_at(
    block: &Value,
    record_origin: &RecordOrigin,
    content_path: &str,
) -> ImportedBlock {
    if let Value::String(text) = block {
        return ImportedBlock {
            kind: ImportedBlockKind::Text,
            origin: block_origin(record_origin, content_path),
            source_type: Some("string".into()),
            text: Some(text.clone()),
            signature: None,
            tool_id: None,
            tool_name: None,
            tool_input: None,
            tool_result_for: None,
            is_error: None,
            children: Vec::new(),
            source_view: block.clone(),
        };
    }
    let source_type = string(block, "type").map(str::to_owned);
    let kind = match source_type.as_deref() {
        Some("text") => ImportedBlockKind::Text,
        Some("thinking") => ImportedBlockKind::Thinking,
        Some("redacted_thinking") | Some("redacted-thinking") => {
            ImportedBlockKind::RedactedThinking
        }
        Some("tool_use") => ImportedBlockKind::ToolUse,
        Some("tool_result") => ImportedBlockKind::ToolResult,
        Some("image") => ImportedBlockKind::Image,
        Some("document") => ImportedBlockKind::Document,
        _ => ImportedBlockKind::Opaque,
    };
    let children = if kind == ImportedBlockKind::ToolResult {
        match block.get("content") {
            Some(Value::Array(values)) => values
                .iter()
                .enumerate()
                .map(|(index, child)| {
                    normalize_block_at(
                        child,
                        record_origin,
                        &format!("{content_path}.content[{index}]"),
                    )
                })
                .collect(),
            Some(Value::String(text)) => vec![normalize_block_at(
                &Value::String(text.clone()),
                record_origin,
                &format!("{content_path}.content"),
            )],
            _ => Vec::new(),
        }
    } else {
        Vec::new()
    };
    ImportedBlock {
        kind,
        origin: block_origin(record_origin, content_path),
        source_type,
        text: string(block, "text")
            .or_else(|| string(block, "thinking"))
            .map(str::to_owned),
        signature: string(block, "signature").map(str::to_owned),
        tool_id: string(block, "id").map(str::to_owned),
        tool_name: string(block, "name").map(str::to_owned),
        tool_input: block.get("input").cloned(),
        tool_result_for: string(block, "tool_use_id").map(str::to_owned),
        is_error: block.get("is_error").and_then(Value::as_bool),
        children,
        source_view: block.clone(),
    }
}

fn build_graph(
    records: &mut [TranscriptRecord],
    messages: &[ImportedMessage],
    diagnostics: &mut Vec<ImportDiagnostic>,
) -> TranscriptGraph {
    let message_by_record: HashMap<&str, &ImportedMessage> = messages
        .iter()
        .map(|message| (message.record_id.as_str(), message))
        .collect();
    let positions: HashMap<String, usize> = records
        .iter()
        .enumerate()
        .map(|(index, record)| (record.record_id.clone(), index))
        .collect();
    let mut uuids: HashMap<String, Vec<String>> = HashMap::new();
    let mut links = Vec::new();
    for record in records.iter() {
        let message = message_by_record.get(record.record_id.as_str()).copied();
        let source = message
            .map(|message| &message.source_view)
            .or(record.metadata.as_ref());
        let uuid = message
            .and_then(|message| message.source_uuid.clone())
            .or_else(|| source.and_then(|value| string(value, "uuid").map(str::to_owned)));
        if let Some(uuid) = &uuid {
            uuids
                .entry(uuid.clone())
                .or_default()
                .push(record.record_id.clone());
        }
        let parent_uuid = message
            .and_then(|message| message.parent_uuid.clone())
            .or_else(|| source.and_then(|value| string(value, "parentUuid").map(str::to_owned)));
        let logical_parent_uuid = message
            .and_then(|message| message.logical_parent_uuid.clone())
            .or_else(|| {
                source.and_then(|value| string(value, "logicalParentUuid").map(str::to_owned))
            });
        let is_sidechain = message
            .map(|message| message.is_sidechain)
            .or_else(|| source.and_then(|value| value.get("isSidechain").and_then(Value::as_bool)))
            .unwrap_or(false);
        let participant = message
            .and_then(|message| message.participant.clone())
            .or_else(|| source.and_then(|value| string(value, "agentId").map(str::to_owned)));
        let is_progress = record.raw_type.as_deref() == Some("progress");
        if uuid.is_some() || message.is_some() {
            links.push((
                record.record_id.clone(),
                record.origin.clone(),
                message.is_some(),
                parent_uuid,
                logical_parent_uuid,
                is_progress,
                is_sidechain,
                participant,
            ));
        }
    }
    let mut duplicate_record_ids = HashSet::new();
    for (uuid, record_ids) in &uuids {
        if record_ids.len() > 1 {
            duplicate_record_ids.extend(record_ids.iter().cloned());
            diagnostics.push(ImportDiagnostic {
                code: "duplicate_source_uuid".into(),
                severity: DiagnosticSeverity::Warning,
                message: format!(
                    "source UUID {uuid} belongs to {} physical records",
                    record_ids.len()
                ),
                artifact_id: None,
                byte_offset: None,
                byte_length: None,
            });
        }
    }
    for record in records.iter_mut() {
        if duplicate_record_ids.contains(&record.record_id) {
            record.disposition = RecordDisposition::Conflict;
        }
    }
    let mut nodes = Vec::new();
    let mut parent_map = HashMap::new();
    for (
        record_id,
        origin,
        is_message,
        parent_uuid,
        logical_parent_uuid,
        is_progress,
        is_sidechain,
        participant,
    ) in links
    {
        let physical = resolve_parent(
            parent_uuid.as_deref(),
            &uuids,
            &positions,
            &record_id,
            diagnostics,
            &origin,
        );
        let logical = resolve_parent(
            logical_parent_uuid.as_deref(),
            &uuids,
            &positions,
            &record_id,
            diagnostics,
            &origin,
        );
        parent_map.insert(record_id.clone(), physical.clone());
        nodes.push(GraphNode {
            record_id,
            physical_parent: physical,
            logical_parent: logical,
            is_message,
            is_progress,
            is_sidechain,
            participant,
        });
    }
    detect_cycles(&parent_map, records, diagnostics);

    // Progress records remain in the physical graph but do not become
    // conversation leaves. Bridge through them when finding message branches.
    let message_ids = nodes
        .iter()
        .filter(|node| node.is_message)
        .map(|node| node.record_id.clone())
        .collect::<HashSet<_>>();
    let mut conversation_children = HashSet::new();
    for message_id in &message_ids {
        let mut cursor = parent_map.get(message_id).cloned().flatten();
        let mut seen = HashSet::new();
        while let Some(parent_id) = cursor {
            if !seen.insert(parent_id.clone()) {
                break;
            }
            if message_ids.contains(&parent_id) {
                conversation_children.insert(parent_id);
                break;
            }
            cursor = parent_map.get(&parent_id).cloned().flatten();
        }
    }

    let mut branches = Vec::new();
    let mut leaves = nodes
        .iter()
        .filter(|node| {
            node.is_message && !node.is_progress && !conversation_children.contains(&node.record_id)
        })
        .collect::<Vec<_>>();
    leaves.sort_by_key(|node| positions.get(&node.record_id).copied().unwrap_or(0));
    let mut main_branch_leaf = None;
    for leaf in leaves {
        let mut chain = Vec::new();
        let mut cursor = Some(leaf.record_id.clone());
        let mut seen = HashSet::new();
        while let Some(record_id) = cursor {
            if !seen.insert(record_id.clone()) {
                break;
            }
            chain.push(record_id.clone());
            cursor = parent_map.get(&record_id).cloned().flatten();
        }
        chain.reverse();
        let participant = leaf.participant.clone();
        if !leaf.is_sidechain && participant.is_none() {
            main_branch_leaf = Some(leaf.record_id.clone());
        }
        branches.push(GraphBranch {
            leaf_record_id: leaf.record_id.clone(),
            record_ids: chain,
            participant,
        });
    }

    let mut assistant_groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut tool_uses: BTreeMap<String, String> = BTreeMap::new();
    let mut tool_results: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for message in messages {
        if message.role == SourceRole::Assistant {
            if let Some(id) = &message.source_message_id {
                assistant_groups
                    .entry(id.clone())
                    .or_default()
                    .push(message.record_id.clone());
            }
        }
        for block in &message.blocks {
            if block.kind == ImportedBlockKind::ToolUse {
                if let Some(id) = block.tool_id.as_ref() {
                    tool_uses.insert(id.clone(), message.record_id.clone());
                }
            }
            if block.kind == ImportedBlockKind::ToolResult {
                if let Some(id) = block.tool_result_for.as_ref() {
                    tool_results
                        .entry(id.clone())
                        .or_default()
                        .push(message.record_id.clone());
                }
            }
        }
    }
    let assistant_groups = assistant_groups
        .into_iter()
        .map(|(source_message_id, record_ids)| AssistantMessageGroup {
            source_message_id,
            record_ids,
        })
        .collect();
    let tool_pairs = tool_uses
        .into_iter()
        .map(|(tool_id, tool_use_record_id)| ToolPairing {
            result_record_ids: tool_results.remove(&tool_id).unwrap_or_default(),
            tool_id,
            tool_use_record_id,
        })
        .collect();
    for (tool_id, result_ids) in tool_results {
        diagnostics.push(ImportDiagnostic {
            code: "orphan_tool_result".into(),
            severity: DiagnosticSeverity::Warning,
            message: format!(
                "tool results have no matching invocation {tool_id}: {}",
                result_ids.join(", ")
            ),
            artifact_id: None,
            byte_offset: None,
            byte_length: None,
        });
    }
    TranscriptGraph {
        nodes,
        branches,
        main_branch_leaf,
        assistant_groups,
        tool_pairs,
    }
}

fn resolve_parent(
    parent_uuid: Option<&str>,
    uuids: &HashMap<String, Vec<String>>,
    positions: &HashMap<String, usize>,
    record_id: &str,
    diagnostics: &mut Vec<ImportDiagnostic>,
    origin: &RecordOrigin,
) -> Option<String> {
    let uuid = parent_uuid?;
    let Some(candidates) = uuids.get(uuid) else {
        diagnostics.push(ImportDiagnostic {
            code: "dangling_parent_uuid".into(),
            severity: DiagnosticSeverity::Warning,
            message: format!("parent UUID {uuid} is unresolved for {record_id}"),
            artifact_id: Some(origin.artifact_id.clone()),
            byte_offset: Some(origin.byte_offset),
            byte_length: Some(origin.byte_length),
        });
        return None;
    };
    if candidates.len() > 1 {
        diagnostics.push(ImportDiagnostic {
            code: "duplicate_parent_uuid".into(),
            severity: DiagnosticSeverity::Warning,
            message: format!(
                "parent UUID {uuid} resolves to {} records",
                candidates.len()
            ),
            artifact_id: Some(origin.artifact_id.clone()),
            byte_offset: Some(origin.byte_offset),
            byte_length: Some(origin.byte_length),
        });
    }
    let current_position = positions.get(record_id).copied().unwrap_or(0);
    candidates
        .iter()
        .filter_map(|candidate| {
            positions
                .get(candidate)
                .map(|position| (*position, candidate))
        })
        .filter(|(position, candidate)| *candidate != record_id && *position < current_position)
        .max_by_key(|(position, _)| *position)
        .map(|(_, candidate)| candidate.clone())
        .or_else(|| {
            candidates
                .iter()
                .find(|candidate| candidate.as_str() != record_id)
                .cloned()
        })
}

fn detect_cycles(
    parents: &HashMap<String, Option<String>>,
    records: &[TranscriptRecord],
    diagnostics: &mut Vec<ImportDiagnostic>,
) {
    let mut globally_seen = HashSet::new();
    for start in parents.keys() {
        if globally_seen.contains(start) {
            continue;
        }
        let mut path = Vec::new();
        let mut local = HashSet::new();
        let mut cursor = Some(start.clone());
        while let Some(current) = cursor {
            if !local.insert(current.clone()) {
                let origin = records
                    .iter()
                    .find(|record| record.record_id == current)
                    .map(|record| &record.origin);
                diagnostics.push(ImportDiagnostic {
                    code: "parent_cycle".into(),
                    severity: DiagnosticSeverity::Error,
                    message: format!("parent chain contains a cycle at {current}"),
                    artifact_id: origin.map(|value| value.artifact_id.clone()),
                    byte_offset: origin.map(|value| value.byte_offset),
                    byte_length: origin.map(|value| value.byte_length),
                });
                break;
            }
            globally_seen.insert(current.clone());
            path.push(current.clone());
            cursor = parents.get(&current).cloned().flatten();
        }
    }
}

fn stream_artifact_lines(
    captured: &crate::model::CapturedSource,
    artifact: &SourceArtifact,
    work_dir: &Path,
    mut consume: impl FnMut(u64, u64, &Path) -> Result<()>,
    context: &OperationContext,
) -> Result<()> {
    let mut stream = ArtifactStream::new(captured, artifact)?;
    let mut input_buffer = [0_u8; 64 * 1024];
    let mut line_file: Option<(PathBufHolder, File)> = None;
    let mut line_number = 0_u64;
    let mut line_start = 0_u64;
    let mut absolute = 0_u64;
    loop {
        context.check_cancelled()?;
        let count = stream
            .read(&mut input_buffer)
            .map_err(|error| io_error("package artifact", error))?;
        if count == 0 {
            break;
        }
        let mut segment_start = 0;
        for index in 0..count {
            if input_buffer[index] == b'\n' {
                write_line_segment(
                    work_dir,
                    &mut line_file,
                    line_number,
                    &input_buffer[segment_start..=index],
                )?;
                absolute += (index - segment_start + 1) as u64;
                let (path, file) = line_file.take().expect("line file created above");
                file.sync_all().map_err(|error| io_error(&path.0, error))?;
                consume(line_start, absolute - line_start, &path.0)?;
                context.report(ImportProgress {
                    phase: ImportPhase::Normalize,
                    artifact_id: Some(artifact.artifact_id.clone()),
                    current_bytes: absolute,
                    total_bytes: Some(artifact.byte_length),
                });
                fs::remove_file(&path.0).map_err(|error| io_error(&path.0, error))?;
                line_number = line_number.saturating_add(1);
                line_start = absolute;
                segment_start = index + 1;
            }
        }
        if segment_start < count {
            write_line_segment(
                work_dir,
                &mut line_file,
                line_number,
                &input_buffer[segment_start..count],
            )?;
            absolute += (count - segment_start) as u64;
        }
    }
    if let Some((path, file)) = line_file.take() {
        file.sync_all().map_err(|error| io_error(&path.0, error))?;
        consume(line_start, absolute - line_start, &path.0)?;
        context.report(ImportProgress {
            phase: ImportPhase::Normalize,
            artifact_id: Some(artifact.artifact_id.clone()),
            current_bytes: absolute,
            total_bytes: Some(artifact.byte_length),
        });
        fs::remove_file(&path.0).map_err(|error| io_error(&path.0, error))?;
    }
    Ok(())
}

struct PathBufHolder(std::path::PathBuf);

fn write_line_segment(
    work_dir: &Path,
    line_file: &mut Option<(PathBufHolder, File)>,
    line_number: u64,
    bytes: &[u8],
) -> Result<()> {
    if bytes.is_empty() {
        return Ok(());
    }
    if line_file.is_none() {
        let path = work_dir.join(format!("line-{line_number:016x}"));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|error| io_error(&path, error))?;
        *line_file = Some((PathBufHolder(path), file));
    }
    line_file
        .as_mut()
        .expect("line file initialized")
        .1
        .write_all(bytes)
        .map_err(|error| io_error("temporary line", error))
}

struct ArtifactStream<'a> {
    captured: &'a crate::model::CapturedSource,
    artifact: &'a SourceArtifact,
    chunk_index: usize,
    current: Option<File>,
}

impl<'a> ArtifactStream<'a> {
    fn new(
        captured: &'a crate::model::CapturedSource,
        artifact: &'a SourceArtifact,
    ) -> Result<Self> {
        if artifact.chunks.is_empty() && artifact.byte_length != 0 {
            return Err(ImportError::CorruptPackage(format!(
                "artifact {} has no chunks",
                artifact.artifact_id
            )));
        }
        Ok(Self {
            captured,
            artifact,
            chunk_index: 0,
            current: None,
        })
    }
}

impl Read for ArtifactStream<'_> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        loop {
            if let Some(file) = self.current.as_mut() {
                let count = file.read(output)?;
                if count > 0 {
                    return Ok(count);
                }
                self.current = None;
                self.chunk_index += 1;
            }
            let Some(chunk) = self.artifact.chunks.get(self.chunk_index) else {
                return Ok(0);
            };
            let path = self
                .captured
                .package_dir
                .join("objects")
                .join(&chunk.sha256);
            self.current = Some(File::open(path)?);
        }
    }
}

fn block_origin(record: &RecordOrigin, content_path: &str) -> RecordOrigin {
    let mut result = record.clone();
    result.content_path = Some(content_path.to_owned());
    result
}

fn contains_opaque_block(block: &ImportedBlock) -> bool {
    block.kind == ImportedBlockKind::Opaque || block.children.iter().any(contains_opaque_block)
}

fn build_metadata(
    records: &[TranscriptRecord],
    messages: &[ImportedMessage],
    manifest: &PackageManifest,
) -> TranscriptMetadata {
    let mut metadata = TranscriptMetadata::default();
    let primary_artifacts = manifest
        .artifacts
        .iter()
        .filter(|artifact| artifact.kind == ArtifactKind::Transcript)
        .map(|artifact| artifact.artifact_id.as_str())
        .collect::<HashSet<_>>();

    for record in records {
        let Some(value) = record.metadata.as_ref() else {
            continue;
        };
        let session_value = if record.raw_type.as_deref() == Some("session_metadata_file") {
            if let Some(session_key) = manifest.source.source_session_key.as_deref() {
                let Some(matched) = find_session_metadata(value, session_key) else {
                    continue;
                };
                matched
            } else {
                value
            }
        } else {
            value
        };
        if metadata.user_title.is_none() {
            metadata.user_title = find_string(session_value, "customTitle")
                .or_else(|| {
                    (record.raw_type.as_deref() == Some("custom-title"))
                        .then(|| find_string(session_value, "title"))
                        .flatten()
                })
                .or_else(|| {
                    (record.raw_type.as_deref() == Some("session_metadata_file"))
                        .then(|| find_string(session_value, "title"))
                        .flatten()
                })
                .map(str::to_owned);
        }
        if metadata.ai_title.is_none() {
            metadata.ai_title = find_string(session_value, "aiTitle").map(str::to_owned);
        }
        if metadata.cwd.is_none() {
            metadata.cwd = find_string(session_value, "cwd").map(str::to_owned);
        }
        if metadata.git_branch.is_none() {
            metadata.git_branch = find_string(session_value, "gitBranch")
                .or_else(|| find_string(session_value, "branch"))
                .map(str::to_owned);
        }
        if metadata.git_remote.is_none() {
            metadata.git_remote = find_string(session_value, "gitRemote")
                .or_else(|| find_string(session_value, "remote"))
                .map(str::to_owned);
        }
        if metadata.git_commit.is_none() {
            metadata.git_commit = find_string(session_value, "gitCommit")
                .or_else(|| find_string(session_value, "commit"))
                .map(str::to_owned);
        }
        if metadata.repository_root.is_none() {
            metadata.repository_root = find_string(session_value, "repositoryRoot")
                .or_else(|| find_string(session_value, "repoRoot"))
                .map(str::to_owned);
        }
        if metadata.source_program_version.is_none() {
            metadata.source_program_version = find_string(session_value, "claudeCodeVersion")
                .or_else(|| find_string(session_value, "sourceVersion"))
                .or_else(|| find_string(session_value, "version"))
                .map(str::to_owned);
        }
        if metadata.mode.is_none() {
            metadata.mode = find_value(session_value, "permissionMode")
                .or_else(|| find_value(session_value, "permission_mode"))
                .or_else(|| find_value(session_value, "mode"))
                .cloned();
        }
        if record.raw_type.as_deref().is_some_and(|kind| {
            matches!(
                kind,
                "summary" | "compact-summary" | "context-collapse" | "snip"
            )
        }) {
            metadata
                .compression_record_ids
                .push(record.record_id.clone());
        }
        if record.raw_type.as_deref() == Some("summary") {
            metadata.summary_record_ids.push(record.record_id.clone());
        }
        if record.raw_type.as_deref() == Some("content-replacement")
            || contains_context_replacement(value)
        {
            metadata
                .content_replacement_record_ids
                .push(record.record_id.clone());
        }
    }
    for message in messages {
        if !primary_artifacts.contains(message.origin.artifact_id.as_str())
            || message.role != SourceRole::User
            || message.kind == MessageKind::ToolResultOnly
            || message.is_meta
        {
            continue;
        }
        let text = message
            .blocks
            .iter()
            .filter_map(block_text)
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_owned();
        if text.is_empty() {
            continue;
        }
        if metadata.first_question.is_none() {
            metadata.first_question = Some(text.clone());
        }
        metadata.last_question = Some(text);
    }
    metadata.display_title = metadata
        .user_title
        .clone()
        .or_else(|| metadata.ai_title.clone())
        .or_else(|| metadata.first_question.clone());
    metadata
}

fn find_session_metadata<'a>(value: &'a Value, session_key: &str) -> Option<&'a Value> {
    match value {
        Value::Array(items) => items
            .iter()
            .find_map(|item| find_session_metadata(item, session_key)),
        Value::Object(object) => {
            let item_key = object
                .get("sessionId")
                .or_else(|| object.get("session_id"))
                .or_else(|| object.get("id"))
                .and_then(Value::as_str);
            if item_key == Some(session_key) {
                return Some(value);
            }
            object
                .values()
                .find_map(|item| find_session_metadata(item, session_key))
        }
        _ => None,
    }
}

fn find_string<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    find_value(value, key).and_then(Value::as_str)
}

fn find_value<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    match value {
        Value::Object(object) => object
            .get(key)
            .or_else(|| object.values().find_map(|item| find_value(item, key))),
        Value::Array(items) => items.iter().find_map(|item| find_value(item, key)),
        _ => None,
    }
}

fn block_text(block: &ImportedBlock) -> Option<&str> {
    if block.kind == ImportedBlockKind::Text {
        block.text.as_deref()
    } else {
        None
    }
}

fn string<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn known_metadata_type(value: Option<&str>) -> bool {
    matches!(
        value,
        Some(
            "progress"
                | "system"
                | "summary"
                | "custom-title"
                | "custom_title"
                | "queue-operation"
                | "file-history-snapshot"
                | "compact-summary"
                | "content-replacement"
                | "agent-name"
                | "task-notification"
        )
    )
}

fn is_context_marker(value: &str) -> bool {
    matches!(
        value,
        "summary" | "compact-summary" | "content-replacement" | "context-collapse" | "snip"
    )
}

fn contains_context_replacement(value: &Value) -> bool {
    let serialized = serde_json::to_string(value).unwrap_or_default();
    [
        "context-collapse",
        "content-replacement",
        "<snip>",
        "isCompactSummary",
    ]
    .iter()
    .any(|needle| serialized.contains(needle))
}

fn parse_timestamp(value: &str) -> std::result::Result<i64, String> {
    // Accept Unix milliseconds/seconds as numeric strings and RFC3339 UTC
    // timestamps. For RFC3339, parsing is intentionally strict and does not
    // guess local time-zone rules.
    if let Ok(milliseconds) = value.parse::<i64>() {
        return Ok(if milliseconds.unsigned_abs() < 10_000_000_000 {
            milliseconds.saturating_mul(1000)
        } else {
            milliseconds
        });
    }
    parse_rfc3339_utc(value).ok_or_else(|| "unsupported or invalid timestamp format".into())
}

fn parse_rfc3339_utc(value: &str) -> Option<i64> {
    let bytes = value.as_bytes();
    if bytes.len() < 20
        || bytes.get(4) != Some(&b'-')
        || bytes.get(7) != Some(&b'-')
        || !matches!(bytes.get(10), Some(b'T' | b't'))
        || bytes.get(13) != Some(&b':')
        || bytes.get(16) != Some(&b':')
    {
        return None;
    }
    let year = digits(bytes.get(0..4)?)? as i64;
    let month = digits(bytes.get(5..7)?)? as i64;
    let day = digits(bytes.get(8..10)?)? as i64;
    let hour = digits(bytes.get(11..13)?)? as i64;
    let minute = digits(bytes.get(14..16)?)? as i64;
    let second = digits(bytes.get(17..19)?)? as i64;
    let mut zone_start = 19;
    let mut fraction_ms = 0_i64;
    if bytes.get(zone_start) == Some(&b'.') {
        zone_start += 1;
        let fraction_start = zone_start;
        while bytes.get(zone_start).is_some_and(u8::is_ascii_digit) {
            zone_start += 1;
        }
        if zone_start == fraction_start {
            return None;
        }
        let first_three =
            digits(bytes.get(fraction_start..zone_start.min(fraction_start + 3))?)? as i64;
        fraction_ms = match (zone_start - fraction_start).min(3) {
            1 => first_three * 100,
            2 => first_three * 10,
            _ => first_three,
        };
    }
    let zone_offset = match bytes.get(zone_start).copied()? {
        b'Z' | b'z' if zone_start + 1 == bytes.len() => 0,
        sign @ (b'+' | b'-') if zone_start + 6 == bytes.len() => {
            if bytes.get(zone_start + 3) != Some(&b':') {
                return None;
            }
            let hours = digits(bytes.get(zone_start + 1..zone_start + 3)?)? as i64;
            let minutes = digits(bytes.get(zone_start + 4..zone_start + 6)?)? as i64;
            if hours > 23 || minutes > 59 {
                return None;
            }
            let total = hours * 3600 + minutes * 60;
            if sign == b'+' { total } else { -total }
        }
        _ => return None,
    };
    let leap_year = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days_in_month = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap_year => 29,
        2 => 28,
        _ => return None,
    };
    if day < 1 || day > days_in_month || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let days = days_from_civil(year, month, day);
    Some((((days * 24 + hour) * 60 + minute) * 60 + second - zone_offset) * 1000 + fraction_ms)
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let adjusted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * adjusted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146097 + day_of_era - 719468
}

fn digits(bytes: &[u8]) -> Option<u32> {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    bytes.iter().try_fold(0_u32, |value, digit| {
        value.checked_mul(10)?.checked_add(u32::from(*digit - b'0'))
    })
}

fn stable_record_id(artifact_id: &str, offset: u64, length: u64, ordinal: u64) -> String {
    let input = format!("{artifact_id}\0{offset}\0{length}\0{ordinal}");
    format!("record:{}", sha256(input.as_bytes()))
}

fn normalization_hash(prepared: &PreparedImport) -> Result<String> {
    let mut prepared = prepared.clone();
    prepared.normalization_hash.clear();
    let bytes = serde_json::to_vec(&prepared)
        .map_err(|error| ImportError::CorruptPackage(error.to_string()))?;
    Ok(sha256(&bytes))
}

pub(crate) fn compute_normalization_hash(prepared: &PreparedImport) -> Result<String> {
    normalization_hash(prepared)
}

#[cfg(test)]
mod timestamp_tests {
    use super::parse_rfc3339_utc;

    #[test]
    fn parses_fractional_utc_and_numeric_offsets() {
        assert_eq!(parse_rfc3339_utc("1970-01-01T00:00:00.125Z"), Some(125));
        assert_eq!(parse_rfc3339_utc("1970-01-01T01:00:00.5+01:00"), Some(500));
        assert_eq!(parse_rfc3339_utc("1969-12-31T19:00:00-05:00"), Some(0));
    }

    #[test]
    fn rejects_invalid_calendar_dates_and_fractions() {
        assert_eq!(parse_rfc3339_utc("2025-02-29T00:00:00Z"), None);
        assert_eq!(
            parse_rfc3339_utc("2024-02-29T00:00:00Z"),
            Some(1_709_164_800_000)
        );
        assert_eq!(parse_rfc3339_utc("2024-01-01T00:00:00.Z"), None);
        assert_eq!(parse_rfc3339_utc("2024-01-01T00:00:00.2"), None);
    }
}
