//! 审计账本 v2 端到端验收（工具调用 → 双写账本 → 链校验）。
//!
//! 设计：`docs/spec/2026-09-19-审计账本v2-spec.md` §6 验收矩阵。覆盖：
//! 1. 成功调用：v2 记录带主体链（session/call_id）、工具面（category）、
//!    授权路径（decision=auto）、对象（路径 + after 指纹）；
//! 2. 被拒调用（Level 1 写操作需审批、通道不可用）：kind=tool_rejected、
//!    decision=challenge_required、error_code=PERMISSION_DENIED；
//! 3. PLAN 模式前置阻断：error_code=BLOCKED_BY_MODE；
//! 4. 失败终态：result.status=error + machine-readable error_code；
//! 5. 隐私：参数明文（content 里的敏感串）绝不落账本；
//! 6. 完整性：seq 连续、prev_hash 逐条衔接、verify_ledger 全绿；
//! 7. 双写：v1 CSV 同步落行（8 旧列 + 7 扩展列 = 15 列）。
//!
//! 隔离：本测试进程独占 `QAQH_DATA_DIR`/`HOME`/`USERPROFILE`（见 [`setup`]），
//! 绝不写真实用户账本；文件内测试以 `SERIAL` 串行（env 是进程级状态）。

use std::path::PathBuf;
use std::sync::Mutex;

/// env/全局 manager 是进程级状态：本文件的测试必须串行。
static SERIAL: Mutex<()> = Mutex::new(());

struct Env {
    _tmp: tempfile::TempDir,
    data: PathBuf,
    workspace: PathBuf,
}

/// 钉住数据根与工作区，初始化工具 manager。
///
/// `data_dir()` 的优先级是 `QAQH_DATA_DIR` > `HOME`/`USERPROFILE`（unix/win），
/// 全部钉住才能保证账本/会话/workspace 落进临时目录。
fn setup(label: &str) -> Env {
    let tmp = tempfile::tempdir().expect("tempdir");
    let data = tmp.path().join(format!("{label}-data"));
    let workspace = tmp.path().join(format!("{label}-ws"));
    std::fs::create_dir_all(&data).expect("create data dir");
    std::fs::create_dir_all(&workspace).expect("create workspace");
    // SAFETY: 本文件所有测试经 SERIAL 串行，且此处在任何工具调用之前设置。
    unsafe {
        std::env::set_var("QAQH_DATA_DIR", &data);
        std::env::set_var("HOME", tmp.path());
        std::env::set_var("USERPROFILE", tmp.path());
    }
    qaqh_workspace::set_workspace(&workspace.to_string_lossy());
    qaqh_workspace::runtime::init_tools(label, &[], vec![]);
    qaqh_workspace::runtime::set_context(label, 4);
    Env {
        _tmp: tmp,
        data,
        workspace,
    }
}

fn call(name: &str, args: serde_json::Value, call_id: &str, ctx: &qaqh_workspace::runtime::ToolCtx) -> qaqh_workspace::execution::ToolExecResult {
    qaqh_workspace::execution::execute_with_context(
        name,
        "",
        &args.to_string(),
        call_id,
        None,
        ctx,
    )
}

#[test]
fn audit_ledger_traces_calls_end_to_end() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let env = setup("audit-ledger");
    let ctx = qaqh_workspace::runtime::ToolCtx::admitted("audit-ledger");

    // ① 成功写入：参数含敏感串 → 账本只存 hash，绝不落明文。
    let secret = "AUDIT_SECRET_SHOULD_NEVER_APPEAR_42";
    let target = env.workspace.join("secret.txt");
    let write = call(
        "write",
        serde_json::json!({"path": target, "content": secret}),
        "call-1",
        &ctx,
    );
    assert!(write.success, "write must succeed: {}", write.content);

    // ② 被拒调用：Level 1（MaxLockdown）写操作需审批，本通道无审批 → 拒绝。
    let locked = qaqh_workspace::runtime::ToolCtx {
        session_id: "audit-ledger".to_string(),
        permission_level: 1,
        mode: 0,
        workspace_root: None,
    };
    let denied = call(
        "write",
        serde_json::json!({"path": env.workspace.join("denied.txt"), "content": "x"}),
        "call-2",
        &locked,
    );
    assert!(!denied.success, "Level 1 write must require approval");

    // ③ PLAN 模式前置阻断（edit 在 PLAN_BLOCKED 名单内）。
    let plan = qaqh_workspace::runtime::ToolCtx {
        session_id: "audit-ledger".to_string(),
        permission_level: 4,
        mode: 1,
        workspace_root: None,
    };
    let blocked = call(
        "edit",
        serde_json::json!({"path": env.workspace.join("plan.txt"), "old_str": "a", "new_str": "b"}),
        "call-3",
        &plan,
    );
    assert!(!blocked.success, "PLAN mode must block edit");

    // ④ 失败终态：读不存在的文件 → 执行到终态但 status=error。
    let missing = call(
        "read",
        serde_json::json!({"path": env.workspace.join("missing.txt")}),
        "call-4",
        &ctx,
    );
    assert!(!missing.success, "missing file must fail");

    // ── 账本断言 ──────────────────────────────────────────────────────────
    let ledger = env.data.join("audit").join("v2.jsonl");
    let raw = std::fs::read_to_string(&ledger).expect("read v2 ledger");
    let records: Vec<serde_json::Value> = raw
        .lines()
        .map(|line| serde_json::from_str(line).expect("parse v2 record"))
        .collect();
    assert_eq!(records.len(), 4, "every call must be audited: {raw}");

    // ① 成功调用：主体链 + 工具面 + 决策 + 对象。
    let first = &records[0];
    assert_eq!(first["schema"], "qaqh.audit/v2");
    assert_eq!(first["kind"], "tool_call");
    assert_eq!(first["seq"], 1);
    assert_eq!(first["actor"]["session"], "audit-ledger");
    assert_eq!(first["actor"]["call_id"], "call-1");
    assert_eq!(first["actor"]["sandbox"], false);
    assert_eq!(first["tool"]["name"], "write");
    assert_eq!(first["tool"]["category"], "write");
    assert_eq!(first["tool"]["permission_level"], 4);
    assert_eq!(first["decision"]["outcome"], "auto");
    assert_eq!(first["result"]["status"], "ok");
    assert!(
        first["args"]["hash"].as_str().map(str::len) == Some(64),
        "args hash must be a sha256 hex digest: {first}"
    );
    let objects = first["objects"].as_array().expect("objects array");
    assert!(
        objects.iter().any(|object| object["path"]
            .as_str()
            .map(|path| path.ends_with("secret.txt"))
            .unwrap_or(false)),
        "object path missing: {first}"
    );
    assert!(
        objects.iter().any(|object| object["after_sha"].is_string()),
        "object after-fingerprint missing: {first}"
    );

    // ② 拒绝记录：需要审批但通道不可用。
    let denied_record = &records[1];
    assert_eq!(denied_record["kind"], "tool_rejected");
    assert_eq!(denied_record["actor"]["call_id"], "call-2");
    assert_eq!(denied_record["decision"]["outcome"], "challenge_required");
    assert_eq!(denied_record["result"]["error_code"], "PERMISSION_DENIED");
    assert_eq!(denied_record["tool"]["permission_level"], 1);

    // ③ PLAN 阻断。
    let blocked_record = &records[2];
    assert_eq!(blocked_record["kind"], "tool_rejected");
    assert_eq!(blocked_record["result"]["error_code"], "BLOCKED_BY_MODE");

    // ④ 失败终态：执行层错误码透传。
    let failed_record = &records[3];
    assert_eq!(failed_record["kind"], "tool_call");
    assert_eq!(failed_record["result"]["status"], "error");
    assert_eq!(failed_record["result"]["error_code"], "NOT_FOUND");

    // 时间与链：seq 连续 + prev_hash 逐条衔接（时间字段必须齐备）。
    for (index, record) in records.iter().enumerate() {
        assert_eq!(record["seq"], (index + 1) as u64, "seq must be contiguous");
        assert!(record["ts"].is_string(), "wall clock missing: {record}");
        assert!(record["ts_mono_ns"].is_u64(), "mono clock missing: {record}");
        assert!(
            record["boot_id"].as_str().map(str::is_empty) == Some(false),
            "boot id missing: {record}"
        );
        if index > 0 {
            assert_eq!(
                record["prev_hash"], records[index - 1]["hash"],
                "chain must link record {index}"
            );
        }
    }

    // 隐私：参数明文绝不出现。
    assert!(
        !raw.contains(secret),
        "raw args must never be written to the ledger"
    );

    // 完整性：链校验全绿。
    let report = qaqh_workspace::audit::v2::verify_ledger(&env.data.join("audit"));
    assert!(report.ok, "verify must pass: {report:?}");
    assert_eq!(report.records, 4);
    assert_eq!(report.first_seq, Some(1));
    assert_eq!(report.last_seq, Some(4));

    // 双写：v1 CSV 同步落 4 行、15 列。
    let csv = std::fs::read_to_string(env.data.join("audit.csv")).expect("read csv");
    let rows: Vec<&str> = csv.lines().collect();
    assert_eq!(rows.len(), 4, "csv must mirror every call: {csv}");
    for row in rows {
        assert_eq!(row.split(',').count(), 15, "row: {row}");
    }
}
