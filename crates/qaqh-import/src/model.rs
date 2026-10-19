use std::{path::PathBuf, sync::Arc};

use serde::{Deserialize, Serialize};

pub const PACKAGE_SCHEMA: &str = "qaqh.transcript-import/v1";
pub const NORMALIZER_VERSION: &str = "claude-code/1";
pub const DEFAULT_CHUNK_SIZE: usize = 1024 * 1024;
pub const MAX_CHUNK_SIZE: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct DiscoverRequest {
    pub input: PathBuf,
    pub allowed_roots: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Discovery {
    pub source: SourceDescriptor,
    pub artifacts: Vec<DiscoveredArtifact>,
    pub diagnostics: Vec<ImportDiagnostic>,
    /// Files created after this list was frozen are intentionally excluded.
    pub snapshot_boundary: String,
    #[serde(skip)]
    pub(crate) allowed_roots: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredArtifact {
    pub artifact_id: String,
    pub kind: ArtifactKind,
    pub relative_path: EncodedPath,
    pub display_path: String,
    pub source_location: SourceLocation,
    #[serde(default)]
    pub referenced_paths: Vec<String>,
    #[serde(skip)]
    pub(crate) source_path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncodedPath {
    pub platform: PathPlatform,
    /// Unix paths use raw bytes; Windows paths use little-endian UTF-16 code units.
    pub units_hex: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceLocation {
    /// Stable source-root identity. The encoded path is retained as provenance
    /// and is never used as a package extraction destination.
    pub root: EncodedPath,
    pub relative_path: EncodedPath,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathPlatform {
    UnixBytes,
    WindowsUtf16Le,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Transcript,
    SubagentTranscript,
    SessionMetadata,
    Attachment,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    ClaudeCode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceDescriptor {
    pub source_kind: SourceKind,
    pub adapter_version: String,
    pub source_program_version: Option<String>,
    pub source_session_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureOptions {
    pub chunk_size: usize,
    pub max_total_bytes: Option<u64>,
    pub max_file_bytes: Option<u64>,
}

impl Default for CaptureOptions {
    fn default() -> Self {
        Self {
            chunk_size: DEFAULT_CHUNK_SIZE,
            max_total_bytes: None,
            max_file_bytes: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CaptureRequest {
    pub discovery: Discovery,
    pub package_dir: PathBuf,
    pub options: CaptureOptions,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapturedSource {
    pub package_dir: PathBuf,
    pub manifest: PackageManifest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageManifest {
    pub schema: String,
    pub source: SourceDescriptor,
    pub snapshot_boundary: String,
    pub artifacts: Vec<SourceArtifact>,
    pub package_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceArtifact {
    pub artifact_id: String,
    pub kind: ArtifactKind,
    pub relative_path: EncodedPath,
    pub display_path: String,
    pub source_location: SourceLocation,
    #[serde(default)]
    pub referenced_paths: Vec<String>,
    pub byte_length: u64,
    pub sha256: String,
    pub chunks: Vec<ChunkRef>,
    pub captured_len: u64,
    pub capture: CaptureObservation,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureObservation {
    pub file_identity: String,
    pub modified_before_unix_ns: Option<u128>,
    pub modified_after_unix_ns: Option<u128>,
    pub second_pass_sha256: String,
    pub captured_at_unix_ms: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkRef {
    pub ordinal: u64,
    pub offset: u64,
    pub byte_length: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportDiagnostic {
    pub code: String,
    pub severity: DiagnosticSeverity,
    pub message: String,
    pub artifact_id: Option<String>,
    pub byte_offset: Option<u64>,
    pub byte_length: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordDisposition {
    Message,
    Metadata,
    Opaque,
    Damaged,
    Conflict,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordOrigin {
    pub artifact_id: String,
    pub byte_offset: u64,
    pub byte_length: u64,
    pub ordinal: u64,
    pub content_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptRecord {
    pub record_id: String,
    pub origin: RecordOrigin,
    pub raw_type: Option<String>,
    pub disposition: RecordDisposition,
    pub message_index: Option<usize>,
    pub metadata: Option<serde_json::Value>,
    pub diagnostics: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceRole {
    User,
    Assistant,
    System,
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    Conversation,
    ToolResultOnly,
    Mixed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportedMessage {
    pub record_id: String,
    pub origin: RecordOrigin,
    pub role: SourceRole,
    pub kind: MessageKind,
    pub source_message_id: Option<String>,
    pub source_uuid: Option<String>,
    pub timestamp_raw: Option<String>,
    pub timestamp_ms: Option<i64>,
    pub timestamp_error: Option<String>,
    pub model: Option<String>,
    pub stop_reason: Option<String>,
    pub request_id: Option<String>,
    pub usage: Option<serde_json::Value>,
    pub parent_uuid: Option<String>,
    pub logical_parent_uuid: Option<String>,
    pub is_sidechain: bool,
    pub is_meta: bool,
    pub is_visible_in_transcript_only: bool,
    pub is_compact_summary: bool,
    pub participant: Option<String>,
    pub blocks: Vec<ImportedBlock>,
    /// Original decoded record for fields not yet given a dedicated view.
    pub source_view: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportedBlock {
    pub kind: ImportedBlockKind,
    pub origin: RecordOrigin,
    pub source_type: Option<String>,
    pub text: Option<String>,
    pub signature: Option<String>,
    pub tool_id: Option<String>,
    pub tool_name: Option<String>,
    pub tool_input: Option<serde_json::Value>,
    pub tool_result_for: Option<String>,
    pub is_error: Option<bool>,
    pub children: Vec<ImportedBlock>,
    /// Complete decoded block view. The original file range remains authoritative.
    pub source_view: serde_json::Value,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TranscriptMetadata {
    pub user_title: Option<String>,
    pub ai_title: Option<String>,
    pub display_title: Option<String>,
    pub first_question: Option<String>,
    pub last_question: Option<String>,
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    pub git_remote: Option<String>,
    pub git_commit: Option<String>,
    pub repository_root: Option<String>,
    pub source_program_version: Option<String>,
    pub mode: Option<serde_json::Value>,
    pub summary_record_ids: Vec<String>,
    pub compression_record_ids: Vec<String>,
    pub content_replacement_record_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportedBlockKind {
    Text,
    Thinking,
    RedactedThinking,
    ToolUse,
    ToolResult,
    Image,
    Document,
    Opaque,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptGraph {
    pub nodes: Vec<GraphNode>,
    pub branches: Vec<GraphBranch>,
    pub main_branch_leaf: Option<String>,
    pub assistant_groups: Vec<AssistantMessageGroup>,
    pub tool_pairs: Vec<ToolPairing>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphNode {
    pub record_id: String,
    pub physical_parent: Option<String>,
    pub logical_parent: Option<String>,
    pub is_message: bool,
    pub is_progress: bool,
    pub is_sidechain: bool,
    pub participant: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphBranch {
    pub leaf_record_id: String,
    pub record_ids: Vec<String>,
    pub participant: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantMessageGroup {
    pub source_message_id: String,
    pub record_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolPairing {
    pub tool_id: String,
    pub tool_use_record_id: String,
    pub result_record_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportCompleteness {
    pub raw_bytes_complete: bool,
    pub messages_complete: bool,
    pub context_complete: CompletenessState,
    pub missing_items: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletenessState {
    Complete,
    Incomplete,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportStatus {
    Complete,
    Incomplete,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreparedImport {
    pub status: ImportStatus,
    pub source: SourceDescriptor,
    pub package_hash: String,
    pub normalizer_version: String,
    pub normalization_hash: String,
    pub records: Vec<TranscriptRecord>,
    pub messages: Vec<ImportedMessage>,
    pub graph: TranscriptGraph,
    pub metadata: TranscriptMetadata,
    pub completeness: ImportCompleteness,
    pub diagnostics: Vec<ImportDiagnostic>,
}

#[derive(Debug, Clone)]
pub struct PrepareRequest {
    pub captured: CapturedSource,
}

#[derive(Clone, Default)]
pub struct OperationContext {
    host: Option<Arc<dyn ImportHost>>,
}

impl OperationContext {
    pub fn with_host(host: Arc<dyn ImportHost>) -> Self {
        Self { host: Some(host) }
    }

    pub(crate) fn check_cancelled(&self) -> crate::error::Result<()> {
        if self.host.as_ref().is_some_and(|host| host.is_cancelled()) {
            Err(crate::error::ImportError::Cancelled)
        } else {
            Ok(())
        }
    }

    pub(crate) fn report(&self, progress: ImportProgress) {
        if let Some(host) = &self.host {
            host.on_progress(progress);
        }
    }
}

impl std::fmt::Debug for OperationContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OperationContext")
            .field("has_host", &self.host.is_some())
            .finish()
    }
}

pub trait ImportHost: Send + Sync + 'static {
    fn is_cancelled(&self) -> bool {
        false
    }

    fn on_progress(&self, _progress: ImportProgress) {}
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportProgress {
    pub phase: ImportPhase,
    pub artifact_id: Option<String>,
    pub current_bytes: u64,
    pub total_bytes: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportPhase {
    Discover,
    Capture,
    Normalize,
    Verify,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationReport {
    pub valid: bool,
    pub package_hash: String,
    pub verified_artifacts: usize,
    pub verified_chunks: usize,
    pub normalization_verified: bool,
    pub errors: Vec<String>,
}
