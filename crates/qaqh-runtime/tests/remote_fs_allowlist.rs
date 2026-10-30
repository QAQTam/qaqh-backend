//! T-2-1 回归：`fs.list` / `fs.read` 路径白名单（安全审查 P0-1）。
//!
//! 入口是 `QaqhService::handle`——daemon `/ringing/v2/service/{method}` 的唯一
//! 分发点——因此这里覆盖的是真实服务路径，而不是 `fs_git` 的私有函数。
//!
//! 隔离：整个用例把 `QAQH_DATA_DIR` 指向临时目录（`SessionManager::init`
//! 与 `data_dir()` 同源），绝不触达用户真实 `~/.qaqh`。二进制内测试按
//! `.cargo/config.toml` 单线程执行，`set_var` 不会与其它用例竞争。

use std::sync::OnceLock;

use serde_json::json;

static SERVICE: OnceLock<qaqh_runtime::QaqhService> = OnceLock::new();
static WORKSPACE: OnceLock<std::path::PathBuf> = OnceLock::new();
/// 会话 id 现在是 canonical UUID（目录名 == SessionId == seed）。
static SEED: OnceLock<String> = OnceLock::new();

fn seed() -> &'static str {
    SEED.get().expect("session created in service()")
}

fn workspace() -> &'static std::path::Path {
    WORKSPACE.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("qaqh-fs-allowlist-ws-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir workspace");
        dir
    })
}

fn service() -> &'static qaqh_runtime::QaqhService {
    SERVICE.get_or_init(|| {
        let root = std::env::temp_dir().join(format!("qaqh-fs-allowlist-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("mkdir data root");
        unsafe {
            std::env::set_var("QAQH_DATA_DIR", root.join("data"));
        }
        qaqh_session::SessionManager::init(qaqh_types::platform::data_dir());
        let manager = qaqh_session::SessionManager::global();
        // 建一个会话并写创世事实：workspace() 经 `SessionCreated.cwd` 成为
        // 该会话的工作根（meta.json 已退役，cwd 只能来自 journal）。
        let workspace_text = workspace().to_str().expect("utf8 workspace");
        let identity = manager
            .allocate_session(Some(workspace_text))
            .expect("allocate session");
        let session_id = identity.session_id.as_str().to_string();
        let dir = manager.session_path_dir(&session_id);
        let now_ms = 1_759_000_000_000i64;
        {
            use qaqh_session::session_fact_v2::{
                EventId, FactPayload, FactSchema, SessionCreated, SessionFact,
            };
            let mut commit = qaqh_session::session_commit::SessionCommit::open_or_create(
                &dir,
                identity.session_id.clone(),
                identity.log_id.clone(),
                qaqh_session::meta::default_context_budget(),
            )
            .expect("open session journal");
            let fact = SessionFact {
                schema: FactSchema::v2(),
                session_id: identity.session_id.clone(),
                log_id: identity.log_id.clone(),
                fact_seq: 0,
                event_id: EventId::new(qaqh_session::canonical::generate_ulid()),
                ts_ms: now_ms,
                causation_id: None,
                turn_id: None,
                call_id: None,
                interaction_id: None,
                payload: FactPayload::SessionCreated(SessionCreated {
                    created_at_ms: now_ms,
                    cwd: workspace_text.to_string(),
                    model: "test-model".to_string(),
                    parent_session_id: None,
                    schema_caps: Vec::new(),
                }),
            };
            commit
                .commit("session-created", 0, vec![fact])
                .expect("commit genesis");
        }
        qaqh_session::meta::refresh_index(&dir);
        let _ = SEED.set(session_id);
        qaqh_runtime::QaqhService::init(manager)
    })
}

fn read(path: &str) -> qaqh_runtime::QaqhResult<serde_json::Value> {
    service().handle("fs.read", &json!({ "path": path }))
}

fn list(path: &str) -> qaqh_runtime::QaqhResult<serde_json::Value> {
    service().handle("fs.list", &json!({ "path": path }))
}

fn read_scoped(path: &str, session_id: &str) -> qaqh_runtime::QaqhResult<serde_json::Value> {
    service().handle(
        "fs.read",
        &json!({ "path": path, "scope_session_id": session_id }),
    )
}

/// 反向：会话工作区（允许根之一）必须仍可浏览，白名单不能收得过窄。
#[test]
fn fs_list_allows_session_workspace() {
    std::fs::write(workspace().join("hello.txt"), "hi\n").expect("seed file");
    let entries = list(workspace().to_str().expect("utf8 workspace"))
        .expect("session workspace must stay listable");
    assert!(entries.is_array(), "{entries}");
}

/// 反向：工作区内的普通文件必须仍可读。
#[test]
fn fs_read_allows_workspace_file() {
    let file = workspace().join("hello.txt");
    let value = read(file.to_str().expect("utf8 file")).expect("workspace file must stay readable");
    assert_eq!(value["content"], json!("hi\n"));
}

/// `..` 逃逸不得借允许根前缀混过去。
#[test]
fn fs_read_rejects_dotdot_escape() {
    let escaped = workspace().join("..").join("etc").join("passwd");
    let error =
        read(escaped.to_str().expect("utf8 escape")).expect_err("dotdot escape must be rejected");
    assert!(error.is_forbidden(), "{error}");
}

/// 任意绝对路径（非允许根）必须被拒。
#[test]
fn fs_read_rejects_arbitrary_absolute_path() {
    let error = read("/etc/hostname").expect_err("/etc/hostname must be rejected");
    assert!(error.is_forbidden(), "{error}");
}

/// WebUI 网关注入 `scope_session_id` 后，读取必须同时落在该会话的 cwd 内。
#[test]
fn fs_read_scope_allows_only_active_session_workspace() {
    let file = workspace().join("hello.txt");
    let value =
        read_scoped(file.to_str().expect("utf8 file"), seed()).expect("active seed file must read");
    assert_eq!(value["content"], json!("hi\n"));
}

/// 其它会话的 cwd 即使存在于全局 allowed_roots，也不能越过 active session scope。
#[test]
fn fs_read_scope_rejects_other_session_workspace() {
    let other_workspace =
        std::env::temp_dir().join(format!("qaqh-fs-allowlist-other-{}", std::process::id()));
    std::fs::create_dir_all(&other_workspace).expect("mkdir other workspace");
    let other_file = other_workspace.join("secret.txt");
    std::fs::write(&other_file, "other\n").expect("seed other file");
    let other_session = "fs-other-seed";
    assert!(
        qaqh_session::SessionManager::global().persist_new_session_if_absent_with(
            other_session,
            Some(other_workspace.to_str().expect("utf8 other workspace")),
            |_| true,
        )
    );

    let error = read_scoped(other_file.to_str().expect("utf8 other file"), seed())
        .expect_err("cross-seed workspace read must be rejected");
    assert!(error.is_forbidden(), "{error}");
}
