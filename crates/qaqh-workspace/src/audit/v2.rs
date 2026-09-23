//! Audit v2 —— 结构化、链式、可验证的工具审计账本。
//!
//! 设计文档：`docs/spec/2026-09-19-审计账本v2-spec.md`。与 v1 CSV
//! （`audit.csv`）**双写**：CSV 保持既有列语义（仅尾部追加新列），v2 是
//! 主账本——每条事件一行 JSONL，字段自描述、哈希链可验证：
//!
//! - **时间**：`ts`（RFC3339 纳秒 UTC）+ `ts_mono_ns`（进程单调时钟）+
//!   `seq`（跨重启连续的全局序号）+ `boot_id`（每次进程启动唯一）；
//! - **行为**：`actor`（主体链 session/call_id）+ `tool` + `decision` +
//!   `result`（含 machine-readable `error_code`）；
//! - **对象**：`objects[]`（路径 + before/after 内容指纹，取自 file_state
//!   账本的 LF 规范视图 hash）；
//! - **完整性**：`hash = sha256(prev_hash + "\n" + canonical_json)`，
//!   `prev_hash` 串成链；[`verify_ledger`] 校验链、序号连续、schema；
//! - **隐私**：参数**永不落明文**（只有 `args.hash` / `args.bytes`，遵守
//!   workspace v2 spec 硬规则 9）；对象只存路径与指纹；
//! - **恢复**：进程重启后从账本尾部续接 seq/hash；崩溃撕裂的尾行移入
//!   `<active>.torn` 侧车（证据保留）后修复，链继续。
//!
//! 轮转：活动文件超过 [`MAX_BYTES`] 时 `v2.jsonl` → `v2.jsonl.1`（旧代顺移，
//! 最多 [`GENERATIONS`] 代）；**链跨段连续**，[`verify_ledger`] 按段序校验。
//! rotate 只做 rename，绝不 truncate——审计记录宁可多留不可丢。

use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

/// v2 账本 schema 标识（每条记录都带；读侧据此判定格式）。
pub const SCHEMA: &str = "qaqh.audit/v2";
/// 活动文件大小上限（字节）。达到后 rotate。
pub const MAX_BYTES: u64 = 4 * 1024 * 1024;
/// 保留的历史代数：`v2.jsonl.1` .. `v2.jsonl.{GENERATIONS}`。
pub const GENERATIONS: u32 = 3;
/// 活动文件名（位于账本根目录内）。
pub const ACTIVE_FILE: &str = "v2.jsonl";

/// rotate 判定 + append + 链状态更新的进程内串行化。
static APPEND_LOCK: Mutex<()> = Mutex::new(());

/// 每活动文件的链状态（seq 高水位 + 链头 hash），按路径缓存。
static CHAIN: LazyLock<Mutex<HashMap<PathBuf, ChainState>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 进程启动标识：pid + 进程内首次取用的时间戳（无需随机数依赖，跨进程唯一）。
static BOOT_ID: LazyLock<String> = LazyLock::new(|| {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:x}-{:x}", std::process::id(), nanos)
});

/// 单调时钟原点（进程内）。
static MONO_START: LazyLock<std::time::Instant> = LazyLock::new(std::time::Instant::now);

// ─────────────────────────────────────────────────────────────────────────────
// 记录模型
// ─────────────────────────────────────────────────────────────────────────────

/// 事件种类（闭集；新增走 schema 评审，读侧未知值容忍）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditKind {
    /// 工具调用到达终态（成功/失败/取消/后台化）。
    ToolCall,
    /// 工具调用在授权或前置检查阶段被拒绝，未进入执行。
    ToolRejected,
}

/// 发起主体类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorKind {
    Agent,
    Subagent,
    User,
    System,
}

/// 主体链：谁、在哪个会话、哪次调用、是否沙箱。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Actor {
    pub kind: ActorKind,
    /// 操作系统登录用户（真实主体；取不到时 `unknown`）。
    pub user: String,
    /// 会话 seed。
    pub session: String,
    /// 工具调用 id（与消息面 `tool_use_id` 同源）。
    pub call_id: String,
    /// 子代理沙箱上下文。
    pub sandbox: bool,
}

/// 工具面元数据（权限上下文的单一事实源）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolInfo {
    pub name: String,
    pub action: String,
    /// 能力类别：`read` | `write` | `exec` | `net`。
    pub category: String,
    /// MCP 动态工具的 server 侧原名（内置工具为 None）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_name: Option<String>,
    /// 生效权限档位（1..=4；取不到时 None）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_level: Option<u8>,
}

/// 授权决策。
///
/// `outcome` 词汇表：`auto`（策略自动放行，含 MCP D5 快路径）、
/// `user_approved`（用户显式批准的一次性凭证）、`sandbox_auto`（子代理
/// 沙箱内自动批准）、`challenge_required`（需要审批但通道不可用）、
/// `rejected`（授权/前置检查拒绝）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    pub outcome: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// 参数元数据——**永不携带明文**（spec 硬规则 9）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArgsMeta {
    /// SHA-256 十六进制（与 v1 CSV `args_hash` 同源）。
    pub hash: String,
    /// 序列化后的参数字节数。
    pub bytes: u64,
}

/// 结果元数据。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultMeta {
    /// `ok` | `error` | `partial` | `cancelled` | `backgrounded`。
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    pub elapsed_ms: u64,
    pub output_bytes: u64,
    pub retry_count: u32,
}

/// 操作对象描述符（可追溯到"对什么做了操作"）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditObject {
    /// `file` | `process` | `net` | `mcp` | `skill` | `session`（当前实现产出 file）。
    pub kind: String,
    /// 对象标识：文件为路径（workspace 相对或绝对，由工具原文决定）。
    pub path: String,
    /// 操作前内容指纹（file_state LF 规范视图 hash；未知为 None）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_sha: Option<String>,
    /// 操作后内容指纹。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_sha: Option<String>,
}

/// 调用方填充的事件体；[`append_event`] 补齐信封（seq/boot/时间/链）后落盘。
#[derive(Debug, Clone)]
pub struct Event {
    pub kind: AuditKind,
    pub actor: Actor,
    pub tool: Option<ToolInfo>,
    pub decision: Option<Decision>,
    pub args: ArgsMeta,
    pub result: Option<ResultMeta>,
    pub objects: Vec<AuditObject>,
}

/// 落盘后的完整记录（JSONL 一行）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub schema: String,
    pub kind: AuditKind,
    /// 全局单调序号（跨重启连续；缺口 = 丢失证据）。
    pub seq: u64,
    pub boot_id: String,
    /// RFC3339 纳秒 UTC（展示用；排序权威是 seq/ts_mono_ns）。
    pub ts: String,
    /// 进程单调时钟（纳秒）。
    pub ts_mono_ns: u64,
    pub actor: Actor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<ToolInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<Decision>,
    pub args: ArgsMeta,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<ResultMeta>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub objects: Vec<AuditObject>,
    /// 前一条记录的 hash；链头为 ""。
    pub prev_hash: String,
    /// 本条 hash；空串 = 计算中间态（序列化时省略，不落盘）。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub hash: String,
}

/// 写入/序列化错误（调用方必须显式处理，禁止静默吞掉）。
#[derive(Debug)]
pub enum AuditError {
    Io(std::io::Error),
    Encode(serde_json::Error),
}

impl std::fmt::Display for AuditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "audit io error: {e}"),
            Self::Encode(e) => write!(f, "audit encode error: {e}"),
        }
    }
}

impl std::error::Error for AuditError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Encode(e) => Some(e),
        }
    }
}

impl From<std::io::Error> for AuditError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<serde_json::Error> for AuditError {
    fn from(e: serde_json::Error) -> Self {
        Self::Encode(e)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ChainState {
    seq: u64,
    hash: String,
}

impl ChainState {
    fn genesis() -> Self {
        Self {
            seq: 0,
            hash: String::new(),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 路径与身份
// ─────────────────────────────────────────────────────────────────────────────

/// 活动账本文件：`{root}/v2.jsonl`。
pub fn active_path(root: &Path) -> PathBuf {
    root.join(ACTIVE_FILE)
}

/// 第 `generation` 代 rotate 文件：`v2.jsonl.1` / `.2` / …。
pub fn rotated_path(root: &Path, generation: u32) -> PathBuf {
    root.join(format!("{ACTIVE_FILE}.{generation}"))
}

/// 操作系统登录用户名（真实主体；取不到时 `unknown`）。
pub fn current_user() -> String {
    std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}

fn mono_ns() -> u64 {
    MONO_START.elapsed().as_nanos() as u64
}

// ─────────────────────────────────────────────────────────────────────────────
// 写入
// ─────────────────────────────────────────────────────────────────────────────

/// 追加一条事件：补齐信封、计算链哈希、落盘，返回落盘记录。
///
/// 调用方负责把语义字段填好；本函数是唯一的信封/链权威。
pub fn append_event(root: &Path, event: Event) -> Result<Record, AuditError> {
    append_event_with_limit(root, event, MAX_BYTES)
}

/// [`append_event`] 的可测形态（rotate 阈值显式注入）。
fn append_event_with_limit(root: &Path, event: Event, limit: u64) -> Result<Record, AuditError> {
    std::fs::create_dir_all(root)?;
    let path = active_path(root);
    let _guard = APPEND_LOCK.lock().unwrap_or_else(|p| p.into_inner());

    rotate_if_needed(root, limit);

    let state = {
        let mut chain = CHAIN.lock().unwrap_or_else(|p| p.into_inner());
        match chain.get(&path) {
            Some(state) => state.clone(),
            None => {
                let recovered = recover_state(root, &path);
                chain.insert(path.clone(), recovered.clone());
                recovered
            }
        }
    };

    let record = Record {
        schema: SCHEMA.to_string(),
        kind: event.kind,
        seq: state.seq + 1,
        boot_id: BOOT_ID.clone(),
        ts: now_rfc3339(),
        ts_mono_ns: mono_ns(),
        actor: event.actor,
        tool: event.tool,
        decision: event.decision,
        args: event.args,
        result: event.result,
        objects: event.objects,
        prev_hash: state.hash,
        hash: String::new(),
    };
    let hash = record_hash(&record)?;
    let record = Record { hash, ..record };

    let mut payload = serde_json::to_string(&record)?;
    payload.push('\n');

    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    // 商业审计基线：账本文件仅属主可读写（Unix；Windows 依赖继承 ACL）。
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    file.write_all(payload.as_bytes())?;

    {
        let mut chain = CHAIN.lock().unwrap_or_else(|p| p.into_inner());
        chain.insert(
            path,
            ChainState {
                seq: record.seq,
                hash: record.hash.clone(),
            },
        );
    }
    Ok(record)
}

/// 链哈希：`sha256(prev_hash + "\n" + canonical_json(record without hash))`。
///
/// canonical_json 由 serde 按结构体字段声明顺序序列化（无 map 遍历），
/// 因此同一条记录在任何进程/时间点重算都得到相同字节。
fn record_hash(record: &Record) -> Result<String, AuditError> {
    let mut canonical = record.clone();
    canonical.hash.clear();
    let json = serde_json::to_string(&canonical)?;
    let mut hasher = sha2::Sha256::new();
    hasher.update(record.prev_hash.as_bytes());
    hasher.update(b"\n");
    hasher.update(json.as_bytes());
    Ok(format!("sha256:{}", hex::encode(hasher.finalize())))
}

/// 超限则 rotate：`v2.jsonl` → `v2.jsonl.1`，旧代顺移，最老一代删除。
///
/// 与 v1 同款语义：只 rename 不 truncate；链哈希跨段连续，与文件边界无关。
fn rotate_if_needed(root: &Path, limit: u64) {
    let path = active_path(root);
    let Ok(meta) = std::fs::metadata(&path) else {
        return; // 不存在：无需 rotate
    };
    if meta.len() < limit {
        return;
    }
    for generation in (1..GENERATIONS).rev() {
        let from = rotated_path(root, generation);
        if from.exists() {
            let to = rotated_path(root, generation + 1);
            let _ = std::fs::remove_file(&to);
            let _ = std::fs::rename(&from, &to);
        }
    }
    let first = rotated_path(root, 1);
    let _ = std::fs::remove_file(&first);
    if let Err(e) = std::fs::rename(&path, &first) {
        // rename 失败（跨卷/权限/占用）：继续 append 到原文件——宁可继续
        // 增长，也不丢记录。
        log::error!("audit v2: rotate {} failed: {e}", path.display());
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 恢复（进程重启 / 崩溃撕裂尾行）
// ─────────────────────────────────────────────────────────────────────────────

/// 从账本尾部恢复链状态：优先活动文件，其次最近的 rotate 代。
fn recover_state(root: &Path, active: &Path) -> ChainState {
    let mut candidates = vec![active.to_path_buf()];
    for generation in 1..=GENERATIONS {
        candidates.push(rotated_path(root, generation));
    }
    for candidate in candidates {
        if let Some(state) = recover_from_file(&candidate) {
            return state;
        }
    }
    ChainState::genesis()
}

/// 读单个文件恢复 (seq, hash)。返回 None = 文件不存在/为空/无一条可解析。
///
/// 崩溃撕裂的尾行（无 `\n` 结尾）先移入 `<file>.torn` 侧车保留证据，再把
/// 活动文件截断到最后一个完整行——不静默丢弃任何字节。
fn recover_from_file(path: &Path) -> Option<ChainState> {
    let mut bytes = std::fs::read(path).ok()?;
    if bytes.is_empty() {
        return None;
    }
    if !bytes.ends_with(b"\n") {
        let cut = bytes
            .iter()
            .rposition(|b| *b == b'\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        let torn = bytes.get(cut..).unwrap_or_default();
        if !torn.is_empty() {
            let mut sidecar = path.as_os_str().to_os_string();
            sidecar.push(".torn");
            let sidecar = PathBuf::from(sidecar);
            if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&sidecar) {
                let _ = file.write_all(torn);
                let _ = file.write_all(b"\n");
            }
            log::warn!(
                "audit v2: repaired torn tail of {} ({} bytes preserved in {})",
                path.display(),
                torn.len(),
                sidecar.display()
            );
        }
        bytes.truncate(cut);
        std::fs::write(path, &bytes).ok()?;
    }
    bytes
        .split(|b| *b == b'\n')
        .rev()
        .filter(|line| !line.is_empty())
        .find_map(|line| serde_json::from_slice::<Record>(line).ok())
        .map(|record| ChainState {
            seq: record.seq,
            hash: record.hash,
        })
}

// ─────────────────────────────────────────────────────────────────────────────
// 校验
// ─────────────────────────────────────────────────────────────────────────────

/// 校验发现的问题（首个即返回；`Display` 给出可读定位）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyProblem {
    /// 尾行无换行符结尾（崩溃撕裂；下次 append 会修复并保留到 `.torn`）。
    TornTail { segment: String, line: u64 },
    /// 行无法解析为 v2 记录。
    Unparsable {
        segment: String,
        line: u64,
        detail: String,
    },
    /// schema 标识不是 `qaqh.audit/v2`。
    SchemaMismatch {
        segment: String,
        line: u64,
        found: String,
    },
    /// 记录内容与自身 hash 不符（篡改/损坏）。
    HashMismatch {
        segment: String,
        line: u64,
        seq: u64,
    },
    /// prev_hash 与上一条记录的 hash 不衔接（删行/换行/跨段丢失）。
    ChainBroken {
        segment: String,
        line: u64,
        seq: u64,
        expected: String,
        found: String,
    },
    /// 序号不连续（丢记录）。
    SeqGap {
        segment: String,
        line: u64,
        expected: u64,
        found: u64,
    },
}

impl std::fmt::Display for VerifyProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TornTail { segment, line } => {
                write!(f, "{segment}:{line}: torn tail (unterminated last line)")
            }
            Self::Unparsable {
                segment,
                line,
                detail,
            } => write!(f, "{segment}:{line}: unparsable record ({detail})"),
            Self::SchemaMismatch {
                segment,
                line,
                found,
            } => write!(f, "{segment}:{line}: schema mismatch (found {found:?})"),
            Self::HashMismatch { segment, line, seq } => {
                write!(f, "{segment}:{line}: seq={seq} hash mismatch (tampered?)")
            }
            Self::ChainBroken {
                segment,
                line,
                seq,
                expected,
                found,
            } => write!(
                f,
                "{segment}:{line}: seq={seq} chain broken (expected prev {expected:?}, found {found:?})"
            ),
            Self::SeqGap {
                segment,
                line,
                expected,
                found,
            } => write!(
                f,
                "{segment}:{line}: seq gap (expected {expected}, found {found})"
            ),
        }
    }
}

/// 校验结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyReport {
    pub ok: bool,
    pub records: u64,
    pub first_seq: Option<u64>,
    pub last_seq: Option<u64>,
    /// 实际参与校验的段（按时间从旧到新）。
    pub segments: Vec<String>,
    /// 历史被 rotate 截断（首条 seq > 1）——不是错误，是事实标注。
    pub truncated_history: bool,
    pub problem: Option<VerifyProblem>,
}

/// 校验整个账本：按段序（`.3 → .2 → .1 → 活动`）逐条检查 schema、链哈希、
/// prev_hash 衔接与 seq 连续性。
pub fn verify_ledger(root: &Path) -> VerifyReport {
    let mut report = VerifyReport {
        ok: true,
        records: 0,
        first_seq: None,
        last_seq: None,
        segments: Vec::new(),
        truncated_history: false,
        problem: None,
    };
    let mut segments: Vec<(String, PathBuf)> = Vec::new();
    for generation in (1..=GENERATIONS).rev() {
        let path = rotated_path(root, generation);
        if path.exists() {
            segments.push((format!("{ACTIVE_FILE}.{generation}"), path));
        }
    }
    let active = active_path(root);
    if active.exists() {
        segments.push((ACTIVE_FILE.to_string(), active));
    }

    let mut prev: Option<(u64, String)> = None;
    for (name, path) in segments {
        report.segments.push(name.clone());
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) => {
                report.ok = false;
                report.problem = Some(VerifyProblem::Unparsable {
                    segment: name,
                    line: 0,
                    detail: e.to_string(),
                });
                return report;
            }
        };
        if bytes.is_empty() {
            continue;
        }
        let torn_tail = !bytes.ends_with(b"\n");
        let mut lines: Vec<&[u8]> = bytes.split(|b| *b == b'\n').collect();
        if torn_tail {
            // 残缺尾行单独报告（不参与链校验）。
            lines.pop();
        }
        for (index, line) in lines.iter().enumerate() {
            if line.is_empty() {
                continue;
            }
            let line_no = index as u64 + 1;
            let record: Record = match serde_json::from_slice(line) {
                Ok(record) => record,
                Err(e) => {
                    report.ok = false;
                    report.problem = Some(VerifyProblem::Unparsable {
                        segment: name,
                        line: line_no,
                        detail: e.to_string(),
                    });
                    return report;
                }
            };
            if record.schema != SCHEMA {
                report.ok = false;
                report.problem = Some(VerifyProblem::SchemaMismatch {
                    segment: name,
                    line: line_no,
                    found: record.schema,
                });
                return report;
            }
            match record_hash(&record) {
                Ok(expected) if expected != record.hash => {
                    report.ok = false;
                    report.problem = Some(VerifyProblem::HashMismatch {
                        segment: name,
                        line: line_no,
                        seq: record.seq,
                    });
                    return report;
                }
                Ok(_) => {}
                Err(e) => {
                    report.ok = false;
                    report.problem = Some(VerifyProblem::Unparsable {
                        segment: name,
                        line: line_no,
                        detail: e.to_string(),
                    });
                    return report;
                }
            }
            if let Some((prev_seq, prev_hash)) = &prev {
                if record.prev_hash != *prev_hash {
                    report.ok = false;
                    report.problem = Some(VerifyProblem::ChainBroken {
                        segment: name,
                        line: line_no,
                        seq: record.seq,
                        expected: prev_hash.clone(),
                        found: record.prev_hash.clone(),
                    });
                    return report;
                }
                if record.seq != prev_seq + 1 {
                    report.ok = false;
                    report.problem = Some(VerifyProblem::SeqGap {
                        segment: name,
                        line: line_no,
                        expected: prev_seq + 1,
                        found: record.seq,
                    });
                    return report;
                }
            } else {
                report.truncated_history = record.seq != 1 || !record.prev_hash.is_empty();
            }
            if report.first_seq.is_none() {
                report.first_seq = Some(record.seq);
            }
            report.last_seq = Some(record.seq);
            report.records += 1;
            prev = Some((record.seq, record.hash.clone()));
        }
        if torn_tail {
            report.ok = false;
            report.problem = Some(VerifyProblem::TornTail {
                segment: name,
                line: lines.len() as u64 + 1,
            });
            return report;
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn actor(call_id: &str) -> Actor {
        Actor {
            kind: ActorKind::Agent,
            user: "tester".to_string(),
            session: "s1".to_string(),
            call_id: call_id.to_string(),
            sandbox: false,
        }
    }

    fn event(call_id: &str) -> Event {
        Event {
            kind: AuditKind::ToolCall,
            actor: actor(call_id),
            tool: Some(ToolInfo {
                name: "read".to_string(),
                action: "run".to_string(),
                category: "read".to_string(),
                effective_name: None,
                permission_level: Some(4),
            }),
            decision: Some(Decision {
                outcome: "auto".to_string(),
                reason: None,
            }),
            args: ArgsMeta {
                hash: "deadbeef".to_string(),
                bytes: 12,
            },
            result: Some(ResultMeta {
                status: "ok".to_string(),
                error_code: None,
                elapsed_ms: 3,
                output_bytes: 5,
                retry_count: 0,
            }),
            objects: vec![AuditObject {
                kind: "file".to_string(),
                path: "src/lib.rs".to_string(),
                before_sha: Some("b1".to_string()),
                after_sha: Some("a1".to_string()),
            }],
        }
    }

    fn read_lines(path: &Path) -> Vec<String> {
        std::fs::read_to_string(path)
            .expect("read ledger")
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// 清空进程内链缓存，模拟"新进程首次 append"的恢复路径。
    fn clear_chain_cache() {
        CHAIN.lock().unwrap_or_else(|p| p.into_inner()).clear();
    }

    #[test]
    fn chain_links_and_verifies() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let first = append_event(root, event("c1")).expect("append first");
        let second = append_event(root, event("c2")).expect("append second");

        assert_eq!(first.seq, 1);
        assert_eq!(first.prev_hash, "");
        assert!(!first.hash.is_empty(), "hash must be stamped");
        assert_eq!(second.seq, 2);
        assert_eq!(second.prev_hash, first.hash);
        assert_ne!(second.hash, first.hash);

        let report = verify_ledger(root);
        assert!(report.ok, "verify must pass: {report:?}");
        assert_eq!(report.records, 2);
        assert_eq!(report.first_seq, Some(1));
        assert_eq!(report.last_seq, Some(2));
        assert!(!report.truncated_history);
    }

    #[test]
    fn tampered_record_breaks_verification() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        for call in ["c1", "c2", "c3"] {
            append_event(root, event(call)).expect("append");
        }
        let path = active_path(root);
        let mut lines = read_lines(&path);
        let mut record: serde_json::Value =
            serde_json::from_str(&lines[1]).expect("parse second record");
        record["result"]["status"] = serde_json::json!("error");
        lines[1] = serde_json::to_string(&record).expect("encode tampered record");
        std::fs::write(&path, format!("{}\n", lines.join("\n"))).expect("write tampered ledger");

        let report = verify_ledger(root);
        assert!(!report.ok, "tamper must be detected");
        match report.problem {
            Some(VerifyProblem::HashMismatch { seq, .. }) => assert_eq!(seq, 2),
            other => panic!("expected hash mismatch, got {other:?}"),
        }
    }

    #[test]
    fn deleted_record_breaks_the_chain() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        for call in ["c1", "c2", "c3"] {
            append_event(root, event(call)).expect("append");
        }
        let path = active_path(root);
        let lines = read_lines(&path);
        let kept = format!("{}\n{}\n", lines[0], lines[2]);
        std::fs::write(&path, kept).expect("write ledger without middle record");

        let report = verify_ledger(root);
        assert!(!report.ok, "deleted record must be detected");
        match report.problem {
            Some(VerifyProblem::ChainBroken { seq, .. }) => assert_eq!(seq, 3),
            other => panic!("expected chain break, got {other:?}"),
        }
    }

    #[test]
    fn rotation_keeps_the_chain_across_segments() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        // limit=1：第二条起每次 append 前都触发 rotate（5 条 → .3/.2/.1/活动）。
        for index in 0..5 {
            append_event_with_limit(root, event(&format!("c{index}")), 1).expect("append");
        }
        for generation in 1..=GENERATIONS {
            assert!(
                rotated_path(root, generation).exists(),
                "generation {generation} must exist"
            );
        }
        let report = verify_ledger(root);
        assert!(report.ok, "chain must survive rotation: {report:?}");
        assert_eq!(report.records, 4, "oldest generation is dropped by design");
        assert_eq!(report.first_seq, Some(2));
        assert!(
            report.truncated_history,
            "seq starts above 1 after rotation"
        );
        assert_eq!(report.segments.len(), 4);
    }

    #[test]
    fn restart_recovers_chain_from_tail() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let first = append_event(root, event("c1")).expect("append first");
        let second = append_event(root, event("c2")).expect("append second");

        clear_chain_cache(); // 模拟进程重启（内存态丢失）

        let third = append_event(root, event("c3")).expect("append after restart");
        assert_eq!(third.seq, 3, "seq must continue across restarts");
        assert_eq!(third.prev_hash, second.hash, "chain must continue");
        assert_eq!(first.seq + 2, third.seq);

        let report = verify_ledger(root);
        assert!(report.ok, "verify must pass after restart: {report:?}");
        assert_eq!(report.records, 3);
    }

    #[test]
    fn torn_tail_is_preserved_and_repaired() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let first = append_event(root, event("c1")).expect("append first");
        let second = append_event(root, event("c2")).expect("append second");

        let path = active_path(root);
        let mut bytes = std::fs::read(&path).expect("read ledger");
        bytes.extend_from_slice(b"{\"schema\":\"qaqh.audit/v2\",\"seq\":3");
        std::fs::write(&path, bytes).expect("write torn tail");

        clear_chain_cache(); // 重启后首次 append 走恢复路径

        let third = append_event(root, event("c3")).expect("append after torn tail");
        assert_eq!(third.seq, 3);
        assert_eq!(third.prev_hash, second.hash);
        assert_eq!(first.seq, 1);

        let report = verify_ledger(root);
        assert!(report.ok, "repaired ledger must verify: {report:?}");
        assert_eq!(report.records, 3);

        let mut sidecar = path.as_os_str().to_os_string();
        sidecar.push(".torn");
        let torn = std::fs::read_to_string(PathBuf::from(sidecar)).expect("torn sidecar exists");
        assert!(
            torn.contains("qaqh.audit/v2"),
            "torn bytes must be preserved: {torn:?}"
        );
    }

    #[test]
    fn verify_reports_torn_tail_without_repair() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        append_event(root, event("c1")).expect("append");
        let path = active_path(root);
        let mut bytes = std::fs::read(&path).expect("read");
        bytes.extend_from_slice(b"{\"half\":");
        std::fs::write(&path, bytes).expect("write torn tail");

        let report = verify_ledger(root);
        assert!(!report.ok);
        assert!(
            matches!(report.problem, Some(VerifyProblem::TornTail { .. })),
            "got {:?}",
            report.problem
        );
        // 校验不修改账本：撕裂字节仍在原文件。
        let raw = std::fs::read_to_string(&path).expect("read after verify");
        assert!(raw.contains("{\"half\":"));
    }

    #[cfg(unix)]
    #[test]
    fn ledger_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        append_event(root, event("c1")).expect("append");
        let mode = std::fs::metadata(active_path(root))
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "audit ledger must be owner-only, got {mode:o}");
    }
}
