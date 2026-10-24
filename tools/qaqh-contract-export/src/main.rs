use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use qaqh_domain::ControlCommand;
use qaqh_ringing::{
    RingingCommand, RingingCommandAck, RingingCommandAckStatus, RingingCommandState,
    RingingCommandStatus, RingingV2ApprovalRespondRequest, RingingV2AskOutcome,
    RingingV2Bootstrap, RingingV2Capabilities, RingingV2ChannelSnapshot, RingingV2CommandAck,
    RingingV2CommandEnvelope, RingingV2CommandResult, RingingV2CommandStatus,
    RingingV2ContentUploadResponse, RingingV2ContentValue, RingingV2Delivery,
    RingingV2DeviceApprovalKind, RingingV2DeviceChallenge, RingingV2DeviceChallenges,
    RingingV2DriverClaimResponse, RingingV2DriverReleaseResponse, RingingV2DriverState,
    RingingV2EventEnvelope, RingingV2ExistingResult, RingingV2InteractionKind,
    RingingV2LeaseRenewResponse, RingingV2OpenRequest, RingingV2OpenResponse,
    RingingV2PendingApprovals, RingingV2PendingInteraction, RingingV2PendingInteractionView,
    RingingV2PendingPermissionView, RingingV2ResetReason, RingingV2ResetRequired,
    RingingV2StreamKey, TimelinePage, TimelineSseFrame,
};
use serde::{Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use ts_rs::{Config, TS};

const FORMAT_VERSION: u32 = 1;
const GENERATOR_VERSION: &str = "qaqh-contract-export/0.1.0";

mod account_fixtures;
#[cfg(test)]
mod artifact_tests;
mod client_fixtures;
mod coverage;
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
    let command = args
        .next()
        .ok_or("expected export --out <dir> or check --against <dir>")?;
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
        _ => Err(
            "expected export --out <dir> [--release] or check --against <dir> [--release]".into(),
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
            compare_trees(&stage, target)
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
            "export const RINGING_SCHEMA = {:?} as const;\nexport const RINGING_V2_VERSION = {} as const;\nexport const RINGING_V2_BASE_PATH = {:?} as const;\nexport const CLIENT_SESSION_HEADER = {:?} as const;\nexport const MAX_SAFE_INTEGER = {} as const;\n",
            qaqh_ringing::RINGING_SCHEMA,
            qaqh_ringing::RINGING_V2_VERSION,
            qaqh_ringing::RINGING_V2_BASE_PATH,
            qaqh_ringing::CLIENT_SESSION_HEADER,
            qaqh_ringing::MAX_SAFE_INTEGER,
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
    write_manifest(stage)?;
    Ok(())
}

fn write_fixtures(stage: &Path) -> Result<(), Box<dyn std::error::Error>> {
    use qaqh_runtime::ringing::service_methods::{
        DaemonVersionParams, FsEntry, FsListParams, FsReadParams, FsReadResult, SessionListParams,
        SessionMetaParams, SessionRenameParams,
    };
    let open_request = RingingV2OpenRequest::new("fixture-client");
    write_fixture(stage, "open-request.json", &open_request)?;

    let command = RingingV2CommandEnvelope::new(
        "cmd-fixture-1",
        "fixture-client",
        RingingCommand::Control(ControlCommand::SessionAttach {
            session_id: "session-fixture".to_owned(),
        }),
    )
    .with_client_session_id("client-session-fixture")
    .with_session_id("session-fixture");
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
    };
    write_fixture(stage, "command-ack-accepted.json", &ack)?;

    let status = RingingV2CommandStatus {
        command_id: "cmd-fixture-1".to_owned(),
        state: RingingCommandState::Accepted,
        payload_fingerprint: "synthetic-fingerprint".to_owned(),
        terminal_event_id: None,
        error_code: None,
        result: None,
    };
    write_fixture(stage, "command-status-accepted.json", &status)?;

    let capabilities = RingingV2Capabilities {
        subscribe: true,
        interact: true,
        drive: true,
        timeline: true,
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
    let rename_params = SessionRenameParams {
        session_id: "session-fixture".to_owned(),
        title: "synthetic-title".to_owned(),
    };
    write_fixture(stage, "service-session-rename-params.json", &rename_params)?;
    write_fixture(
        stage,
        "service-session-rename-result.json",
        &Option::<()>::None,
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
        &SessionListParams {},
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
    meta.message_count = 3;
    meta.turn_count = 1;
    meta.title = Some("synthetic session".into());
    meta.cwd = Some("/synthetic/workspace".into());
    qaqh_types::SessionListEntry {
        meta,
        status: qaqh_types::SessionRunStatus::Idle,
        workspace_id: None,
    }
}

fn write_fixture<T>(stage: &Path, name: &str, value: &T) -> Result<(), Box<dyn std::error::Error>>
where
    T: Serialize + DeserializeOwned,
{    let serialized = serde_json::to_vec_pretty(value)?;
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
        RingingCommand, RingingCommandAck, RingingCommandAckStatus, RingingCommandState,
        RingingCommandStatus, RingingV2AskOutcome,
        RingingV2Capabilities, RingingV2CommandAck,
        RingingV2CommandEnvelope, RingingV2CommandResult, RingingV2CommandStatus,
        RingingV2ContentUploadResponse, RingingV2ContentValue, RingingV2Delivery,
        RingingV2DriverClaimResponse,
        RingingV2DriverReleaseResponse, RingingV2DriverState,
        RingingV2ExistingResult, RingingV2InteractionKind, RingingV2LeaseRenewResponse,
        RingingV2OpenRequest, RingingV2OpenResponse, RingingV2PendingApprovals,
        RingingV2PendingInteraction, RingingV2PendingInteractionView,
        RingingV2PendingPermissionView,
        RingingV2ResetReason, RingingV2ResetRequired, RingingV2StreamKey,
        RingingV2ApprovalRespondRequest, RingingV2DeviceApprovalKind, RingingV2DeviceChallenge,
        RingingV2DeviceChallenges,
        TimelinePage, TimelineSseFrame,
        qaqh_ringing::RingingV2PairTokenRequest,
        qaqh_ringing::RingingV2PairTokenResponse,
        qaqh_ringing::RingingV2PairRequest,
        qaqh_ringing::RingingV2PairResponse,
        qaqh_ringing::RingingV2DeviceWire,
        qaqh_ringing::RingingV2DevicesResponse,
        // 泛型容器按当前产品的具体组合导出：bootstrap 的 control/conversation/
        // tool state 与事件 payload 都是一手契约类型（qaqh-session::client_v2 /
        // projection / ProjectionPayload），不再以 `()` 占位。
        RingingV2Bootstrap<
            qaqh_session::client_v2::ClientV2ControlState,
            qaqh_session::projection::ConversationSnapshot,
            qaqh_session::client_v2::ClientV2ToolState,
        >,
        RingingV2ChannelSnapshot<qaqh_session::client_v2::ClientV2ControlState>,
        RingingV2EventEnvelope<qaqh_session::session_fact_v2::ProjectionPayload>,
        qaqh_session::client_v2::ClientV2ControlState,
        qaqh_session::client_v2::ClientV2TeamResponse,
        qaqh_session::client_v2::ClientV2ToolState,
        qaqh_runtime::ringing::pending_store::ReceiptPolicy,
        qaqh_runtime::ringing::service_methods::DaemonVersionParams,
        qaqh_runtime::ringing::service_methods::FsEntry,
        qaqh_runtime::ringing::service_methods::FsListParams,
        qaqh_runtime::ringing::service_methods::FsReadParams,
        qaqh_runtime::ringing::service_methods::FsReadResult,
        qaqh_runtime::ringing::service_methods::SessionListParams,
        qaqh_runtime::ringing::service_methods::SessionMetaParams,
        qaqh_runtime::ringing::service_methods::SessionRenameParams,
        qaqh_types::SessionListEntry,
        qaqh_types::SessionMeta,
        qaqh_types::SessionRunStatus,
        qaqh_types::SkillSessionEntry,
        qaqh_types::SkillSessionEntryState,
        qaqh_types::SkillSessionStateV2,
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
    // 泛型容器的当前产品具体组合（spec §3.2：真实组合的公共类型名）。
    // `export *` 不把名字引入本模块作用域，别名所需的成员类型必须显式
    // import；这些名字都来自上方 types/ 导出闭包，生成器保证存在。
    output.push_str(
        "import type { ClientV2ControlState } from \"./types/qaqh/ClientV2ControlState\";\n\
         import type { ClientV2ToolState } from \"./types/qaqh/ClientV2ToolState\";\n\
         import type { ConversationSnapshot } from \"./types/qaqh/ConversationSnapshot\";\n\
         import type { ProjectionPayload } from \"./types/qaqh/ProjectionPayload\";\n\
         import type { RingingV2Bootstrap } from \"./types/qaqh/RingingV2Bootstrap\";\n\
         import type { RingingV2EventEnvelope } from \"./types/qaqh/RingingV2EventEnvelope\";\n\
         export type ClientV2Bootstrap = RingingV2Bootstrap<ClientV2ControlState, ConversationSnapshot, ClientV2ToolState>;\n\
         export type ClientV2Event = RingingV2EventEnvelope<ProjectionPayload>;\n\
         export type ClientV2Payload = ProjectionPayload;\n",
    );
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
    let expected_files = list_files(expected)?;
    let actual_files = list_files(actual)?;
    if expected_files != actual_files {
        return Err(format!(
            "artifact file set differs; expected={expected_files:?}, actual={actual_files:?}"
        )
        .into());
    }
    for path in expected_files {
        if fs::read(expected.join(&path))? != fs::read(actual.join(&path))? {
            return Err(format!("artifact differs: {path}").into());
        }
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
}
