//! [`Db`], the one way the rest of Kiki reaches the database.
//!
//! Connections are never handed out directly. Instead, work is passed to
//! [`Db::read`] or [`Db::write`] as a synchronous closure, which runs on a
//! blocking thread with a connection checked out for exactly as long as it
//! runs. That rules out, by construction, the two ways a connection pool
//! deadlocks:
//!
//! - holding a connection across an `.await`, since the closure cannot
//!   await; and
//! - checking out a second connection while holding one, since the closure
//!   is given a connection and never the pool. A closure that reaches a
//!   [`Db`] some other way is refused with [`DbError::Nested`] rather than
//!   left to wait on itself.
//!
//! Writes go through a single dedicated connection. SQLite lets only one
//! connection write at a time anyway, so writers now queue for that
//! connection instead of for the database's write lock, and never take up a
//! connection a reader could use. Reads use a separate pool whose
//! connections are opened `query_only`, so a write sent down the read path
//! fails at once instead of competing for the lock.

use super::pool::{ConnectionManager, Pool};
use crate::metrics::{Metrics, PoolMetrics};
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{Connection, OpenFlags};
use std::cell::Cell;
use std::panic::Location;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// The number of connections in the read pool unless
/// [`DbOptions::readers`] says otherwise. Each keeps a page cache of its
/// own, so more mostly cost memory.
pub const DEFAULT_READERS: u32 = 4;

/// Errors from getting a connection to run database work on.
///
/// Errors from the work itself are whatever the closure returns.
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    /// No connection became free within the pool's connection timeout.
    #[error(transparent)]
    Pool(#[from] r2d2::Error),

    /// Database work asked for a connection while its thread already held
    /// one. Waiting would risk a deadlock, so it is refused.
    #[error("database connection requested at {at} while this thread already holds one")]
    Nested {
        /// Where the second connection was asked for.
        at: &'static Location<'static>,
    },

    /// The blocking task running the work panicked or was cancelled.
    #[error("database task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
}

/// Options for [`Db::open`].
#[derive(Clone, Default)]
pub struct DbOptions {
    /// Connections in the read pool; [`DEFAULT_READERS`] if unset.
    pub readers: Option<u32>,
    /// How long to wait for a free connection before failing with
    /// [`DbError::Pool`]; r2d2's default of 30 seconds if unset.
    pub connection_timeout: Option<Duration>,
    /// When set, records statement timings, page I/O, and pool usage.
    pub metrics: Option<Arc<Metrics>>,
}

/// A handle to the database: one connection for writing, and a pool of
/// read-only connections. Cheap to clone.
///
/// # Examples
///
/// ```
/// use kiki_rss::db::{Db, DbOptions};
///
/// let dir = tempfile::TempDir::new()?;
/// let path = dir.path().join("kiki.db");
/// rusqlite::Connection::open(&path)?; // Db::open does not create databases.
/// let db = Db::open(&path, DbOptions::default())?;
/// db.write_blocking(|conn| conn.execute_batch("CREATE TABLE t (x); INSERT INTO t VALUES (1);"))??;
/// let n: i64 = db.read_blocking(|conn| conn.query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0)))??;
/// assert_eq!(n, 1);
/// # Ok::<(), anyhow::Error>(())
/// ```
#[derive(Clone, Debug)]
pub struct Db {
    readers: Pool,
    writer: Pool,
    #[cfg(test)]
    path: std::path::PathBuf,
}

impl Db {
    /// Open the database at `path`, which must already exist.
    ///
    /// Every connection waits up to five seconds for a lock, uses WAL
    /// journaling, and enforces foreign keys; see [`connection_manager`].
    ///
    /// # Errors
    ///
    /// Fails if the first connections cannot be opened.
    pub fn open(path: &Path, options: DbOptions) -> anyhow::Result<Self> {
        // The writer is opened first: it is what switches a new database
        // into WAL mode, which the read-only connections cannot do.
        let writer = build_pool(path, &options, 1, false)?;
        let readers = build_pool(
            path,
            &options,
            options.readers.unwrap_or(DEFAULT_READERS),
            true,
        )?;
        Ok(Self {
            readers,
            writer,
            #[cfg(test)]
            path: path.to_path_buf(),
        })
    }

    /// A connection of the test's own to the same database, outside the
    /// pools, for setting up and checking on what the code under test does.
    #[cfg(test)]
    #[allow(clippy::expect_used)]
    pub(crate) fn connect(&self) -> Connection {
        let conn = Connection::open(&self.path).expect("open the test database");
        conn.execute_batch("PRAGMA busy_timeout=5000; PRAGMA foreign_keys=ON;")
            .expect("configure the test connection");
        conn
    }

    /// Run `f` with a read-only connection, on a blocking thread.
    ///
    /// # Errors
    ///
    /// Fails with [`DbError::Pool`] if no reader is free within the
    /// connection timeout, or [`DbError::Task`] if `f` panics.
    #[track_caller]
    pub fn read<T, F>(&self, f: F) -> impl std::future::Future<Output = Result<T, DbError>>
    where
        F: FnOnce(&mut Connection) -> T + Send + 'static,
        T: Send + 'static,
    {
        spawn(self.readers.clone(), Location::caller(), f)
    }

    /// Run `f` with the writer connection, on a blocking thread.
    ///
    /// # Errors
    ///
    /// As [`Db::read`], for the writer.
    #[track_caller]
    pub fn write<T, F>(&self, f: F) -> impl std::future::Future<Output = Result<T, DbError>>
    where
        F: FnOnce(&mut Connection) -> T + Send + 'static,
        T: Send + 'static,
    {
        spawn(self.writer.clone(), Location::caller(), f)
    }

    /// Run `f` with a read-only connection on this thread, blocking it.
    ///
    /// For synchronous code, and for async code whose work borrows from the
    /// caller; see [`crate::db::blocking`] for how it keeps the runtime
    /// responsive.
    ///
    /// # Errors
    ///
    /// Fails with [`DbError::Nested`] if this thread already holds a
    /// connection, or [`DbError::Pool`] if no reader is free within the
    /// connection timeout.
    #[track_caller]
    pub fn read_blocking<T>(&self, f: impl FnOnce(&mut Connection) -> T) -> Result<T, DbError> {
        let at = Location::caller();
        super::blocking(|| run(&self.readers, at, f))
    }

    /// Run `f` with the writer connection on this thread, blocking it.
    ///
    /// # Errors
    ///
    /// As [`Db::read_blocking`], for the writer.
    #[track_caller]
    pub fn write_blocking<T>(&self, f: impl FnOnce(&mut Connection) -> T) -> Result<T, DbError> {
        let at = Location::caller();
        super::blocking(|| run(&self.writer, at, f))
    }

    /// The number of connections open, and of those idle, across the
    /// read pool and the writer.
    pub fn connections(&self) -> (u32, u32) {
        let (r, w) = (self.readers.state(), self.writer.state());
        (
            r.connections + w.connections,
            r.idle_connections + w.idle_connections,
        )
    }
}

thread_local! {
    /// Whether this thread has a connection checked out through a [`Db`].
    static HOLDING: Cell<bool> = const { Cell::new(false) };
}

/// Marks this thread as holding a connection until dropped.
struct Holding;

impl Holding {
    fn claim(at: &'static Location<'static>) -> Result<Self, DbError> {
        if HOLDING.with(|h| h.replace(true)) {
            tracing::error!("refusing a nested database connection requested at {at}");
            return Err(DbError::Nested { at });
        }
        Ok(Holding)
    }
}

impl Drop for Holding {
    fn drop(&mut self) {
        HOLDING.with(|h| h.set(false));
    }
}

/// Check a connection out of `pool` and run `f` with it, on this thread.
fn run<T>(
    pool: &Pool,
    at: &'static Location<'static>,
    f: impl FnOnce(&mut Connection) -> T,
) -> Result<T, DbError> {
    let _holding = Holding::claim(at)?;
    let mut conn = pool.get()?;
    Ok(f(&mut conn))
}

async fn spawn<T, F>(pool: Pool, at: &'static Location<'static>, f: F) -> Result<T, DbError>
where
    F: FnOnce(&mut Connection) -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(move || run(&pool, at, f)).await?
}

fn build_pool(
    path: &Path,
    options: &DbOptions,
    size: u32,
    read_only: bool,
) -> anyhow::Result<Pool> {
    let manager = connection_manager(path, options.metrics.clone(), read_only);
    let mut builder = r2d2::Pool::builder().max_size(size);
    if let Some(timeout) = options.connection_timeout {
        builder = builder.connection_timeout(timeout);
    }
    if let Some(metrics) = &options.metrics {
        builder = builder.event_handler(Box::new(PoolMetrics(metrics.clone())));
    }
    Ok(builder.build(manager)?)
}

/// The connection manager for one of a [`Db`]'s pools.
///
/// Every connection waits up to five seconds for a lock (set first, so the
/// other pragmas wait too), uses WAL journaling with `synchronous=NORMAL`
/// (safe in WAL mode: a power loss can drop the last commits but never
/// corrupt the database, and it saves an fsync per commit), enforces
/// foreign keys, and has a `regexp` function. With `read_only`, it is also
/// `query_only`. With `metrics`, every statement is timed, and the page I/O
/// of each checkout recorded.
fn connection_manager(
    path: &Path,
    metrics: Option<Arc<Metrics>>,
    read_only: bool,
) -> ConnectionManager {
    let statement_metrics = metrics.clone();
    let inner = SqliteConnectionManager::file(path)
        .with_flags(OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .with_init(move |c| {
            if let Some(metrics) = &statement_metrics {
                crate::db::profile::install(c, metrics.clone())?;
            }
            c.execute_batch(
                "PRAGMA busy_timeout=5000; PRAGMA journal_mode=WAL; \
                 PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON;",
            )?;
            if read_only {
                c.execute_batch("PRAGMA query_only=ON;")?;
            }
            add_regexp(c)
        });
    let manager = ConnectionManager::new(inner);
    match metrics {
        Some(metrics) => manager.with_metrics(metrics),
        None => manager,
    }
}

/// Define `regexp(pattern, text)`, which SQLite calls for `text REGEXP
/// pattern`.
fn add_regexp(c: &Connection) -> rusqlite::Result<()> {
    use rusqlite::functions::FunctionFlags;
    c.create_scalar_function(
        "regexp",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let pattern = ctx.get_raw(0).as_str()?;
            let text = ctx.get_raw(1).as_str().unwrap_or("");
            let re = regex::Regex::new(pattern)
                .map_err(|e| rusqlite::Error::UserFunctionError(Box::new(e)))?;
            Ok(re.is_match(text))
        },
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn open(options: DbOptions) -> (tempfile::TempDir, Db) {
        let dir = tempfile::TempDir::with_prefix("kiki_").unwrap();
        let path = dir.path().join("kiki.db");
        Connection::open(&path)
            .unwrap()
            .execute_batch("CREATE TABLE t (x)")
            .unwrap();
        let db = Db::open(&path, options).unwrap();
        (dir, db)
    }

    #[test]
    fn readers_cannot_write() {
        let (_dir, db) = open(DbOptions::default());
        let err = db
            .read_blocking(|c| c.execute("INSERT INTO t VALUES (1)", []))
            .unwrap()
            .unwrap_err();
        assert!(err.to_string().contains("readonly"), "{err}");
        db.write_blocking(|c| c.execute("INSERT INTO t VALUES (1)", []))
            .unwrap()
            .unwrap();
    }

    /// A second checkout on a thread that holds one is refused at once,
    /// even from the other pool, rather than waiting on the pool.
    #[test]
    fn nested_checkouts_are_refused() {
        let (_dir, db) = open(DbOptions {
            connection_timeout: Some(Duration::from_secs(60)),
            ..Default::default()
        });
        let inner = db
            .write_blocking(|_| db.write_blocking(|_| ()).map(|_| ()))
            .unwrap();
        assert!(matches!(inner, Err(DbError::Nested { .. })), "{inner:?}");
        let inner = db.write_blocking(|_| db.read_blocking(|_| ())).unwrap();
        assert!(matches!(inner, Err(DbError::Nested { .. })), "{inner:?}");

        // The refusal releases nothing it did not take: both work after.
        db.write_blocking(|_| ()).unwrap();
        db.read_blocking(|_| ()).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn async_work_runs_off_the_runtime() {
        let (_dir, db) = open(DbOptions::default());
        db.write(|c| c.execute("INSERT INTO t VALUES (1)", []))
            .await
            .unwrap()
            .unwrap();
        let n: i64 = db
            .read(|c| c.query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0)))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(n, 1);
        // Borrowing work on a multi-threaded runtime runs in place.
        let local = 2;
        let doubled = db.read_blocking(|_| local * 2).unwrap();
        assert_eq!(doubled, 4);
    }
}
