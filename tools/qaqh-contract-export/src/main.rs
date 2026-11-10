use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use qaqh_ringing::{
    CommandTarget, RingingCommand, RingingCommandAck, RingingCommandAckStatus, RingingCommandState,
    RingingCommandStatus, RingingV2ApprovalRespondRequest, RingingV2AskOutcome,
    RingingV2Capabilities, RingingV2ClientKind, RingingV2ClientView, RingingV2CommandAck,
    RingingV2CommandEnvelope, RingingV2CommandResult, RingingV2CommandStatus,
    RingingV2ContentUploadResponse, RingingV2DeviceApprovalKind, RingingV2DeviceChallenge,
    RingingV2DeviceChallenges, RingingV2DriverClaimResponse, RingingV2DriverReleaseResponse,
    RingingV2ExistingResult, RingingV2LeaseRenewResponse, RingingV2OpenRequest,
    RingingV2OpenResponse, RingingV2PendingApprovals, RingingV2PendingInteractionView,
    RingingV2PendingPermissionView, RingingV2ResyncRequired, RingingV2SyncResult,
};
use serde::{Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use ts_rs::{Config, TS};

const FORMAT_VERSION: u32 = 1;
/// 投影**规则**一变才 bump（`export()` 用它做目录所有权判定）。产物集合的变化由
/// `contract_hash` 记账，不归这个版本管 —— 0.2.0 → 0.3.0 是 ADR-0008 把 `schema/`
/// 折进 `generate()` 那一步的预留 bump（它同时会重写每个 C# 文件头部的生成器注释，
/// 240 个文件，属规则 15 的 `doc-only` 面而不是 wire 面）。
const GENERATOR_VERSION: &str = "qaqh-contract-export/0.2.0";

mod account_fixtures;
#[cfg(test)]
mod artifact_tests;
mod client_fixtures;
mod coverage;
// cs_backend must run after the ts-rs type export: it parses the generated
// TypeScript subset and re-encodes the same wire contract as C# sources.
mod cs_backend;
/// ADR-0008 step 2 (measurement, not migration): reconcile the IR (JSON Schema, read off
/// the Rust DTO) against the C# that `cs_backend` already emitted. Writes nothing.
mod ir_diff;
/// ADR-0008 step 1: JSON Schema projection straight off the Rust DTO. Its output is a
/// preview tree, not a contract artifact — and it never parses the TypeScript backend's
/// output, which is the whole point (see `FLOAT_FIELDS` in `cs_backend.rs`).
mod schema_backend;
mod validation;
mod wire_fixtures;

fn validate_source(release: bool) -> Result<(), Box<dyn std::error::Error>> {
    if release {
        let revision = env::var("QAQH_SOURCE_REVISION").unwrap_or_default();
        let dirty = env::var("QAQH_SOURCE_DIRTY").unwrap_or_default();
        if revision.trim().is_empty() || revision == "development" {
            return Err("--release requires QAQH_SOURCE_REVISION".into());
        }
        if dirty != "0" && dirty != "false" {
            return Err("--release requires QAQH_SOURCE_DIRTY=0 or false".into());
        }
    }
    Ok(())
}

#[derive(Serialize)]
struct FileHash<'a> {
    path: &'a str,
    sha256: String,
}

#[derive(Serialize)]
struct Manifest {
    format_version: u32,
    wire_version: u32,
    backend_version: String,
    source_revision: String,
    source_dirty: bool,
    generator_version: &'static str,
    contract_hash: String,
    files: Vec<FileHashOwned>,
}

#[derive(Serialize)]
struct FileHashOwned {
    path: String,
    sha256: String,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("qaqh-contract-export: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1);
    let command = args.next().ok_or(
        "expected export --out <dir>, check --against <dir>, schema-preview --out <dir>, \
         or ir-diff [dir]",
    )?;
    if command == "ir-diff" {
        // 一次性量尺，默认自己生成一份临时 stage（不落产物、不进 manifest、不动金标准）；
        // 也可以 `ir-diff --against <dir>` 复用一棵已导出的树，省一次 generate()。
        return match args.next().as_deref() {
            None => ir_diff_from_fresh_stage(),
            Some("--against") => {
                let target =
                    PathBuf::from(args.next().ok_or("ir-diff --against needs a directory")?);
                if args.next().is_some() {
                    return Err("unexpected trailing argument".into());
                }
                ir_diff::run(&target)
            }
            Some(other) => {
                return Err(format!("ir-diff takes no argument or --against <dir>, got {other:?}")
                    .into());
            }
        };
    }
    let flag = args.next().ok_or("missing output flag")?;
    let target = PathBuf::from(args.next().ok_or("missing directory")?);
    let release = match args.next().as_deref() {
        None => false,
        Some("--release") => true,
        Some(_) => return Err("unexpected argument (only --release is supported)".into()),
    };
    if args.next().is_some() {
        return Err("unexpected trailing argument".into());
    }
    match (command.as_str(), flag.as_str()) {
        ("export", "--out") => export(&target, release),
        ("check", "--against") => check(&target, release),
        ("schema-preview", "--out") => schema_preview(&target, release),
        _ => Err(
            "expected export --out <dir> [--release], check --against <dir> [--release], \
             or schema-preview --out <dir>"
                .into(),
        ),
    }
}

fn export(target: &Path, release: bool) -> Result<(), Box<dyn std::error::Error>> {
    let parent = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    if target.exists() && !target.is_dir() {
        return Err(format!("target exists and is not a directory: {}", target.display()).into());
    }
    if target.exists() {
        let metadata = fs::symlink_metadata(target)?;
        if metadata.file_type().is_symlink() {
            return Err("refusing to replace a symlink target".into());
        }
        let manifest_path = target.join("manifest.json");
        let manifest_bytes = fs::read(&manifest_path)
            .map_err(|_| "existing target is not an exporter-owned contract directory")?;
        let manifest: serde_json::Value = serde_json::from_slice(&manifest_bytes)?;
        if manifest
            .get("generator_version")
            .and_then(serde_json::Value::as_str)
            != Some(GENERATOR_VERSION)
        {
            return Err(
                "refusing to replace a directory not owned by this exporter version".into(),
            );
        }
    }
    fs::create_dir_all(parent)?;
    let stage = staging_path(target);
    if stage.exists() {
        return Err(format!("staging path already exists: {}", stage.display()).into());
    }
    fs::create_dir(&stage)?;
    let result = validate_source(release)
        .and_then(|()| generate(&stage))
        .and_then(|()| {
            validate_layout(&stage)?;
            commit_stage(&stage, target)
        });
    if result.is_err() {
        let _ = fs::remove_dir_all(&stage);
    }
    result
}

fn commit_stage(stage: &Path, target: &Path) -> Result<(), Box<dyn std::error::Error>> {
    if !target.exists() {
        fs::rename(stage, target)?;
        return Ok(());
    }
    let backup = staging_path(target).with_extension("backup");
    if backup.exists() {
        return Err(format!("backup path already exists: {}", backup.display()).into());
    }
    fs::rename(target, &backup)?;
    match fs::rename(stage, target) {
        Ok(()) => {
            fs::remove_dir_all(backup)?;
            Ok(())
        }
        Err(error) => {
            let restore = fs::rename(&backup, target);
            if let Err(restore_error) = restore {
                return Err(format!("commit failed ({error}); restoring previous target also failed ({restore_error})").into());
            }
            Err(error.into())
        }
    }
}

fn check(target: &Path, release: bool) -> Result<(), Box<dyn std::error::Error>> {
    if !target.is_dir() {
        return Err(format!("contract directory does not exist: {}", target.display()).into());
    }
    let parent = env::temp_dir();
    let stage = parent.join(format!(
        "qaqh-contract-check-{}-{}",
        std::process::id(),
        unique_suffix()
    ));
    fs::create_dir(&stage)?;
    let result = validate_source(release)
        .and_then(|()| generate(&stage))
        .and_then(|()| {
            validate_layout(&stage)?;
            compare_trees(&stage, target)?;
            compare_manifest(&stage, target)
        });
    let _ = fs::remove_dir_all(&stage);
    result
}

/// ADR-0008 step 1: emit the JSON Schema preview tree. Deliberately *not* wired into
/// `generate()`: folding it in changes the artifact file set, which is the ADR-0007
/// refreeze path, not a silent side effect of an unrelated commit.
fn schema_preview(target: &Path, release: bool) -> Result<(), Box<dyn std::error::Error>> {
    if release {
        return Err(
            "schema-preview writes no manifest and stamps no source revision; drop --release"
                .into(),
        );
    }
    if target.join("manifest.json").exists() {
        return Err(format!(
            "refusing to write a schema preview into a contract artifact tree: {}",
            target.display()
        )
        .into());
    }
    fs::create_dir_all(target)?;
    let count = schema_backend::write_preview(target)?;
    println!(
        "schema preview: {count} type schemas + schema/index.json at {} (outside manifest.json)",
        target.display()
    );
    Ok(())
}

/// `ir-diff` with no directory: generate once into a temp stage purely to have the
/// `cs/*.cs` side to read. `generate()` + `validate_layout()` are the same calls `check()`
/// makes, so the diagnostic measures the real artifact tree and then throws it away.
fn ir_diff_from_fresh_stage() -> Result<(), Box<dyn std::error::Error>> {
    let stage = env::temp_dir().join(format!(
        "qaqh-contract-ir-diff-{}-{}",
        std::process::id(),
        unique_suffix()
    ));
    fs::create_dir(&stage)?;
    let result = generate(&stage).and_then(|()| {
        validate_layout(&stage)?;
        ir_diff::run(&stage)
    });
    let _ = fs::remove_dir_all(&stage);
    result
}

fn generate(stage: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let types_dir = stage.join("types");
    fs::create_dir_all(&types_dir)?;
    let config = Config::new()
        .with_out_dir(&types_dir)
        .with_large_int("number");
    export_types(&config)?;

    write_index(stage)?;
    write(
        stage,
        "node.ts",
        "export type { DaemonDiscovery } from \"./types/qaqh/DaemonDiscovery\";\n",
    )?;
    write(
        stage,
        "constants.ts",
        &format!(
            "export const RINGING_SCHEMA = {:?} as const;\nexport const RINGING_V2_VERSION = {} as const;\nexport const RINGING_V2_BASE_PATH = {:?} as const;\nexport const CLIENT_SESSION_HEADER = {:?} as const;\nexport const MAX_SAFE_INTEGER = {} as const;\nexport const COMMAND_CONTRACT_REVISION = {:?} as const;\nexport const SESSION_CONTRACT_REVISION = {:?} as const;\n",
            qaqh_ringing::RINGING_SCHEMA,
            qaqh_ringing::RINGING_V2_VERSION,
            qaqh_ringing::RINGING_V2_BASE_PATH,
            qaqh_ringing::CLIENT_SESSION_HEADER,
            qaqh_ringing::MAX_SAFE_INTEGER,
            qaqh_ringing::COMMAND_CONTRACT_REVISION,
            qaqh_ringing::SESSION_CONTRACT_REVISION,
        ),
    )?;
    write_service_map(stage)?;
    fs::create_dir_all(stage.join("fixtures"))?;
    write(
        stage,
        "fixtures/README.md",
        "Synthetic samples serialized and round-trip checked from the canonical Rust wire DTOs by qaqh-contract-export. Handler-composed projection fixtures remain pending. No user data belongs here.\n",
    )?;
    write_fixtures(stage)?;
    coverage::write(stage)?;
    cs_backend::write(stage)?;
    write_manifest(stage)?;
    Ok(())
}

fn write_fixtures(stage: &Path) -> Result<(), Box<dyn std::error::Error>> {
    use qaqh_runtime::ringing::service_methods::{
        ClientsListParams, ConfigLoadParams, DaemonVersionParams, DiagnosticsMemorySnapshotParams,
        ExecSessionListParams, ExecSessionListResult, FsEntry, FsListParams, FsReadParams,
        FsReadResult, PlanItemStatusView, PlanReadItem, PlanReadParams, SessionActivityParams,
        SessionDashboard, SessionDashboardParams,
        SessionDashboardTask, SessionGetActivityParams, SessionListParams, SessionMetaParams,
        SessionToolActivity, SessionTurnParams, SkillCatalogDiagnostic, SkillCatalogEntry,
        SkillsCatalogParams, SkillsCatalogResult, SkillsListToolsParams, TodoCountsSnapshot,
        TodoItemSnapshot, TodoListParams, TodoListSnapshot, TodoStatusParams, TodoStatusSnapshot,
        WorkspaceGetParams, WorkspaceListParams,
    };
    let open_request = RingingV2OpenRequest::new("fixture-client");
    write_fixture(stage, "open-request.json", &open_request)?;

    let command = RingingV2CommandEnvelope::new(
        "cmd-fixture-1",
        CommandTarget::Session {
            session_id: "session-fixture".to_owned(),
        },
        RingingCommand::SessionAttach {},
    )
    .with_client_identity("fixture-client", "client-session-fixture");
    command
        .validate()
        .map_err(|code| format!("invalid generated command fixture: {code}"))?;
    write_fixture(stage, "command-attach.json", &command)?;

    let ack = RingingV2CommandAck {
        command_id: "cmd-fixture-1".to_owned(),
        status: RingingCommandAckStatus::Accepted,
        code: None,
        message: None,
        retry_after_ms: None,
        existing: None,
        result: None,
        operation: None,
        revision: None,
    };
    write_fixture(stage, "command-ack-accepted.json", &ack)?;

    let status = RingingV2CommandStatus {
        command_id: "cmd-fixture-1".to_owned(),
        state: RingingCommandState::Accepted,
        payload_fingerprint: "synthetic-fingerprint".to_owned(),
        terminal_event_id: None,
        error_code: None,
        result: None,
        sync_result: None,
    };
    write_fixture(stage, "command-status-accepted.json", &status)?;

    let capabilities = RingingV2Capabilities {
        subscribe: true,
        interact: true,
        service: true,
        content: true,
        single_stream: true,
        pairing: false,
    };
    let open_response = RingingV2OpenResponse {
        schema: qaqh_ringing::RINGING_SCHEMA.to_owned(),
        version: qaqh_ringing::RINGING_V2_VERSION,
        accepted: true,
        client_session_id: "client-session-fixture".to_owned(),
        server_epoch: "fixture-epoch".to_owned(),
        lease_ttl_ms: 30_000,
        renew_interval_ms: 10_000,
        capabilities,
        // B07:握手即携带命令目录代际。
        command_contract_revision: qaqh_ringing::COMMAND_CONTRACT_REVISION.to_owned(),
        session_contract_revision: qaqh_ringing::SESSION_CONTRACT_REVISION.to_owned(),
    };
    open_response
        .validate()
        .map_err(|code| format!("invalid generated open fixture: {code}"))?;
    write_fixture(stage, "open-response.json", &open_response)?;
    wire_fixtures::write(stage)?;

    let version_params = DaemonVersionParams {};
    write_fixture(stage, "service-daemon-version-params.json", &version_params)?;
    write_fixture(
        stage,
        "service-daemon-version-result.json",
        &env!("CARGO_PKG_VERSION").to_owned(),
    )?;
    // `clients.list` 是 daemon 基础设施查询（活跃 lease 视图）；条目形状即
    // `RingingV2ClientView`，与 devices/pair fixtures 同一合成身份。
    write_fixture(stage, "service-clients-list-params.json", &ClientsListParams {})?;
    write_fixture(
        stage,
        "service-clients-list-result.json",
        &vec![RingingV2ClientView {
            client_session_id: "client-session-fixture".to_owned(),
            instance_key: "device-fixture".to_owned(),
            client_kind: Some(RingingV2ClientKind::Desktop),
            attached_sessions: 1,
            expires_in_ms: 30_000,
        }],
    )?;
    write_fixture(
        stage,
        "service-fs-list-params.json",
        &FsListParams {
            path: "/synthetic/workspace".to_owned(),
            scope_session_id: Some("session-fixture".to_owned()),
        },
    )?;
    write_fixture(
        stage,
        "service-fs-list-result.json",
        &vec![FsEntry {
            name: "README.md".to_owned(),
            path: "/synthetic/workspace/README.md".to_owned(),
            is_dir: false,
            is_file: true,
            size: 42,
            modified_ms: Some(1_700_000_000_000),
        }],
    )?;
    write_fixture(
        stage,
        "service-fs-read-params.json",
        &FsReadParams {
            path: "/synthetic/workspace/README.md".to_owned(),
            max_bytes: Some(4096),
            scope_session_id: Some("session-fixture".to_owned()),
        },
    )?;
    write_fixture(
        stage,
        "service-fs-read-result.json",
        &FsReadResult {
            path: "/synthetic/workspace/README.md".to_owned(),
            size: 42,
            truncated: false,
            content: "Synthetic fixture content.".to_owned(),
        },
    )?;
    write_fixture(
        stage,
        "service-session-list-params.json",
        &SessionListParams { include_archived: false },
    )?;
    write_fixture(
        stage,
        "service-session-list-result.json",
        &vec![session_list_entry_fixture()],
    )?;
    write_fixture(
        stage,
        "service-session-meta-params.json",
        &SessionMetaParams {
            session_id: "0199a0f0-0000-7000-8000-000000000001".to_owned(),
        },
    )?;
    // 单条形状与 session.list 条目同形（前端契约 G2）；null 结果（meta 缺失）
    // 无独立 fixture——TS 侧由 `SessionListEntry | null` 类型表达。
    write_fixture(
        stage,
        "service-session-meta-result.json",
        &session_list_entry_fixture(),
    )?;
    write_fixture(
        stage,
        "service-session-activity-params.json",
        &SessionActivityParams {},
    )?;
    write_fixture(
        stage,
        "service-session-activity-result.json",
        &vec![qaqh_ringing_contract::SessionActivity {
            session_id: "session-fixture".to_owned(),
            state: qaqh_ringing_contract::SessionActivityState::Running,
            turn_id: Some("turn-fixture-1".to_owned()),
            seq: 3,
            updated_at: 1_759_000_060_000,
        }],
    )?;
    write_fixture(
        stage,
        "service-session-get_activity-params.json",
        &SessionGetActivityParams {
            session_id: "0199a0f0-0000-7000-8000-000000000001".to_owned(),
        },
    )?;
    write_fixture(
        stage,
        "service-session-get_activity-result.json",
        &vec![SessionToolActivity {
            tool_name: "fs_read".to_owned(),
            summary: "read synthetic file".to_owned(),
            status: qaqh_types::ToolStatus::Ok,
            finished_at_ms: 42,
            args: "{\"path\":\"/synthetic/workspace/README.md\"}".to_owned(),
        }],
    )?;
    write_fixture(
        stage,
        "service-skills-catalog-params.json",
        &SkillsCatalogParams {
            cwd: Some("/synthetic/workspace".to_owned()),
        },
    )?;
    write_fixture(
        stage,
        "service-skills-catalog-result.json",
        &SkillsCatalogResult {
            skills: vec![SkillCatalogEntry {
                name: "qaqh-core".to_owned(),
                description: "synthetic skill".to_owned(),
                scope: "user".to_owned(),
                origin: "user".to_owned(),
                path: "/synthetic/skills/qaqh-core".to_owned(),
            }],
            diagnostics: vec![SkillCatalogDiagnostic {
                severity: "warning".to_owned(),
                path: "/synthetic/skills/broken-skill".to_owned(),
                message: "synthetic diagnostic".to_owned(),
            }],
        },
    )?;
    write_fixture(
        stage,
        "session-create-result.json",
        &qaqh_ringing::SessionCreateResult {
            session_id: "0199a0f0-0000-7000-8000-000000000001".to_owned(),
        },
    )?;
    let workspace_session_id = "0199a0f0-0000-7000-8000-000000000001".to_owned();
    write_fixture(
        stage,
        "service-workspace-get-params.json",
        &WorkspaceGetParams {
            session_id: workspace_session_id.clone(),
        },
    )?;
    write_fixture(
        stage,
        "service-workspace-get-result.json",
        &"/synthetic/workspace".to_owned(),
    )?;
    write_fixture(
        stage,
        "service-workspace-list-params.json",
        &WorkspaceListParams {},
    )?;
    write_fixture(
        stage,
        "service-workspace-list-result.json",
        &vec![workspace_entry_fixture()],
    )?;
    // ── B08:此前 untyped 的 8 个方法的代表性 fixtures ──
    write_fixture(
        stage,
        "service-config-load-params.json",
        &ConfigLoadParams {},
    )?;
    write_fixture(
        stage,
        "service-config-load-result.json",
        &qaqh_config_api::ConfigDto::default(),
    )?;
    write_fixture(
        stage,
        "service-session-dashboard-params.json",
        &SessionDashboardParams {
            session_id: "0199a0f0-0000-7000-8000-000000000001".to_owned(),
        },
    )?;
    write_fixture(
        stage,
        "service-session-dashboard-result.json",
        &SessionDashboard {
            tasks: vec![SessionDashboardTask {
                id: "todo-fixture-1".to_owned(),
                subject: "synthetic task".to_owned(),
                description: String::new(),
                status: "in_progress".to_owned(),
                evidence: None,
            }],
            recent_edits: vec!["/synthetic/workspace/src/main.rs".to_owned()],
        },
    )?;
    write_fixture(
        stage,
        "service-exec_session-list-params.json",
        &ExecSessionListParams {
            session_id: "0199a0f0-0000-7000-8000-000000000001".to_owned(),
        },
    )?;
    write_fixture(
        stage,
        "service-exec_session-list-result.json",
        &ExecSessionListResult {
            sessions: Vec::new(),
        },
    )?;
    write_fixture(
        stage,
        "service-session-turn-params.json",
        &SessionTurnParams {
            session_id: "0199a0f0-0000-7000-8000-000000000001".to_owned(),
            turn_id: "turn-fixture-1".to_owned(),
        },
    )?;
    write_fixture(
        stage,
        "service-session-turn-result.json",
        &Option::<qaqh_ringing_contract::SessionTurnSnapshot>::None,
    )?;
    write_fixture(
        stage,
        "service-skills-list_tools-params.json",
        &SkillsListToolsParams {},
    )?;
    write_fixture(
        stage,
        "service-skills-list_tools-result.json",
        &vec!["fs_read".to_owned(), "todo_list".to_owned()],
    )?;
    write_fixture(
        stage,
        "service-todo-list-params.json",
        &TodoListParams {
            session_id: "0199a0f0-0000-7000-8000-000000000001".to_owned(),
            status: None,
        },
    )?;
    write_fixture(
        stage,
        "service-todo-list-result.json",
        &TodoListSnapshot {
            items: vec![TodoItemSnapshot {
                id: "todo-fixture-1".to_owned(),
                title: "synthetic task".to_owned(),
                description: String::new(),
                status: "in_progress".to_owned(),
                evidence: None,
            }],
            current_id: Some("todo-fixture-1".to_owned()),
            counts: TodoCountsSnapshot {
                pending: 0,
                in_progress: 1,
                completed: 0,
                cancelled: 0,
                total: 1,
            },
        },
    )?;
    write_fixture(
        stage,
        "service-todo-status-params.json",
        &TodoStatusParams {
            session_id: "0199a0f0-0000-7000-8000-000000000001".to_owned(),
        },
    )?;
    // 单条形状与 todo.list 同源;null 结果(无 todo 文件)由
    // `TodoStatusSnapshot | null` 类型表达,不设独立 fixture。
    write_fixture(
        stage,
        "service-todo-status-result.json",
        &TodoStatusSnapshot {
            mode: "manual".to_owned(),
            current_id: Some("todo-fixture-1".to_owned()),
            current_title: Some("synthetic task".to_owned()),
            pending: 0,
            in_progress: 1,
            completed: 0,
            cancelled: 0,
            total: 1,
            items: vec![TodoItemSnapshot {
                id: "todo-fixture-1".to_owned(),
                title: "synthetic task".to_owned(),
                description: String::new(),
                status: "in_progress".to_owned(),
                evidence: None,
            }],
        },
    )?;
    write_fixture(
        stage,
        "service-plan-read-params.json",
        &PlanReadParams {
            session_id: "0199a0f0-0000-7000-8000-000000000001".to_owned(),
        },
    )?;
    write_fixture(
        stage,
        "service-plan-read-result.json",
        &vec![PlanReadItem {
            id: "item-fixture-1".to_owned(),
            title: "synthetic plan step".to_owned(),
            status: PlanItemStatusView::Pending,
            comment: String::new(),
            actions: Vec::new(),
        }],
    )?;
    write_fixture(
        stage,
        "service-diagnostics-memory-snapshot-params.json",
        &DiagnosticsMemorySnapshotParams {
            after_sequence: None,
        },
    )?;
    write_fixture(
        stage,
        "service-diagnostics-memory-snapshot-result.json",
        &qaqh_memwatch::MemorySnapshot {
            schema_version: 1,
            enabled: true,
            started_at_ms: 1_759_000_000_000,
            sampled_at_ms: 1_759_000_060_000,
            components_sampled_at_ms: Some(1_759_000_060_000),
            components_stale: false,
            process: qaqh_memwatch::ProcessMemory {
                resident_bytes: Some(524_288),
                private_bytes: Some(262_144),
                virtual_bytes: Some(1_048_576),
                peak_resident_bytes: Some(524_288),
                source: "synthetic".to_owned(),
                error: None,
            },
            peak_sampled_private_bytes: Some(262_144),
            peak_sampled_resident_bytes: Some(524_288),
            tracked_session_count: 1,
            resident_session_count: 1,
            dropped_session_gauges: 0,
            dropped_phase_samples: 0,
            oldest_phase_sequence: Some(1),
            latest_phase_sequence: 1,
            phase_gap: false,
            components: Vec::new(),
            sessions: Vec::new(),
            phases: Vec::new(),
        },
    )?;
    account_fixtures::write(stage)?;
    client_fixtures::write(stage)?;
    Ok(())
}

/// `session.list` / `session.meta` 条目样本（前端契约 G2 的统一形状）。
/// 全合成字段，无真实会话元数据。
fn session_list_entry_fixture() -> qaqh_types::SessionListEntry {
    let mut meta = qaqh_types::SessionMeta::default();
    meta.session_id = "0199a0f0-0000-7000-8000-000000000001".into();
    meta.created_at = 1_759_000_000_000;
    meta.updated_at = 1_759_000_060_000;
    meta.model = "synthetic-model".into();
    meta.turn_count = 1;
    meta.title = Some("synthetic session".into());
    meta.cwd = Some("/synthetic/workspace".into());
    qaqh_types::SessionListEntry {
        meta,
        status: qaqh_types::SessionRunStatus::Idle,
        workspace_id: None,
    }
}

/// `workspace.list` 条目样本（meta + `missing_dir`）；全合成字段。
fn workspace_entry_fixture() -> qaqh_ringing_contract::WorkspaceEntry {
    workspace_meta_fixture().entry(false)
}

/// `workspace.create`/`workspace.rename` 结果样本（裸 `WorkspaceMeta`，
/// 无 `missing_dir` 键）。
fn workspace_meta_fixture() -> qaqh_ringing_contract::WorkspaceMeta {
    qaqh_ringing_contract::WorkspaceMeta {
        id: "ws-fixture-1".to_owned(),
        path: "/synthetic/workspace".to_owned(),
        title: "synthetic workspace".to_owned(),
        order: 0,
        session_ids: vec!["0199a0f0-0000-7000-8000-000000000001".to_owned()],
    }
}

fn write_fixture<T>(stage: &Path, name: &str, value: &T) -> Result<(), Box<dyn std::error::Error>>
where
    T: Serialize + DeserializeOwned,
{
    let serialized = serde_json::to_vec_pretty(value)?;
    let parsed: T = serde_json::from_slice(&serialized)?;
    let round_trip = serde_json::to_vec_pretty(&parsed)?;
    if serialized != round_trip {
        return Err(format!("fixture {name} changed after serde round-trip").into());
    }
    fs::write(
        stage.join("fixtures").join(name),
        [serialized.as_slice(), b"\n"].concat(),
    )?;
    Ok(())
}

fn export_types(config: &Config) -> Result<(), Box<dyn std::error::Error>> {
    macro_rules! export_all { ($($ty:ty),+ $(,)?) => { $(<$ty>::export_all(config)?;)+ } }
    export_all!(
        qaqh_types::DaemonDiscovery,
        RingingCommand,
        RingingCommandAck,
        RingingCommandAckStatus,
        RingingCommandState,
        RingingCommandStatus,
        RingingV2AskOutcome,
        RingingV2Capabilities,
        RingingV2ClientKind,
        RingingV2ClientView,
        RingingV2CommandAck,
        RingingV2CommandEnvelope,
        RingingV2CommandResult,
        RingingV2CommandStatus,
        RingingV2ContentUploadResponse,
        RingingV2DriverClaimResponse,
        RingingV2DriverReleaseResponse,
        RingingV2ExistingResult,
        RingingV2LeaseRenewResponse,
        RingingV2OpenRequest,
        RingingV2OpenResponse,
        RingingV2PendingApprovals,
        RingingV2PendingInteractionView,
        RingingV2PendingPermissionView,
        RingingV2ResyncRequired,
        RingingV2ApprovalRespondRequest,
        RingingV2DeviceApprovalKind,
        RingingV2DeviceChallenge,
        RingingV2DeviceChallenges,
        qaqh_ringing::RingingV2PairTokenRequest,
        qaqh_ringing::RingingV2PairTokenResponse,
        qaqh_ringing::RingingV2PairRequest,
        qaqh_ringing::RingingV2PairResponse,
        qaqh_ringing::RingingV2DeviceWire,
        qaqh_ringing::RingingV2DevicesResponse,
        // 会话面即契约类型（qaqh-ringing-contract）：bootstrap/state/事件帧
        // 一手导出,无投影/泛型容器中间层。
        RingingV2SyncResult,
        // JSON-in-JSON 边界（ADR-0004 §决策 3）：审批卡的正文穿过 HTTP 面,它的形状
        // 必须是导出物,不接受"它是 `serde_json::Value` 所以不用管"。
        qaqh_ringing_contract::PermissionRequestDetails,
        qaqh_ringing_contract::SessionBootstrap,
        qaqh_ringing_contract::SessionState,
        qaqh_ringing_contract::SessionTurnSnapshot,
        qaqh_ringing_contract::SessionEvent,
        qaqh_ringing_contract::SessionEventFrame,
        qaqh_ringing_contract::ToolCallState,
        qaqh_ringing_contract::TurnPhase,
        qaqh_ringing_contract::ExecSessionView,
        qaqh_ringing_contract::ExecSessionState,
        qaqh_ringing_contract::ExecSessionOutcome,
        qaqh_ringing_contract::ExecStopReason,
        qaqh_ringing_contract::ExecCancelReason,
        qaqh_ringing_contract::ExecInterruptReason,
        qaqh_types::ExecSessionId,
        qaqh_ringing_contract::fact::ContentHash,
        qaqh_types::SessionContentRef,
        qaqh_types::SessionToolError,
        qaqh_types::ToolCallOutcome,
        qaqh_types::ToolCancellationReason,
        qaqh_types::ToolInterruptionReason,
        qaqh_ringing_contract::TeamResponse,
        qaqh_ringing_runtime::pending_store::ReceiptPolicy,
        qaqh_runtime::ringing::service_methods::ClientsListParams,
        qaqh_runtime::ringing::service_methods::DaemonVersionParams,
        qaqh_runtime::ringing::service_methods::FsEntry,
        qaqh_runtime::ringing::service_methods::FsListParams,
        qaqh_runtime::ringing::service_methods::FsReadParams,
        qaqh_runtime::ringing::service_methods::FsReadResult,
        qaqh_runtime::ringing::service_methods::SessionActivityParams,
        qaqh_runtime::ringing::service_methods::SessionGetActivityParams,
        qaqh_runtime::ringing::service_methods::ExecSessionListParams,
        qaqh_runtime::ringing::service_methods::ExecSessionListResult,
        qaqh_runtime::ringing::service_methods::SessionTurnParams,
        qaqh_runtime::ringing::service_methods::SessionListParams,
        qaqh_runtime::ringing::service_methods::SessionMetaParams,
        qaqh_runtime::ringing::service_methods::SessionToolActivity,
        qaqh_runtime::ringing::service_methods::SkillsCatalogParams,
        qaqh_runtime::ringing::service_methods::SkillsCatalogResult,
        qaqh_runtime::ringing::service_methods::WorkspaceGetParams,
        qaqh_runtime::ringing::service_methods::WorkspaceListParams,
        qaqh_runtime::ringing::service_methods::ConfigLoadParams,
        qaqh_runtime::ringing::service_methods::DiagnosticsMemorySnapshotParams,
        qaqh_runtime::ringing::service_methods::SessionDashboardParams,
        qaqh_runtime::ringing::service_methods::SessionDashboard,
        qaqh_runtime::ringing::service_methods::SessionDashboardTask,
        qaqh_runtime::ringing::service_methods::SkillsListToolsParams,
        qaqh_runtime::ringing::service_methods::PlanReadParams,
        qaqh_runtime::ringing::service_methods::PlanReadItem,
        qaqh_runtime::ringing::service_methods::PlanItemStatusView,
        qaqh_runtime::ringing::service_methods::TodoListParams,
        qaqh_runtime::ringing::service_methods::TodoListSnapshot,
        qaqh_runtime::ringing::service_methods::TodoItemSnapshot,
        qaqh_runtime::ringing::service_methods::TodoCountsSnapshot,
        qaqh_runtime::ringing::service_methods::TodoStatusParams,
        qaqh_runtime::ringing::service_methods::TodoStatusSnapshot,
        qaqh_memwatch::MemorySnapshot,
        qaqh_memwatch::ProcessMemory,
        qaqh_memwatch::SessionMemory,
        qaqh_memwatch::ComponentMemory,
        qaqh_memwatch::PhaseSample,
        qaqh_memwatch::PhaseKind,
        qaqh_ringing_contract::SessionActivity,
        qaqh_ringing_contract::SessionActivityState,
        qaqh_ringing_contract::WorkspaceEntry,
        qaqh_ringing_contract::WorkspaceMeta,
        qaqh_types::SessionListEntry,
        qaqh_types::SessionMeta,
        qaqh_types::SessionRunStatus,
        qaqh_types::SkillSessionEntry,
        qaqh_types::SkillSessionEntryState,
        qaqh_types::SkillSessionStateV2,
        // ── 配置 DTO 面（config.load / config.save 的读写形状）──
        qaqh_config_api::ConfigDto,
        qaqh_config_api::ConfigPatch,
        qaqh_config_api::SubagentDto,
        qaqh_config_api::SubagentPatch,
        qaqh_config_api::ExecDto,
        qaqh_config_api::ExecPatch,
        qaqh_config_api::SandboxDto,
        qaqh_config_api::SandboxPatch,
        qaqh_config_api::McpDto,
        qaqh_config_api::McpServerDto,
        qaqh_config_api::LspDto,
        qaqh_config_api::LspServerDto,
        qaqh_types::ReasoningKind,
        // ── RingingV2 唯一命令目录、信封、目标与前置条件 ──
        qaqh_ringing::CommandTarget,
        qaqh_ringing::CommandPrecondition,
        qaqh_ringing::ConversationSendMessage,
        qaqh_ringing::SessionCreateResult,
        qaqh_ringing::SessionSettingsPatch,
        qaqh_ringing::ToolsSettingPatch,
        qaqh_ringing::ProviderUpsertDto,
        qaqh_ringing::SecretWrite,
        qaqh_profile::ModelDef,
        qaqh_profile::ModelDefaults,
    );
    qaqh_runtime::ringing::service_methods::accounts::export_types(config)?;
    Ok(())
}

fn write_service_map(stage: &Path) -> Result<(), Box<dyn std::error::Error>> {
    use qaqh_runtime::ringing::service_methods::{MethodKind, TYPED_METHODS, accounts};
    // 两份 descriptor 清单（通用切片 + 账号面）合流；生成物只认 descriptor
    // 的类型名，不在此处维护第二份方法名单。
    let descriptors: Vec<_> = TYPED_METHODS
        .iter()
        .chain(accounts::TYPED_METHODS.iter())
        .collect();
    let mut imports: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut entries = String::new();
    for descriptor in &descriptors {
        let params = dto_reference(descriptor.params_type, &mut imports)?;
        let result = match descriptor.result_type {
            "null" => "null".to_owned(),
            "string" => "string".to_owned(),
            // 基础数组(如 skills.list_tools 的 string[])不经 DTO 导入。
            "string[]" => "string[]".to_owned(),
            other => dto_reference(other, &mut imports)?,
        };
        entries.push_str(&format!(
            "  \"{}\": {{ params: {}; result: {}; }};\n",
            descriptor.method, params, result
        ));
    }
    let import_list = imports.into_iter().collect::<Vec<_>>().join(", ");
    let mut output = format!(
        "import type {{ {import_list} }} from \"./index\";\n\nexport interface ServiceMap {{\n{entries}}}\n\nexport const serviceMetadata = {{\n"
    );
    for descriptor in &descriptors {
        let kind = match descriptor.info.kind {
            MethodKind::Read => "read",
            MethodKind::Write => "write",
        };
        output.push_str(&format!(
            "  \"{}\": {{ kind: \"{}\", requires_session: {}, admin_only: {} }},\n",
            descriptor.method, kind, descriptor.info.requires_session, descriptor.info.admin_only
        ));
    }
    output.push_str("} as const;\n");
    write(stage, "service-map.ts", &output)?;
    Ok(())
}

/// Map a descriptor type reference (`X`, `X[]`, `X?` nullable) to a TS type
/// name, registering the DTO import. Anything that is not a plain DTO name is
/// a descriptor bug; primitives are handled by the caller before reaching here.
fn dto_reference(
    type_name: &str,
    imports: &mut std::collections::BTreeSet<String>,
) -> Result<String, String> {
    let (base, suffix) = match type_name.strip_suffix("[]") {
        Some(base) => (base, "[]"),
        None => match type_name.strip_suffix('?') {
            Some(base) => (base, " | null"),
            None => (type_name, ""),
        },
    };
    let valid = !base.is_empty()
        && base
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && base.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !valid {
        return Err(format!(
            "descriptor type is not a plain generated DTO name: {type_name}"
        ));
    }
    imports.insert(base.to_owned());
    Ok(format!("{base}{suffix}"))
}

fn write_manifest(stage: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let paths: Vec<_> = list_files(stage)?
        .into_iter()
        .filter(|path| path != "manifest.json")
        .collect();
    let files: Vec<FileHashOwned> = paths
        .iter()
        .map(|path| {
            let bytes = fs::read(stage.join(path))?;
            Ok(FileHashOwned {
                path: path.clone(),
                sha256: hex::encode(Sha256::digest(bytes)),
            })
        })
        .collect::<Result<_, std::io::Error>>()?;
    let hash_input = serde_json::to_vec(
        &files
            .iter()
            .map(|f| FileHash {
                path: &f.path,
                sha256: f.sha256.clone(),
            })
            .collect::<Vec<_>>(),
    )?;
    let revision = env::var("QAQH_SOURCE_REVISION").unwrap_or_else(|_| "development".into());
    let dirty = env::var("QAQH_SOURCE_DIRTY")
        .map(|v| v != "0" && v != "false")
        .unwrap_or(true);
    let manifest = Manifest {
        format_version: FORMAT_VERSION,
        wire_version: qaqh_ringing::RINGING_V2_VERSION,
        backend_version: env!("CARGO_PKG_VERSION").into(),
        source_revision: revision,
        source_dirty: dirty,
        generator_version: GENERATOR_VERSION,
        contract_hash: hex::encode(Sha256::digest(hash_input)),
        files,
    };
    let json = serde_json::to_vec_pretty(&manifest)?;
    fs::write(
        stage.join("manifest.json"),
        [json.as_slice(), b"\n"].concat(),
    )?;
    Ok(())
}

fn write_index(stage: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let mut output = String::new();
    for path in list_files(&stage.join("types"))? {
        if !path.ends_with(".ts") || path == "qaqh/DaemonDiscovery.ts" {
            continue;
        }
        let module = path.strip_suffix(".ts").ok_or("invalid TS output path")?;
        output.push_str(&format!("export * from \"./types/{module}\";\n"));
    }
    // 会话面 = 契约类型直出（qaqh-ringing-contract）。`export *` 已把
    // SessionBootstrap/SessionState/SessionEvent/SessionEventFrame 引入公共
    // 根,不再有泛型容器别名层。
    write(stage, "index.ts", &output)?;
    Ok(())
}

fn validate_layout(stage: &Path) -> Result<(), Box<dyn std::error::Error>> {
    for required in [
        "manifest.json",
        "index.ts",
        "node.ts",
        "constants.ts",
        "service-map.ts",
        "coverage.json",
        "fixtures/README.md",
        // C# contract surface must ship alongside the TypeScript one.
        "cs/ContractConstants.cs",
        "cs/ContractJson.cs",
        "cs/RingingV2OpenResponse.cs",
    ] {
        if !stage.join(required).is_file() {
            return Err(format!("missing required artifact {required}").into());
        }
    }
    if !stage.join("types/qaqh/RingingV2OpenRequest.ts").is_file() {
        return Err("ts-rs did not generate expected wire roots".into());
    }
    validation::validate(stage)?;
    Ok(())
}

fn compare_trees(expected: &Path, actual: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let expected_files = export_artifact_paths(expected)?;
    let actual_files = export_artifact_paths(actual)?;
    if expected_files != actual_files {
        // 整棵树 dump 两遍等于没报错：600 条路径会吃掉 ~24KB 日志，真正要看的那 3 条
        // 反而被埋掉。增删+数量足够定位；逐产物的 wire 语义判定是
        // `scripts/check-surface-change-policy.py` 的活（它给 `artifact added/deleted`
        // 与 `wire-differs` 列表），这里不重复一遍。
        let only_expected: Vec<&str> = expected_files
            .iter()
            .filter(|path| !actual_files.contains(path))
            .map(String::as_str)
            .collect();
        let only_actual: Vec<&str> = actual_files
            .iter()
            .filter(|path| !expected_files.contains(path))
            .map(String::as_str)
            .collect();
        return Err(format!(
            "artifact file set differs: {} vs {} files; only-in-first=[{}], only-in-second=[{}]",
            expected_files.len(),
            actual_files.len(),
            summarize(&only_expected),
            summarize(&only_actual)
        )
        .into());
    }
    // manifest.json 走 compare_manifest 的语义比对（per-commit 戳剔除），
    // 不在这张字节等值表里。差异一次报全，而不是第一个就停 —— 换锁时最需要知道的
    // 是"到底动了哪几个产物"，逐个试错要跑 601 文件那么多次导出。
    let mut differing: Vec<&str> = Vec::new();
    for path in &expected_files {
        if path == "manifest.json" {
            continue;
        }
        if fs::read(expected.join(path))? != fs::read(actual.join(path))? {
            differing.push(path);
        }
    }
    if !differing.is_empty() {
        return Err(format!(
            "{} artifacts differ byte-wise: [{}]",
            differing.len(),
            summarize(&differing)
        )
        .into());
    }
    Ok(())
}

/// 报错用的路径摘要：最多 20 条，其余只报数量。
fn summarize(paths: &[&str]) -> String {
    let shown = paths.len().min(20);
    let mut rendered = paths[..shown].join(", ");
    if paths.len() > shown {
        rendered.push_str(&format!(", +{} more", paths.len() - shown));
    }
    rendered
}

/// `snapshots/` 是 `scripts/check-lts-api-surface.py` 消费的 route/facade
/// 基线，不是导出器产物。与 guard 的 `compare_file_set` 同口径排除，否则
/// `check --against docs/api-surface/*` 永远报集合漂移（误导性的"API 变更"）。
fn export_artifact_paths(root: &Path) -> Result<Vec<String>, std::io::Error> {
    Ok(list_files(root)?
        .into_iter()
        .filter(|path| !path.starts_with("snapshots/"))
        .collect())
}

/// manifest 语义比对：`source_revision` / `source_dirty` 是 per-commit 戳，
/// 按口径剔除后必须全等——`contract_hash` 覆盖全部产物哈希，仍逐字节钉死。
fn compare_manifest(expected: &Path, actual: &Path) -> Result<(), Box<dyn std::error::Error>> {
    fn load(
        dir: &Path,
    ) -> Result<serde_json::Map<String, serde_json::Value>, Box<dyn std::error::Error>> {
        let text = fs::read_to_string(dir.join("manifest.json"))?;
        let mut manifest: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&text)?;
        manifest.remove("source_revision");
        manifest.remove("source_dirty");
        Ok(manifest)
    }
    if load(expected)? != load(actual)? {
        return Err("manifest.json differs beyond source_revision/source_dirty".into());
    }
    Ok(())
}

fn list_files(root: &Path) -> Result<Vec<String>, std::io::Error> {
    fn visit(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<(), std::io::Error> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "symlinks are not allowed in a contract artifact: {}",
                        path.display()
                    ),
                ));
            }
            if kind.is_dir() {
                visit(root, &path, out)?;
            } else if kind.is_file() {
                out.push(
                    path.strip_prefix(root)
                        .map_err(std::io::Error::other)?
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    visit(root, root, &mut out)?;
    out.sort();
    Ok(out)
}

fn write(root: &Path, relative: &str, contents: &str) -> Result<(), std::io::Error> {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, contents)
}

fn staging_path(target: &Path) -> PathBuf {
    let name = target.file_name().unwrap_or_default().to_string_lossy();
    target.with_file_name(format!(
        ".{name}.staging-{}-{}",
        std::process::id(),
        unique_suffix()
    ))
}

fn unique_suffix() -> String {
    format!(
        "{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_rejects_extra_and_modified_files() {
        let root = env::temp_dir().join(format!("qaqh-contract-test-{}", unique_suffix()));
        let expected = root.join("expected");
        let actual = root.join("actual");
        fs::create_dir_all(&expected).expect("expected dir");
        fs::create_dir_all(&actual).expect("actual dir");
        fs::write(expected.join("file.ts"), "export type A = string;\n").expect("write expected");
        fs::write(actual.join("file.ts"), "export type A = number;\n").expect("write actual");
        assert!(compare_trees(&expected, &actual).is_err());
        fs::write(actual.join("extra.ts"), "export type B = string;\n").expect("write extra");
        assert!(compare_trees(&expected, &actual).is_err());
        fs::remove_dir_all(root).expect("cleanup test tree");
    }

    /// guard 口径：snapshots/ 里的 route/facade 基线不是导出产物，不得进
    /// 文件集比对（否则 `check --against docs/api-surface/*` 永远假阳）。
    #[test]
    fn compare_trees_ignores_snapshot_baseline() {
        let root = env::temp_dir().join(format!("qaqh-contract-test-{}", unique_suffix()));
        let expected = root.join("expected");
        let actual = root.join("actual");
        fs::create_dir_all(&expected).expect("expected dir");
        fs::create_dir_all(&actual).expect("actual dir");
        fs::write(expected.join("index.ts"), "export * from \"./a\";\n").expect("write expected");
        fs::write(actual.join("index.ts"), "export * from \"./a\";\n").expect("write actual");
        fs::create_dir_all(actual.join("snapshots")).expect("snapshots dir");
        fs::write(actual.join("snapshots/routes.txt"), "GET /x\n").expect("write snapshot");
        compare_trees(&expected, &actual).expect("snapshots/ must not count as artifact drift");
        fs::remove_dir_all(root).expect("cleanup test tree");
    }

    /// manifest 语义比对：per-commit 戳允许不同；contract_hash 动一个字符
    /// 必须失败。
    #[test]
    fn compare_manifest_carves_out_per_commit_stamps() {
        let root = env::temp_dir().join(format!("qaqh-contract-test-{}", unique_suffix()));
        let expected = root.join("expected");
        let actual = root.join("actual");
        fs::create_dir_all(&expected).expect("expected dir");
        fs::create_dir_all(&actual).expect("actual dir");
        fs::write(
            expected.join("manifest.json"),
            r#"{"source_revision":"aaaa","source_dirty":false,"contract_hash":"h1"}"#,
        )
        .expect("write expected manifest");
        fs::write(
            actual.join("manifest.json"),
            r#"{"source_revision":"bbbb","source_dirty":true,"contract_hash":"h1"}"#,
        )
        .expect("write actual manifest");
        compare_manifest(&expected, &actual).expect("volatile stamps must be excluded");
        fs::write(
            actual.join("manifest.json"),
            r#"{"source_revision":"bbbb","source_dirty":true,"contract_hash":"h2"}"#,
        )
        .expect("mutate contract hash");
        assert!(compare_manifest(&expected, &actual).is_err());
        fs::remove_dir_all(root).expect("cleanup test tree");
    }
}
