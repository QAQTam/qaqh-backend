//! v4.5.1 任务4:有界只读连接池(修复单 §二.4.1,宪法 #8 独立子模块)。
//!
//! 旧实现是单把 `Arc<Mutex<Connection>>`:多会话并发 `assemble_context` 与前端
//! 快照读互相串行排队。这里以 `Mutex` + `Condvar` + `Vec<Connection>` 实现
//! 固定容量 4 条 `SQLITE_OPEN_READ_ONLY | SQLITE_OPEN_NO_MUTEX` 连接的有界借用池,
//! 不引入重量级第三方连接池依赖;`Db::read` / `Db::read_blocking` 对外签名不变。
//!
//! fail-closed 纪律:
//! - 互斥锁中毒一律返回 `DbError::ReadLockPoisoned`,绝不返回可疑句柄;
//! - 连接级致命错误(corrupt / not-a-database / OOM)的句柄**不回池**,丢弃并
//!   释放容量计数,由后续借用者重新打开补位;
//! - 归还路径锁中毒时连接宁可丢弃也不回流。

use std::path::PathBuf;
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};

use super::DbError;

/// 池容量(修复单 §二.4.1:固定 4 条)。
const READ_POOL_CAPACITY: usize = 4;
const READ_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) struct ReadPool {
    path: PathBuf,
    state: Mutex<PoolState>,
    released: Condvar,
}

struct PoolState {
    /// 空闲可借连接;借用方 `pop`,归还方 `push`。
    idle: Vec<Connection>,
    /// 已打开连接数(含在借);打开失败时回退。
    opened: usize,
}

impl ReadPool {
    /// `prewarmed` 是 `Db::open` 既有健全性检查打开的那条只读连接,
    /// 直接作为池的第一条连接,不再额外打开。
    pub(crate) fn new(path: PathBuf, prewarmed: Option<Connection>) -> ReadPool {
        let (idle, opened) = match prewarmed {
            Some(conn) => (vec![conn], 1),
            None => (Vec::new(), 0),
        };
        ReadPool {
            path,
            state: Mutex::new(PoolState { idle, opened }),
            released: Condvar::new(),
        }
    }

    fn open_read_only(&self) -> Result<Connection, DbError> {
        let conn = Connection::open_with_flags(
            &self.path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|source| DbError::Open {
            path: self.path.clone(),
            source,
        })?;
        conn.busy_timeout(READ_BUSY_TIMEOUT)?;
        // journal_size_limit 是连接级设置:写连接之外,池内每条只读连接也必须
        // 带同一上限,否则读回校验与任意连接侧 checkpoint 的封顶语义漂移。
        super::maintenance::configure_journal_size_limit(&conn)?;
        Ok(conn)
    }

    /// 借用一条只读连接执行 `f`;借用守卫 Drop 时自动归还(修复单 §二.4.1)。
    pub(crate) fn with<T, E, F>(&self, f: F) -> Result<T, DbError>
    where
        E: Into<DbError>,
        F: FnOnce(&Connection) -> Result<T, E>,
    {
        let mut lease = Lease::acquire(self)?;
        let outcome = f(lease.connection()).map_err(Into::into);
        lease.poisoned = outcome
            .as_ref()
            .err()
            .is_some_and(is_connection_fatal);
        outcome
    }
}

/// 连接级致命错误:句柄已不可信,回流会污染后续读者——丢弃并放行补位。
/// 查询级错误(锁定、磁盘满、行缺失)不属于连接损坏,连接照常回池。
fn is_connection_fatal(error: &DbError) -> bool {
    matches!(
        error,
        DbError::Sql(rusqlite::Error::SqliteFailure(failure, _))
            if matches!(
                failure.code,
                rusqlite::ErrorCode::DatabaseCorrupt
                    | rusqlite::ErrorCode::NotADatabase
                    | rusqlite::ErrorCode::OutOfMemory
            )
    )
}

struct Lease<'a> {
    pool: &'a ReadPool,
    conn: Option<Connection>,
    poisoned: bool,
}

impl<'a> Lease<'a> {
    fn acquire(pool: &'a ReadPool) -> Result<Lease<'a>, DbError> {
        let mut state = pool
            .state
            .lock()
            .map_err(|_| DbError::ReadLockPoisoned)?;
        loop {
            if let Some(conn) = state.idle.pop() {
                return Ok(Lease {
                    pool,
                    conn: Some(conn),
                    poisoned: false,
                });
            }
            if state.opened < READ_POOL_CAPACITY {
                state.opened += 1;
                drop(state);
                return match pool.open_read_only() {
                    Ok(conn) => Ok(Lease {
                        pool,
                        conn: Some(conn),
                        poisoned: false,
                    }),
                    Err(error) => {
                        let mut state = pool
                            .state
                            .lock()
                            .map_err(|_| DbError::ReadLockPoisoned)?;
                        state.opened -= 1;
                        pool.released.notify_one();
                        Err(error)
                    }
                };
            }
            // 池满:等任意一条连接归还。锁中毒同样 fail-closed 报错。
            state = pool
                .released
                .wait(state)
                .map_err(|_| DbError::ReadLockPoisoned)?;
        }
    }

    fn connection(&self) -> &Connection {
        self.conn
            .as_ref()
            .expect("lease holds its connection until Drop")
    }
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        let Some(conn) = self.conn.take() else {
            return;
        };
        let Ok(mut state) = self.pool.state.lock() else {
            // 归还路径锁中毒:连接丢弃,绝不回流损坏句柄(fail-closed)。
            drop(conn);
            return;
        };
        if self.poisoned {
            // 致命错误的连接不回池;释放容量计数,放行后续补位。
            state.opened -= 1;
        } else {
            state.idle.push(conn);
        }
        self.pool.released.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::super::Db;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// 修复单 §四 任务4 反证1:4 线程同时经 `db.read_blocking` 各执行一次
    /// 耗时 100ms 的读闭包。旧单锁实现总耗时 >= 400ms(必红);
    /// 4 连接只读池总耗时 < 250ms(绿)。
    #[test]
    fn four_concurrent_reads_run_in_parallel_not_serialized() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Arc::new(Db::open(dir.path().join("qaqh.db")).expect("open"));
        let start = Instant::now();
        let mut joins = Vec::new();
        for _ in 0..4 {
            let db = Arc::clone(&db);
            joins.push(std::thread::spawn(move || {
                db.read_blocking(|conn| {
                    std::thread::sleep(Duration::from_millis(100));
                    conn.query_row("SELECT 40 + 2", [], |row| row.get::<_, i64>(0))
                        .map_err(super::DbError::from)
                })
                .expect("read")
            }));
        }
        for join in joins {
            join.join().expect("join");
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(250),
            "4 条只读连接必须并行执行:实际总耗时 {elapsed:?}(旧单锁实现 >= 400ms)"
        );
    }

    /// 池满时借用在 `Condvar` 上排队,归还后放行——不扩容、不饿死。
    #[test]
    fn leases_queue_when_pool_is_exhausted_then_drain() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Barrier;

        const THREADS: usize = 8;
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Arc::new(Db::open(dir.path().join("qaqh.db")).expect("open"));
        let barrier = Arc::new(Barrier::new(THREADS));
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let mut joins = Vec::with_capacity(THREADS);
        for _ in 0..THREADS {
            let db = Arc::clone(&db);
            let barrier = Arc::clone(&barrier);
            let active = Arc::clone(&active);
            let max_active = Arc::clone(&max_active);
            joins.push(std::thread::spawn(move || {
                barrier.wait();
                db.read_blocking(|conn| {
                    let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                    max_active.fetch_max(current, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(40));
                    let value = conn
                        .query_row("SELECT 1", [], |row| row.get::<_, i64>(0))
                        .map_err(super::DbError::from);
                    active.fetch_sub(1, Ordering::SeqCst);
                    value
                })
                .expect("read")
            }));
        }
        for join in joins {
            assert_eq!(join.join().expect("join"), 1);
        }
        assert_eq!(active.load(Ordering::SeqCst), 0, "全部借用归还后活跃数必须归零");
        assert_eq!(
            max_active.load(Ordering::SeqCst),
            super::READ_POOL_CAPACITY,
            "8 线程并发持连接 40ms 必须精确打满 4 条连接且不超容(真实走过 Condvar::wait)"
        );
        // 全部归还后,同一批再用并发读依然可完成(连接成功回流)。
        let db2 = Arc::clone(&db);
        assert_eq!(
            db2.read_blocking(|conn| {
                conn.query_row("SELECT 2", [], |row| row.get::<_, i64>(0))
                    .map_err(super::DbError::from)
            })
            .expect("read"),
            2
        );
    }
}
