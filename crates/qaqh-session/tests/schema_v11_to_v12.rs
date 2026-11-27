use qaqh_session::db::schema::{SCHEMA_VERSION, initialize};
use rusqlite::Connection;

#[test]
fn v11_upgrade_creates_empty_v12_tables() {
    let mut conn = Connection::open_in_memory().expect("open");
    initialize(&mut conn).expect("fresh schema");
    conn.execute_batch(
        "DROP TABLE session_todos;
         DROP TABLE session_todo_state;
         DROP TABLE paired_devices;
         DROP TABLE command_receipts;
         DROP TABLE quota_reservations;
         DELETE FROM schema_version;
         INSERT INTO schema_version VALUES (11);",
    )
    .expect("v11 fixture");

    initialize(&mut conn).expect("upgrade v11");

    let version: u32 = conn
        .query_row("SELECT MAX(version) FROM schema_version", [], |row| {
            row.get(0)
        })
        .expect("schema version");
    assert_eq!(version, SCHEMA_VERSION);

    for table in [
        "session_todos",
        "session_todo_state",
        "paired_devices",
        "command_receipts",
        "quota_reservations",
    ] {
        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [table],
                |row| row.get(0),
            )
            .expect("table existence");
        assert_eq!(exists, 1, "{table} must exist after v11 upgrade");

        let rows: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .expect("table row count");
        assert_eq!(rows, 0, "{table} must be empty after v11 upgrade");
    }
}
