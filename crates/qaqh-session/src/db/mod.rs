//! SQLite 业务存储:唯一业务数据权威。
//!
//! 职责边界(架构报告一 §7):单库 + WAL;所有写操作进入串行写入执行器;
//! 读查询使用短连接直查业务表。不在数据库外重建提交协议、摘要链或恢复框架;
//! 失败直接返回数据库错误,不静默回退。

use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};

use rusqlite::{Connection, OpenFlags};
use tokio::sync::oneshot;

pub mod devices;
pub mod goals;
/// v4.5.1 任务2/3:WAL 封顶、空间回收与撤回孤儿 BLOB 清理(独立子模块)。
pub(crate) mod maintenance;
pub mod quota;
/// v4.5.1 任务4:有界只读连接池(独立子模块,替代单把 `Mutex<Connection>`)。
mod read_pool;
pub mod receipts;
pub mod schema;
pub mod store;
pub mod team;
pub mod todos;
pub mod view;
#[cfg(test)]
mod view_tests;
pub mod workspace;

/// SQLite ≥ 3.51.3 包含官方 WAL-reset 修复,低于该版本直接拒绝启动。
const MIN_SQLITE_VERSION: (u32, u32, u32) = (3, 51, 3);

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("create data dir {path}: {source}")]
    CreateDir {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("open database {path}: {source}")]
    Open {
        path: PathBuf,
        source: rusqlite::Error,
    },
    #[error("journal_mode returned {actual:?}, WAL not engaged")]
    WalNotEngaged { actual: Option<String> },
    #[error("synchronous is {actual}, expected FULL(2)")]
    SyncNotFull { actual: i64 },
    #[error("journal_size_limit is {actual}, expected {expected}")]
    JournalSizeLimit { actual: i64, expected: i64 },
    #[error("sqlite {found} is older than required {required:?}")]
    SqliteTooOld {
        found: String,
        required: (u32, u32, u32),
    },
    #[error("schema version {found} is newer than supported {supported}")]
    SchemaTooNew { found: u32, supported: u32 },
    #[error(
        "schema version {found} is older than supported {supported}; delete the database to recreate it"
    )]
    SchemaTooOld { found: u32, supported: u32 },
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("write executor stopped")]
    ExecutorStopped,
    #[error("read connection poisoned")]
    ReadLockPoisoned,
    #[error("session {session_id} not found")]
    SessionNotFound { session_id: String },
    #[error("turn {turn_id} not found in session {session_id}")]
    TurnNotFound { session_id: String, turn_id: String },
    #[error("session {session_id} has an active turn {turn_id}; truncate requires an idle session")]
    TruncateConflict { session_id: String, turn_id: String },
    #[error("session {session_id} already has an unfinished goal")]
    GoalExists { session_id: String },
    #[error("session {session_id} has no goal")]
    NoGoal { session_id: String },
    #[error("corrupt business row: {0}")]
    Corrupt(String),
    #[error("workspace operation: {0}")]
    Workspace(String),
}

impl DbError {
    /// v4.5.1 任务1:盘满判别。`SQLITE_FULL`(SQLite)与 `StorageFull`(宿主 OS)
    /// 统一归为磁盘满,供回合收口 Fail-Closed 时产出用户可读的明确错误。
    pub fn is_disk_full(&self) -> bool {
        match self {
            DbError::Sql(rusqlite::Error::SqliteFailure(error, _)) => {
                error.code == rusqlite::ErrorCode::DiskFull
            }
            DbError::Open { source, .. } => matches!(
                source,
                rusqlite::Error::SqliteFailure(error, _)
                    if error.code == rusqlite::ErrorCode::DiskFull
            ),
            DbError::Io(error) => error.kind() == std::io::ErrorKind::StorageFull,
            DbError::CreateDir { source, .. } => source.kind() == std::io::ErrorKind::StorageFull,
            _ => false,
        }
    }
}

struct WriteJob(Box<dyn FnOnce(&mut Connection) + Send>);

/// 唯一数据库句柄:一个串行写执行器线程 + 一个有界只读连接池(v4.5.1 任务4)。
pub struct Db {
    path: PathBuf,
    write_tx: mpsc::Sender<WriteJob>,
    read_pool: Arc<read_pool::ReadPool>,
}

impl Db {
    /// 打开(必要时创建)数据库,落实 WAL + synchronous=FULL 并读回确认。
    pub fn open(path: impl AsRef<Path>) -> Result<Db, DbError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| DbError::CreateDir {
                path: parent.to_path_buf(),
                source,
            })?;
        }

        let mut write_conn = Connection::open(&path).map_err(|source| DbError::Open {
            path: path.clone(),
            source,
        })?;
        configure(&mut write_conn)?;

        let read_conn = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|source| DbError::Open {
            path: path.clone(),
            source,
        })?;
        read_conn.busy_timeout(std::time::Duration::from_secs(5))?;
        // journal_size_limit 是连接级设置:预热只读连接与后续池内连接必须同律。
        maintenance::configure_journal_size_limit(&read_conn)?;
        let read_pool = Arc::new(read_pool::ReadPool::new(path.clone(), Some(read_conn)));

        schema::initialize(&mut write_conn)?;

        let write_tx = spawn_executor(write_conn)?;

        Ok(Db {
            path,
            write_tx,
            read_pool,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 串行写入:闭包在写执行器线程上运行,可包含多条语句,提交由事务表达。
    pub async fn write<T, E, F>(&self, f: F) -> Result<T, DbError>
    where
        T: Send + 'static,
        E: Into<DbError> + Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T, E> + Send + 'static,
    {
        let (reply_tx, reply_rx) = oneshot::channel();
        let job: Box<dyn FnOnce(&mut Connection) + Send> = Box::new(move |conn| {
            // 调用方被取消(oneshot 接收端丢弃)时,写入结果随之丢弃;事务边界不变。
            let _ = reply_tx.send(f(conn).map_err(Into::into));
        });
        self.write_tx
            .send(WriteJob(job))
            .map_err(|_| DbError::ExecutorStopped)?;
        Ok(reply_rx.await.map_err(|_| DbError::ExecutorStopped)??)
    }

    /// 只读查询:在阻塞线程上借用池中一条只读连接,查询必须尽快结束。
    pub async fn read<T, E, F>(&self, f: F) -> Result<T, DbError>
    where
        T: Send + 'static,
        E: Into<DbError> + Send + 'static,
        F: FnOnce(&Connection) -> Result<T, E> + Send + 'static,
    {
        let pool = Arc::clone(&self.read_pool);
        tokio::task::spawn_blocking(move || pool.with(f))
            .await
            .map_err(|_| DbError::ExecutorStopped)?
    }

    /// 串行写入(阻塞版):供同步线程(会话 owner 泵)使用,同一个写执行器。
    pub fn write_blocking<T, E, F>(&self, f: F) -> Result<T, DbError>
    where
        T: Send + 'static,
        E: Into<DbError> + Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T, E> + Send + 'static,
    {
        let (reply_tx, reply_rx) = mpsc::channel();
        let job: Box<dyn FnOnce(&mut Connection) + Send> = Box::new(move |conn| {
            let _ = reply_tx.send(f(conn).map_err(Into::into));
        });
        self.write_tx
            .send(WriteJob(job))
            .map_err(|_| DbError::ExecutorStopped)?;
        reply_rx.recv().map_err(|_| DbError::ExecutorStopped)?
    }

    /// 只读查询(阻塞版):调用方必须已在同步线程上。
    pub fn read_blocking<T, E, F>(&self, f: F) -> Result<T, DbError>
    where
        E: Into<DbError>,
        F: FnOnce(&Connection) -> Result<T, E>,
    {
        self.read_pool.with(f)
    }
}

fn configure(conn: &mut Connection) -> Result<(), DbError> {
    conn.busy_timeout(std::time::Duration::from_secs(5))?;

    let journal_mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
    if journal_mode != "wal" {
        return Err(DbError::WalNotEngaged {
            actual: Some(journal_mode),
        });
    }

    conn.pragma_update(None, "synchronous", "FULL")?;
    let sync: i64 = conn.query_row("PRAGMA synchronous", [], |row| row.get(0))?;
    if sync != 2 {
        return Err(DbError::SyncNotFull { actual: sync });
    }

    conn.pragma_update(None, "foreign_keys", "ON")?;
    // v4.5.1 任务3:WAL 高水位封顶(读回校验 fail-closed)。
    maintenance::configure_journal_size_limit(conn)?;

    let version: String = conn.query_row("SELECT sqlite_version()", [], |row| row.get(0))?;
    let parsed = parse_version(&version);
    if parsed < MIN_SQLITE_VERSION {
        return Err(DbError::SqliteTooOld {
            found: version,
            required: MIN_SQLITE_VERSION,
        });
    }

    Ok(())
}

fn parse_version(version: &str) -> (u32, u32, u32) {
    let mut parts = [0u32; 3];
    for (index, part) in version.split('.').take(3).enumerate() {
        parts[index] = part.parse().unwrap_or(0);
    }
    (parts[0], parts[1], parts[2])
}

fn spawn_executor(mut conn: Connection) -> Result<mpsc::Sender<WriteJob>, DbError> {
    let (tx, rx) = mpsc::channel::<WriteJob>();
    std::thread::Builder::new()
        .name("qaqh-db-writer".into())
        .spawn(move || {
            // 执行器线程与 daemon 同生命周期;发送端全部关闭后自然退出。
            while let Ok(job) = rx.recv() {
                (job.0)(&mut conn);
            }
        })
        .map_err(DbError::Io)?;
    Ok(tx)
}
