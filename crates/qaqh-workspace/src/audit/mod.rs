//! Tool execution audit —— 工具调用审计账本（双写）。
//!
//! 每次工具调用产生一条 [`AuditEntry`]，落到两个账本：
//!
//! - **v1 CSV**（`<data_dir>/audit.csv`）：兼容列 + 尾部追加新列，人读/脚本
//!   消费；rotate 到 `audit.csv.1`（旧代顺移，最多 [`AUDIT_GENERATIONS`] 代），
//!   只 rename 不 truncate。
//! - **v2 JSONL**（`<data_dir>/audit/v2.jsonl`，见 [`v2`]）：主账本——字段
//!   自描述、seq 跨重启连续、SHA-256 哈希链可验证（[`v2::verify_ledger`]）。
//!
//! 写入失败**不得静默**：两条写入都尝试执行，任一失败以 [`AuditError`] 返回，
//! 调用方负责上报（fail-closed 策略属配置层，另行接线）。
//!
//! 隐私基线（workspace v2 spec 硬规则 9）：参数只落 SHA-256 与字节数，
//! 永不落明文；对象只落路径与内容指纹。

use sha2::Digest;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub mod v2;

/// 单个 `audit.csv` 的大小上限（字节）。达到后 rotate。
const MAX_AUDIT_BYTES: u64 = 4 * 1024 * 1024;

/// 保留的历史代数：`audit.csv.1` .. `audit.csv.{AUDIT_GENERATIONS}`。
const AUDIT_GENERATIONS: u32 = 3;

/// rotate 判定 + append 的进程内串行化。
///
/// 两个线程同时判定「超限」并 rotate，会让其中一个 rename 失败/把记录写进
/// 已被改名走的 inode；同进程内必须互斥。跨进程的安全性由 rename 语义保证
/// （见 [`rotate_if_needed`]）。
static AUDIT_APPEND_LOCK: Mutex<()> = Mutex::new(());

/// A single audit entry for a tool invocation.
///
/// 前 8 个字段对应 v1 CSV 的历史列语义；其余为 v2 扩展（CSV 尾部追加，
/// 旧消费方按列索引读取不受影响）。
#[derive(Debug, Clone)]
pub struct AuditEntry {
    /// RFC3339 UTC 时间戳（v1 列；v2 信封时间由写入层自产）。
    pub ts: String,
    pub user: String,
    pub tool: String,
    pub action: String,
    /// SHA-256 十六进制参数指纹。
    pub args_hash: String,
    /// 序列化后的参数字节数。
    pub args_bytes: u64,
    /// 终态词汇表：`ok` | `error` | `partial` | `cancelled` | `backgrounded`。
    pub status: String,
    pub elapsed_ms: u64,
    /// v2 事件种类：`tool_call`（到达终态）| `tool_rejected`（未进入执行）。
    pub kind: v2::AuditKind,
    pub session: String,
    pub call_id: String,
    /// 能力类别：`read` | `write` | `exec` | `net`。
    pub category: String,
    /// 生效权限档位（1..=4；取不到时 None）。
    pub permission_level: Option<u8>,
    /// machine-readable 错误码（失败时；取自 `ToolResult.error.code`）。
    pub error_code: Option<String>,
    pub output_bytes: u64,
    pub retry_count: u32,
    /// MCP 动态工具的 server 侧原名（内置工具为 None）。
    pub effective_name: Option<String>,
    /// 授权结果词汇表见 [`v2::Decision`]。
    pub decision: Option<String>,
    pub decision_reason: Option<String>,
    /// 操作对象（路径 + before/after 指纹）。
    pub objects: Vec<v2::AuditObject>,
}

impl AuditEntry {
    /// v1 CSV `result` 列：`ok` | `fail`（由 status 派生，保持旧列语义）。
    fn csv_result(&self) -> &'static str {
        match self.status.as_str() {
            "ok" | "backgrounded" => "ok",
            _ => "fail",
        }
    }

    /// v1 CSV `files` 列：对象路径 `;` 连接（保持旧列语义）。
    fn csv_files(&self) -> String {
        self.objects
            .iter()
            .map(|object| object.path.as_str())
            .collect::<Vec<_>>()
            .join(";")
    }

    /// 投影为 v2 事件体（信封由 [`v2::append_event`] 补齐）。
    fn to_v2_event(&self) -> v2::Event {
        let sandbox = crate::authorization::is_subagent_sandbox();
        v2::Event {
            kind: self.kind,
            actor: v2::Actor {
                kind: if sandbox {
                    v2::ActorKind::Subagent
                } else {
                    v2::ActorKind::Agent
                },
                user: v2::current_user(),
                session: self.session.clone(),
                call_id: self.call_id.clone(),
                sandbox,
            },
            tool: Some(v2::ToolInfo {
                name: self.tool.clone(),
                action: self.action.clone(),
                category: self.category.clone(),
                effective_name: self.effective_name.clone(),
                permission_level: self.permission_level,
            }),
            decision: self.decision.as_ref().map(|outcome| v2::Decision {
                outcome: outcome.clone(),
                reason: self.decision_reason.clone(),
            }),
            args: v2::ArgsMeta {
                hash: self.args_hash.clone(),
                bytes: self.args_bytes,
            },
            result: Some(v2::ResultMeta {
                status: self.status.clone(),
                error_code: self.error_code.clone(),
                elapsed_ms: self.elapsed_ms,
                output_bytes: self.output_bytes,
                retry_count: self.retry_count,
            }),
            objects: self.objects.clone(),
        }
    }
}

/// 账本写入错误（CSV 与 v2 都尝试写入，任一失败即上报）。
#[derive(Debug)]
pub enum AuditError {
    /// v1 CSV 写入失败。
    Csv(std::io::Error),
    /// v2 JSONL 写入失败。
    V2(v2::AuditError),
}

impl std::fmt::Display for AuditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Csv(e) => write!(f, "audit csv write failed: {e}"),
            Self::V2(e) => write!(f, "audit v2 write failed: {e}"),
        }
    }
}

impl std::error::Error for AuditError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Csv(e) => Some(e),
            Self::V2(e) => Some(e),
        }
    }
}

/// Path to the v1 CSV log file.
fn audit_path() -> std::path::PathBuf {
    #[cfg(test)]
    {
        unit_test_dir().join("audit.csv")
    }
    #[cfg(not(test))]
    {
        qaqh_types::platform::data_dir().join("audit.csv")
    }
}

/// v2 账本根目录：`{data_dir}/audit`。
pub fn audit_dir() -> PathBuf {
    #[cfg(test)]
    {
        unit_test_dir()
    }
    #[cfg(not(test))]
    {
        qaqh_types::platform::data_dir().join("audit")
    }
}

/// 单测隔离目录：crate 内单测绝不写真实用户账本（历史教训——测试记录
/// 曾污染生产 `audit.csv`）。集成测试（lib 以非 cfg(test) 构建）需自行
/// 钉住 `QAQH_DATA_DIR` 重定向。
#[cfg(test)]
fn unit_test_dir() -> PathBuf {
    std::env::temp_dir().join(format!("qaqh-audit-unit-test-{}", std::process::id()))
}

/// 第 `generation` 代 rotate 文件：`audit.csv` → `audit.csv.1` / `.2` / …。
fn rotated_path(path: &Path, generation: u32) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".{generation}"));
    PathBuf::from(name)
}

/// 超限则 rotate：`audit.csv` → `audit.csv.1`，旧代顺移，最老一代删除。
///
/// **不丢记录**：只 `rename`，绝不 truncate。`rename` 在同一文件系统内是
/// 原子替换 ⇒ rotate 期间并发的 append（包括其它进程已持有 fd 的写）会落到
/// 被改名后的 inode（即 `.1`），记录仍在盘上；下一次 append 重新 open 即落到
/// 新的 `audit.csv`。唯一被丢弃的是超出 [`AUDIT_GENERATIONS`] 的最老一代——
/// 有界保留正是 rotation 的目的。
fn rotate_if_needed(path: &Path) {
    let Ok(meta) = std::fs::metadata(path) else {
        return; // 不存在：无需 rotate
    };
    if meta.len() < MAX_AUDIT_BYTES {
        return;
    }
    // 旧代顺移：从最老往新挪，避免覆盖。Windows 上 rename 到已存在目标会
    // 失败，故先删目标（即丢弃最老一代）。
    for generation in (1..AUDIT_GENERATIONS).rev() {
        let from = rotated_path(path, generation);
        if from.exists() {
            let to = rotated_path(path, generation + 1);
            let _ = std::fs::remove_file(&to);
            let _ = std::fs::rename(&from, &to);
        }
    }
    let first = rotated_path(path, 1);
    let _ = std::fs::remove_file(&first);
    if let Err(e) = std::fs::rename(path, &first) {
        // rename 失败（跨卷/权限/目标占用）：继续 append 到原文件——宁可
        // 继续增长，也不丢记录。
        log::error!("audit: rotate {} failed: {e}", path.display());
    }
}

/// Append a single audit entry to both ledgers (v1 CSV + v2 JSONL).
///
/// 两个账本都会尝试写入（互不短路）；任一失败返回错误，调用方必须上报。
pub fn append_audit(entry: &AuditEntry) -> Result<(), AuditError> {
    append_audit_impl(&audit_path(), &audit_dir(), entry)
}

/// [`append_audit`] 的可测形态（路径显式注入）。
fn append_audit_impl(
    csv_path: &Path,
    v2_root: &Path,
    entry: &AuditEntry,
) -> Result<(), AuditError> {
    let csv_result = append_csv_to(csv_path, entry).map_err(AuditError::Csv);
    let v2_result = v2::append_event(v2_root, entry.to_v2_event())
        .map(|_| ())
        .map_err(AuditError::V2);
    csv_result.and(v2_result)
}

/// 追加一行 v1 CSV（15 列：8 旧列 + 7 扩展列）。
fn append_csv_to(path: &Path, entry: &AuditEntry) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // rotate 判定与 append 在同一临界区：并发线程若在「判定超限 → rotate」
    // 与「open 旧 inode → write」之间交错，会把记录写进已被 rename 走的文件
    // （不丢但落错代），或与 rotate 竞争导致其失败后无限增长。
    let _guard = AUDIT_APPEND_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    rotate_if_needed(path);
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    // 商业审计基线：账本文件仅属主可读写（Unix；Windows 依赖继承 ACL）。
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    // CSV row: ts,user,tool,action,args_hash,result,elapsed_ms,files,
    //           session,call_id,category,error_code,output_bytes,retry_count,decision
    let row = format!(
        "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}\n",
        csv_escape(&entry.ts),
        csv_escape(&entry.user),
        csv_escape(&entry.tool),
        csv_escape(&entry.action),
        csv_escape(&entry.args_hash),
        csv_escape(entry.csv_result()),
        entry.elapsed_ms,
        csv_escape(&entry.csv_files()),
        csv_escape(&entry.session),
        csv_escape(&entry.call_id),
        csv_escape(&entry.category),
        csv_escape(entry.error_code.as_deref().unwrap_or("")),
        entry.output_bytes,
        entry.retry_count,
        csv_escape(entry.decision.as_deref().unwrap_or("")),
    );
    file.write_all(row.as_bytes())
}

/// Compute SHA-256 hex digest of the serialized arguments.
pub fn hash_args(args: &serde_json::Value) -> String {
    let json_str = serde_json::to_string(args).unwrap_or_default();
    let hash = sha2::Sha256::digest(json_str.as_bytes());
    hex::encode(hash)
}

/// 参数字节数（与 [`hash_args`] 同一序列化形态）。
pub fn args_size(args: &serde_json::Value) -> u64 {
    serde_json::to_string(args)
        .map(|json| json.len() as u64)
        .unwrap_or(0)
}

/// `ToolStatus` → 审计词汇表（ok | error | partial | cancelled | backgrounded）。
pub fn status_str(status: qaqh_types::ToolStatus) -> &'static str {
    match status {
        qaqh_types::ToolStatus::Ok => "ok",
        qaqh_types::ToolStatus::Error => "error",
        qaqh_types::ToolStatus::Partial => "partial",
        qaqh_types::ToolStatus::Cancelled => "cancelled",
        qaqh_types::ToolStatus::Backgrounded => "backgrounded",
    }
}

/// Escape a string for CSV: wrap in quotes if it contains comma, quote, or newline.
fn csv_escape(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        let escaped = s.replace('"', "\"\"");
        format!("\"{escaped}\"")
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(marker: &str) -> AuditEntry {
        AuditEntry {
            ts: "2026-09-17T12:00:00.000000000+00:00".to_string(),
            user: "agent".to_string(),
            tool: marker.to_string(),
            action: "run".to_string(),
            args_hash: "deadbeef".to_string(),
            args_bytes: 42,
            status: "ok".to_string(),
            elapsed_ms: 1,
            kind: v2::AuditKind::ToolCall,
            session: "sess-1".to_string(),
            call_id: format!("call-{marker}"),
            category: "write".to_string(),
            permission_level: Some(3),
            error_code: None,
            output_bytes: 7,
            retry_count: 0,
            effective_name: None,
            decision: Some("auto".to_string()),
            decision_reason: None,
            objects: vec![v2::AuditObject {
                kind: "file".to_string(),
                path: "/tmp/example.txt".to_string(),
                before_sha: None,
                after_sha: Some("a1".to_string()),
            }],
        }
    }

    #[test]
    fn audit_csv_growth_bounded() {
        // T-8-3①：audit.csv 不再无界增长。预置一个超过上限的日志（模拟长期
        // append 后的体积），下一次 append 必须触发 rotate：活动文件体积回落
        // 到上限以下，旧内容与新记录都不丢（rename，不是 truncate）。
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("audit.csv");
        let oversized = MAX_AUDIT_BYTES as usize + 64;
        std::fs::write(&path, "x".repeat(oversized)).expect("seed oversized audit");

        append_csv_to(&path, &entry("row-after-rotate")).expect("append after rotate");

        let active = std::fs::read_to_string(&path).expect("read active");
        assert!(
            (active.len() as u64) < MAX_AUDIT_BYTES,
            "active audit.csv must shrink below the cap after rotate, got {} bytes",
            active.len()
        );
        assert!(
            active.contains("row-after-rotate"),
            "the triggering row must survive in the active file: {active:?}"
        );

        let rotated = std::fs::read_to_string(rotated_path(&path, 1)).expect("rotated .1 exists");
        assert_eq!(
            rotated.len(),
            oversized,
            "rotate must move (not truncate) the old file"
        );
        assert!(rotated.starts_with("xxx"));
    }

    #[test]
    fn audit_rotate_preserves_concurrent_rows() {
        // rotate 与并发 append 竞争时不得丢记录：8 线程 × 25 行，全部行必须
        // 能在活动文件或各代 rotate 文件中找到。
        let dir = tempfile::tempdir().expect("tempdir");
        let path = std::sync::Arc::new(dir.path().join("audit.csv"));
        std::fs::write(&*path, "x".repeat(MAX_AUDIT_BYTES as usize + 1)).expect("seed oversized");

        let mut handles = Vec::new();
        for t in 0..8u32 {
            let p = std::sync::Arc::clone(&path);
            handles.push(std::thread::spawn(move || {
                for i in 0..25u32 {
                    append_csv_to(&p, &entry(&format!("row-{t}-{i}"))).expect("append row");
                }
            }));
        }
        for handle in handles {
            handle.join().expect("audit writer thread must not panic");
        }

        let mut all = std::fs::read_to_string(&*path).expect("read active");
        for generation in 1..=AUDIT_GENERATIONS {
            if let Ok(rotated) = std::fs::read_to_string(rotated_path(&path, generation)) {
                all.push_str(&rotated);
            }
        }
        for t in 0..8u32 {
            for i in 0..25u32 {
                let marker = format!("row-{t}-{i}");
                assert!(all.contains(&marker), "{marker} lost across rotate");
            }
        }
    }

    #[test]
    fn dual_write_produces_csv_row_and_chained_v2_record() {
        let dir = tempfile::tempdir().expect("tempdir");
        let csv_path = dir.path().join("audit.csv");
        let v2_root = dir.path().join("audit");
        append_audit_impl(&csv_path, &v2_root, &entry("write")).expect("dual write");

        // v1 CSV：15 列，新列携带 session/call_id/category/decision。
        let csv = std::fs::read_to_string(&csv_path).expect("read csv");
        let row = csv.lines().next().expect("one csv row");
        assert_eq!(row.split(',').count(), 15, "row: {row}");
        assert!(row.contains("sess-1"), "session column missing: {row}");
        assert!(row.contains("call-write"), "call_id column missing: {row}");

        // v2 JSONL：链头记录 + 信封字段。
        let ledger = std::fs::read_to_string(v2::active_path(&v2_root)).expect("read v2");
        let line = ledger.lines().next().expect("one v2 line");
        let parsed: v2::Record = serde_json::from_str(line).expect("parse v2 record");
        assert_eq!(parsed.seq, 1);
        assert!(!parsed.hash.is_empty(), "hash must be stamped");
        assert_eq!(parsed.schema, v2::SCHEMA);
        assert_eq!(parsed.actor.session, "sess-1");
        assert_eq!(parsed.actor.call_id, "call-write");
        assert_eq!(parsed.tool.as_ref().map(|t| t.category.as_str()), Some("write"));
        assert_eq!(
            parsed.decision.as_ref().map(|d| d.outcome.as_str()),
            Some("auto")
        );
        assert_eq!(parsed.result.as_ref().map(|r| r.status.as_str()), Some("ok"));
        assert_eq!(parsed.objects.len(), 1);
        assert!(
            v2::verify_ledger(&v2_root).ok,
            "chain must verify after dual write"
        );
    }

    #[test]
    fn csv_failure_is_reported_not_swallowed() {
        // 目标路径是目录 → open 必失败；错误必须返回而不是静默。
        let dir = tempfile::tempdir().expect("tempdir");
        let csv_path = dir.path().join("audit.csv");
        std::fs::create_dir_all(&csv_path).expect("create dir at csv path");
        let v2_root = dir.path().join("audit");
        let result = append_audit_impl(&csv_path, &v2_root, &entry("boom"));
        assert!(matches!(result, Err(AuditError::Csv(_))), "{result:?}");
        // v2 侧仍应成功写入（互不短路）。
        assert!(v2::active_path(&v2_root).exists());
    }

    #[cfg(unix)]
    #[test]
    fn csv_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let csv_path = dir.path().join("audit.csv");
        append_csv_to(&csv_path, &entry("perm")).expect("append");
        let mode = std::fs::metadata(&csv_path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "audit.csv must be owner-only, got {mode:o}");
    }
}
