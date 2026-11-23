//! v4.4 `P1-0b` 反证守卫（宪法 #18）：`session.list` 不得持 `registry` 中央锁
//! 跨越 SQLite IO —— 慢库读只许在**自身**路径上排队，不许把纯内存的
//! `session.activity` 一起钉死（`LOCK_AUDIT_REPORT` L-01.2 复合停摆）。
//!
//! 装置零 mock、零产品代码钩子：`Db::read_blocking` 的本体就是
//! `read_conn.lock()` + 执行闭包，所以另起一个线程用它把只读连接占住
//! `FUSE_HOLD_MS`，等价于一次慢盘 / WAL checkpoint 的持库窗口。
//!
//! - **旧实现红**：`list_sessions` 第一行就取 `registry` guard，随后卡在
//!   `read_blocking` 上，guard 活过整个占库窗口 ⇒ 纯内存的 `session.activity`
//!   被拖到接近 `FUSE_HOLD_MS`，最后一条断言当场红。
//! - **新实现绿**：`registry` 锁只圈住一次内存运行态快照
//!   （`AgentRegistry::run_state_snapshot`），两次 `read_blocking`、逐行
//!   `session_run_status` 与工作区自动绑定的 `write_blocking` 全在锁外
//!   ⇒ `session.activity` 亚毫秒返回。

use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;

use qaqh_runtime::QaqhService;

/// 只读连接被占住的窗口，只需显著大于快路径阈值，无需精确。
const FUSE_HOLD_MS: u64 = 400;
/// 快路径上限：新实现是亚毫秒纯内存读，旧实现被拖到接近整个占库窗口。
const FAST_PATH_MAX_MS: u64 = 150;
/// `session.list` 至少要排队这么久，用来证明引信确实落在它的读路径里
/// （否则本用例退化成空断言，必须当场暴露而不是默默变绿）。
const LIST_QUEUED_MIN_MS: u64 = 100;
/// 让 `session.list` 取完锁并停在 `read_blocking` 上的观察窗口，不参与断言。
const DISPATCH_SETTLE_MS: u64 = 30;

static SERVICE: std::sync::OnceLock<QaqhService> = std::sync::OnceLock::new();

fn service() -> &'static QaqhService {
    SERVICE.get_or_init(|| {
        static TMP: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
        let tmp = TMP.get_or_init(|| {
            let tmp = tempfile::tempdir().expect("isolated data root tempdir");
            // SAFETY: OnceLock 初始化窗口内的单次进程级设置，此后只读。
            unsafe {
                std::env::set_var("QAQH_DATA_DIR", tmp.path().join(".qaqh"));
                std::env::set_var("HOME", tmp.path());
                std::env::set_var("USERPROFILE", tmp.path());
            }
            tmp
        });
        let _ = tmp;
        QaqhService::init(qaqh_types::platform::data_dir())
    })
}

/// 把 SQLite 只读连接占住 `FUSE_HOLD_MS`。返回的 receiver 在闭包**已进入
/// 持锁区间**时收到信号，测试据此决定何时让慢路径起跑。
fn hold_read_connection(db: Arc<qaqh_session::db::Db>) -> mpsc::Receiver<()> {
    let (engaged_tx, engaged_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = db.read_blocking(move |_conn| {
            let _ = engaged_tx.send(());
            std::thread::sleep(Duration::from_millis(FUSE_HOLD_MS));
            Ok::<(), qaqh_session::db::DbError>(())
        });
    });
    engaged_rx
}

#[test]
fn list_sessions_does_not_hold_registry_lock_across_db_io() {
    let service = service();
    // 预热：首次调用带懒装配（config / provider 目录），排除在计时窗口之外。
    service
        .handle("session.list", &json!({ "include_archived": true }))
        .expect("warmup session.list");

    let db = Arc::clone(service.owners().db());
    let engaged = hold_read_connection(db);
    engaged
        .recv_timeout(Duration::from_secs(5))
        .expect("read-connection fuse never engaged");

    // 慢路径：只读连接被占住，`session.list` 必然卡在它自己的 `read_blocking`。
    let list_service = service.clone();
    let list = std::thread::spawn(move || {
        let started = Instant::now();
        let result = list_service.handle("session.list", &json!({ "include_archived": true }));
        (started.elapsed(), result)
    });

    // 观察窗口：让慢路径取完 registry 锁并停在 read_blocking 上再开始计时。
    std::thread::sleep(Duration::from_millis(DISPATCH_SETTLE_MS));

    // 快路径：`session.activity` 只读 registry 内存态，应当亚毫秒返回。
    let probe_started = Instant::now();
    service
        .handle("session.activity", &json!({}))
        .expect("session.activity");
    let probe_elapsed = probe_started.elapsed();

    let (list_elapsed, list_result) = list.join().expect("session.list thread panicked");
    list_result.expect("session.list must succeed once the read connection frees");
    assert!(
        list_elapsed >= Duration::from_millis(LIST_QUEUED_MIN_MS),
        "反证装置失效：session.list 没有落在被占住的只读连接上（{list_elapsed:?}），\
         本次运行无法判定锁边界，请检查 read_blocking 路径而不是放宽阈值"
    );
    assert!(
        probe_elapsed < Duration::from_millis(FAST_PATH_MAX_MS),
        "L-01.2 复合停摆回归：session.list 持 registry 锁跨越 SQLite 读，把纯内存的 \
         session.activity 一起钉死（探针 {probe_elapsed:?} / 上限 \
         {FAST_PATH_MAX_MS}ms，只读连接被占 {FUSE_HOLD_MS}ms）"
    );
}
