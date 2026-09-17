//! T-2-1 回归：`fs.list` / `fs.read` 路径白名单（安全审查 P0-1）。
//!
//! 入口是 `QaqhService::handle`——daemon `/ringing/v1/service/{method}` 的唯一
//! 分发点——因此这里覆盖的是真实服务路径，而不是 `fs_git` 的私有函数。
//!
//! 隔离：整个用例把 `QAQH_DATA_DIR` 指向临时目录（`SessionManager::init`
//! 与 `data_dir()` 同源），绝不触达用户真实 `~/.qaqh`。二进制内测试按
//! `.cargo/config.toml` 单线程执行，`set_var` 不会与其它用例竞争。

use std::sync::OnceLock;

use serde_json::json;

static SERVICE: OnceLock<qaqh_runtime::QaqhService> = OnceLock::new();
static WORKSPACE: OnceLock<std::path::PathBuf> = OnceLock::new();

const SEED: &str = "fs-allowlist-seed";

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
        // 建一个会话，把 workspace() 经 meta.cwd 登记为允许根。
        assert!(manager.persist_new_session_if_absent_with(
            SEED,
            Some(workspace().to_str().expect("utf8 workspace")),
            |_| true,
        ));
        qaqh_runtime::QaqhService::init(manager)
    })
}

fn read(path: &str) -> Result<serde_json::Value, String> {
    service().handle("fs.read", &json!({ "path": path }))
}

fn list(path: &str) -> Result<serde_json::Value, String> {
    service().handle("fs.list", &json!({ "path": path }))
}

/// `meta.json`（会话持久态）必须被拒，且拒绝是白名单/边界错误而非 IO 失败。
#[test]
fn fs_read_rejects_meta_json() {
    let meta = qaqh_types::platform::data_dir()
        .join("sessions")
        .join(SEED)
        .join("meta.json");
    let error = read(meta.to_str().expect("utf8 meta")).expect_err("meta.json must be rejected");
    assert!(
        error.starts_with("FORBIDDEN"),
        "expected a FORBIDDEN rejection, got: {error}"
    );
    assert!(
        !error.contains("No such file") && !error.contains("os error"),
        "rejection must not surface as an IO failure: {error}"
    );
}

/// `sessions/` 目录本身也必须被拒（`fs.list` 不得枚举会话命名空间）。
#[test]
fn fs_list_rejects_sessions_dir() {
    let sessions = qaqh_types::platform::data_dir().join("sessions");
    let error =
        list(sessions.to_str().expect("utf8 sessions")).expect_err("sessions dir must be rejected");
    assert!(
        error.starts_with("FORBIDDEN"),
        "expected a FORBIDDEN rejection, got: {error}"
    );
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
    assert!(error.starts_with("FORBIDDEN"), "{error}");
}

/// 任意绝对路径（非允许根）必须被拒。
#[test]
fn fs_read_rejects_arbitrary_absolute_path() {
    let error = read("/etc/hostname").expect_err("/etc/hostname must be rejected");
    assert!(error.starts_with("FORBIDDEN"), "{error}");
}
