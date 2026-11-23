//! MUST 铁闸 2 的实测四件:行数核对 / 墓碑改名 / 二次启动幂等 / 故障回滚。
//!
//! 这些用例只走公开入口 `Db::open`——吸入语义就是启动期语义,不用任何
//! test-only 后门。

use std::fs;
use std::path::Path;

use qaqh_session::db::Db;

fn count(db: &Db, sql: &str) -> i64 {
    db.read_blocking(|conn| conn.query_row(sql, [], |row| row.get::<_, i64>(0)))
        .expect("count query")
}

fn seed_legacy_files(dir: &Path) {
    fs::create_dir_all(dir.join("sessions/SEEDAAA")).expect("mkdir sessions/SEEDAAA");
    fs::create_dir_all(dir.join("sessions/SEEDBBB")).expect("mkdir sessions/SEEDBBB");
    fs::create_dir_all(dir.join("quota/ROOTX")).expect("mkdir quota/ROOTX");
    // 三行 todo:第三行无 id(应回落 title),含 evidence。
    fs::write(
        dir.join("sessions/SEEDAAA/todo.json"),
        "{\"items\":[{\"id\":\"1\",\"title\":\"t1\",\"description\":\"\",\"status\":\"pending\"},{\"id\":\"2\",\"title\":\"t2\",\"description\":\"d\",\"status\":\"in_progress\",\"evidence\":\"ev\"},{\"title\":\"t3\",\"status\":\"completed\"}],\"mode\":\"goal\",\"current_id\":\"2\",\"auto_turns\":3,\"max_auto_turns\":24,\"archived\":false,\"cleared_notified\":true}",
    )
    .expect("write todo AAA");
    // 空清单也要保住状态位(state 恒一行)。
    fs::write(
        dir.join("sessions/SEEDBBB/todo.json"),
        "{\"items\":[],\"mode\":\"manual\",\"current_id\":null,\"auto_turns\":0,\"max_auto_turns\":24,\"archived\":true,\"cleared_notified\":false}",
    )
    .expect("write todo BBB");
    fs::write(
        dir.join("ringing-devices.json"),
        "[{\"device_id\":\"dev1\",\"name\":\"phone\",\"platform\":\"android\",\"scope\":\"interact\",\"token_digest\":\"digest1\",\"created_at_ms\":1,\"last_seen_ms\":2},{\"device_id\":\"dev2\",\"name\":\"pad\",\"platform\":\"harmonyos\",\"scope\":\"view\",\"token_digest\":\"digest2\",\"created_at_ms\":3,\"last_seen_ms\":4}]",
    )
    .expect("write devices");
    fs::write(
        dir.join("ringing-command-receipts.json"),
        "{\"cmd1\":{\"fingerprint\":\"fp1\",\"client_session_id\":\"cs1\",\"accepted_at_ms\":10,\"state\":\"succeeded\",\"terminal_event_id\":\"e1\",\"error_code\":null,\"result\":null,\"sync_result\":null},\"cmd2\":{\"fingerprint\":\"fp2\",\"accepted_at_ms\":20,\"state\":\"running\"}}",
    )
    .expect("write receipts");
    // r1 释放、r2 仍 held;折叠成 2 行终态。
    fs::write(
        dir.join("quota/ROOTX/ledger.jsonl"),
        "{\"op\":\"reserved\",\"reservation_id\":\"r1\",\"root_session_id\":\"ROOTX\",\"child_session_id\":\"c1\",\"kind\":\"spawn\",\"amount\":1,\"canonical_key\":\"k1\",\"at_ms\":5}\n{\"op\":\"released\",\"reservation_id\":\"r1\",\"reason\":\"completed\",\"at_ms\":6}\n{\"op\":\"reserved\",\"reservation_id\":\"r2\",\"root_session_id\":\"ROOTX\",\"child_session_id\":null,\"kind\":\"tool\",\"amount\":2,\"canonical_key\":\"k2\",\"at_ms\":7}\n",
    )
    .expect("write quota ledger");
}

fn assert_all_tombstoned(dir: &Path) {
    for name in [
        "sessions/SEEDAAA/todo.json",
        "sessions/SEEDBBB/todo.json",
        "ringing-devices.json",
        "ringing-command-receipts.json",
        "quota/ROOTX/ledger.jsonl",
    ] {
        assert!(
            !dir.join(name).exists(),
            "legacy file {name} must be gone after ingest"
        );
        assert!(
            dir.join(format!("{name}.migrated-v4.4")).is_file(),
            "tombstone {name}.migrated-v4.4 must exist"
        );
    }
}

#[test]
fn ingests_all_four_ledgers_with_exact_row_counts_and_tombstones() {
    let dir = tempfile::tempdir().expect("tempdir");
    seed_legacy_files(dir.path());
    let db = Db::open(dir.path().join("qaqh.db")).expect("open db");

    // 行数 == 原文件记录数(铁闸 2 的核对在实现里是硬失败,这里是它的正证)。
    assert_eq!(count(&db, "SELECT COUNT(*) FROM session_todos"), 3);
    assert_eq!(count(&db, "SELECT COUNT(*) FROM session_todo_state"), 2);
    assert_eq!(count(&db, "SELECT COUNT(*) FROM paired_devices"), 2);
    assert_eq!(count(&db, "SELECT COUNT(*) FROM command_receipts"), 2);
    assert_eq!(count(&db, "SELECT COUNT(*) FROM quota_reservations"), 2);

    // 语义抽查:无 id 回落 title;evidence/状态词表原样;折叠终态。
    let fallback: String = db
        .read_blocking(|conn| {
            conn.query_row(
                "SELECT todo_id FROM session_todos WHERE title='t3'",
                [],
                |row| row.get(0),
            )
        })
        .expect("todo_id fallback");
    assert_eq!(fallback, "t3");
    let (state, reason): (String, Option<String>) = db
        .read_blocking(|conn| {
            conn.query_row(
                "SELECT state, release_reason FROM quota_reservations WHERE reservation_id='r1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
        })
        .expect("folded r1");
    assert_eq!(
        (state.as_str(), reason.as_deref()),
        ("released", Some("completed"))
    );
    let mode: String = db
        .read_blocking(|conn| {
            conn.query_row(
                "SELECT mode FROM session_todo_state WHERE session_id='SEEDAAA'",
                [],
                |row| row.get(0),
            )
        })
        .expect("todo mode");
    assert_eq!(mode, "goal");

    assert_all_tombstoned(dir.path());
}

#[test]
fn second_open_never_reingests() {
    let dir = tempfile::tempdir().expect("tempdir");
    seed_legacy_files(dir.path());
    {
        let db = Db::open(dir.path().join("qaqh.db")).expect("first open");
        assert_eq!(count(&db, "SELECT COUNT(*) FROM paired_devices"), 2);
    }
    {
        let db = Db::open(dir.path().join("qaqh.db")).expect("second open");
        // 版本位已 12 ⇒ 阶梯不再吸入;行数若翻倍即主键/幂等失守。
        assert_eq!(count(&db, "SELECT COUNT(*) FROM session_todos"), 3);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM paired_devices"), 2);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM command_receipts"), 2);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM quota_reservations"), 2);
    }
    // 墓碑不会被二次改名出 `.migrated-v4.4.migrated-v4.4`。
    assert_all_tombstoned(dir.path());
}

#[test]
fn crash_between_commit_and_rename_does_not_reingest() {
    let dir = tempfile::tempdir().expect("tempdir");
    seed_legacy_files(dir.path());
    {
        Db::open(dir.path().join("qaqh.db")).expect("first open");
    }
    // 模拟 COMMIT 成功、改名前进程被杀:把墓碑改回原名,数据已在库中。
    fs::rename(
        dir.path().join("ringing-devices.json.migrated-v4.4"),
        dir.path().join("ringing-devices.json"),
    )
    .expect("simulate crash before retire");
    {
        let db = Db::open(dir.path().join("qaqh.db")).expect("open after simulated crash");
        // 版本位闸门:不得重复吸入(重复 INSERT 会当场主键冲突报错)。
        assert_eq!(count(&db, "SELECT COUNT(*) FROM paired_devices"), 2);
    }
    // 旧文件原地保留是允许的最终态(retire 失败仅 WARN);不得被吞掉。
    assert!(dir.path().join("ringing-devices.json").is_file());
}

#[test]
fn corrupt_legacy_file_rolls_back_version_and_keeps_files_in_place() {
    let dir = tempfile::tempdir().expect("tempdir");
    seed_legacy_files(dir.path());
    // 铁闸 2 语义:损坏旧文件是硬失败,不是「读不懂当空」。
    fs::write(dir.path().join("sessions/SEEDAAA/todo.json"), "{ not json")
        .expect("corrupt todo");
    assert!(Db::open(dir.path().join("qaqh.db")).is_err());

    // 回滚:EXCLUSIVE 事务整体回退 ⇒ v12 表不存在,旧文件一个都不许改名。
    let conn = rusqlite::Connection::open(dir.path().join("qaqh.db")).expect("raw open");
    let tables: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name IN ('session_todos','paired_devices','quota_reservations')",
            [],
            |row| row.get(0),
        )
        .expect("master query");
    assert_eq!(tables, 0, "rolled-back db must not carry any v12 table");
    for name in [
        "sessions/SEEDAAA/todo.json",
        "sessions/SEEDBBB/todo.json",
        "ringing-devices.json",
        "ringing-command-receipts.json",
        "quota/ROOTX/ledger.jsonl",
    ] {
        assert!(dir.path().join(name).is_file(), "{name} must survive rollback");
        assert!(
            !dir.path().join(format!("{name}.migrated-v4.4")).exists(),
            "no tombstone may exist after rollback"
        );
    }
}

#[test]
fn quota_ledger_root_mismatch_is_hard_failure() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::create_dir_all(dir.path().join("quota/ROOTX")).expect("mkdir quota");
    fs::write(
        dir.path().join("quota/ROOTX/ledger.jsonl"),
        "{\"op\":\"reserved\",\"reservation_id\":\"r1\",\"root_session_id\":\"OTHER\",\"child_session_id\":null,\"kind\":\"spawn\",\"amount\":1,\"canonical_key\":\"k\",\"at_ms\":5}\n",
    )
    .expect("write mismatched ledger");
    assert!(Db::open(dir.path().join("qaqh.db")).is_err());
    assert!(dir.path().join("quota/ROOTX/ledger.jsonl").is_file());
}

#[test]
fn legacy_v11_database_upgrades_and_ingests_in_one_step() {
    // 铁闸 2 主场景是存量库升级(不是空库首建):先把当前代码建出的 v12 库
    // 手工退成真 v11 形态(删 v12 五表、印版本 11),再放旧文件重新 open。
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("qaqh.db");
    Db::open(&db_path).expect("fresh v12 db");
    {
        let conn = rusqlite::Connection::open(&db_path).expect("open built db");
        conn.execute_batch(
            "DROP TABLE quota_reservations;
             DROP TABLE command_receipts;
             DROP TABLE paired_devices;
             DROP TABLE session_todo_state;
             DROP TABLE session_todos;
             UPDATE schema_version SET version = 11;",
        )
        .expect("downgrade to true v11 shape");
    }
    seed_legacy_files(dir.path());
    let db = Db::open(&db_path).expect("upgrade v11 -> v12 with ingest");
    assert_eq!(count(&db, "SELECT COUNT(*) FROM session_todos"), 3);
    assert_eq!(count(&db, "SELECT COUNT(*) FROM quota_reservations"), 2);
    assert_all_tombstoned(dir.path());
}

#[test]
fn misstamped_v11_database_is_repaired_then_upgraded() {
    // 旧手写阶梯的存量病灶:`Some(5)~Some(8)` 链跳过 10→11 却把版本位印成
    // 11——现场存在「记 11、缺 team_mode」的错印库。`repair_skipped_v11`
    // 必须在 11→12 前把它补形,而不是让启动在业务读写处才炸。
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("qaqh.db");
    Db::open(&db_path).expect("fresh v12 db");
    {
        let conn = rusqlite::Connection::open(&db_path).expect("open built db");
        conn.execute_batch(
            "DROP TABLE quota_reservations;
             DROP TABLE command_receipts;
             DROP TABLE paired_devices;
             DROP TABLE session_todo_state;
             DROP TABLE session_todos;
             ALTER TABLE sessions DROP COLUMN team_mode;
             UPDATE schema_version SET version = 11;",
        )
        .expect("forge misstamped v11 shape");
    }
    let db = Db::open(&db_path).expect("open must survive misstamped v11");
    let has_team_mode: i64 = db
        .read_blocking(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM pragma_table_info('sessions') WHERE name='team_mode'",
                [],
                |row| row.get(0),
            )
        })
        .expect("pragma query");
    assert_eq!(has_team_mode, 1);
    // 错印修复与吸入同事务:COMMIT 即成功 ⇒ 旧文件必已墓碑。
    Db::open(&db_path).expect("reopen is a clean v12 no-op");
}
