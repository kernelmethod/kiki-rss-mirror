//! Connection pooling for the SQLite database.
//!
//! [`ConnectionManager`] wraps r2d2_sqlite's [`SqliteConnectionManager`] so
//! that, when given a [`Metrics`] recorder, every connection returned to the
//! pool reports the page I/O it did while it was checked out. SQLite tracks
//! this per connection (see `sqlite3_db_status`), so reads and writes are
//! counted without instrumenting individual queries.

use crate::metrics::Metrics;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{ffi, Connection};
use std::os::raw::c_int;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// A pool of connections to the kiki database. Only [`super::Db`] holds
/// one; everything else goes through it.
pub(super) type Pool = r2d2::Pool<ConnectionManager>;

/// Run blocking database work from async code without stalling the runtime.
///
/// Checking a connection out of a pool can block for the pool's whole
/// connection timeout, and queries block for as long as SQLite takes. Done
/// directly on a Tokio worker thread, that stops every other task scheduled
/// on the thread, including tasks that would return a connection to the
/// pool. On a multi-threaded runtime `f` therefore runs under
/// [`tokio::task::block_in_place`], which first moves this thread's other
/// tasks to another thread. On a current-thread runtime, or outside any
/// runtime, `f` simply runs, since there is nowhere else to move them.
///
/// Unlike [`tokio::task::spawn_blocking`], `f` may borrow from the caller.
/// Prefer `spawn_blocking` when the work owns everything it uses.
///
/// # Examples
///
/// ```
/// let (tx, rx) = std::sync::mpsc::channel();
/// tx.send(1)?;
/// // Stands in for work that would stall the runtime.
/// let one = kiki_rss::db::blocking(|| rx.recv())?;
/// assert_eq!(one, 1);
/// # Ok::<(), anyhow::Error>(())
/// ```
pub fn blocking<T>(f: impl FnOnce() -> T) -> T {
    use tokio::runtime::{Handle, RuntimeFlavor};
    match Handle::try_current() {
        Ok(h) if h.runtime_flavor() == RuntimeFlavor::MultiThread => tokio::task::block_in_place(f),
        _ => f(),
    }
}

/// SQLite's default page size, used until a connection reports the real one.
const DEFAULT_PAGE_SIZE: u64 = 4096;

/// An r2d2 connection manager that records database I/O into [`Metrics`].
///
/// Without metrics attached it behaves exactly like the wrapped
/// [`SqliteConnectionManager`].
///
/// # Examples
///
/// ```
/// use kiki_rss::db::ConnectionManager;
/// use kiki_rss::metrics::Metrics;
/// use r2d2_sqlite::SqliteConnectionManager;
/// use std::sync::Arc;
///
/// let metrics = Arc::new(Metrics::new()?);
/// let manager = ConnectionManager::new(SqliteConnectionManager::memory()).with_metrics(metrics);
/// let pool = r2d2::Pool::new(manager)?;
/// pool.get()?.execute_batch("CREATE TABLE t (x); INSERT INTO t VALUES (1);")?;
/// # Ok::<(), anyhow::Error>(())
/// ```
pub struct ConnectionManager {
    inner: SqliteConnectionManager,
    metrics: Option<Arc<Metrics>>,
    /// Page size of the database, learned when a connection is opened. Every
    /// connection in a pool opens the same database, so one value suffices.
    page_size: AtomicU64,
}

impl ConnectionManager {
    /// Wrap `inner`, without recording any metrics.
    pub fn new(inner: SqliteConnectionManager) -> Self {
        Self {
            inner,
            metrics: None,
            page_size: AtomicU64::new(DEFAULT_PAGE_SIZE),
        }
    }

    /// Record the I/O of every connection checked back in to `metrics`.
    pub fn with_metrics(mut self, metrics: Arc<Metrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    fn record_io(&self, metrics: &Metrics, conn: &Connection) {
        let cache_hits = take_db_status(conn, ffi::SQLITE_DBSTATUS_CACHE_HIT);
        let page_reads = take_db_status(conn, ffi::SQLITE_DBSTATUS_CACHE_MISS);
        let page_writes = take_db_status(conn, ffi::SQLITE_DBSTATUS_CACHE_WRITE);
        metrics.record_db_io(
            cache_hits,
            page_reads,
            page_writes,
            self.page_size.load(Ordering::Relaxed),
        );
    }
}

impl From<SqliteConnectionManager> for ConnectionManager {
    fn from(inner: SqliteConnectionManager) -> Self {
        Self::new(inner)
    }
}

impl std::fmt::Debug for ConnectionManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionManager")
            .field("inner", &self.inner)
            .field("metrics", &self.metrics.is_some())
            .finish_non_exhaustive()
    }
}

impl r2d2::ManageConnection for ConnectionManager {
    type Connection = Connection;
    type Error = rusqlite::Error;

    fn connect(&self) -> Result<Connection, rusqlite::Error> {
        let conn = self.inner.connect()?;
        if self.metrics.is_some() {
            let page_size: i64 = conn.query_row("PRAGMA page_size", [], |row| row.get(0))?;
            if page_size > 0 {
                self.page_size.store(page_size as u64, Ordering::Relaxed);
            }
        }
        Ok(conn)
    }

    fn is_valid(&self, conn: &mut Connection) -> Result<(), rusqlite::Error> {
        self.inner.is_valid(conn)
    }

    /// Called by r2d2 each time a connection is returned to the pool, which
    /// makes it the one place that sees every connection after use.
    fn has_broken(&self, conn: &mut Connection) -> bool {
        if let Some(metrics) = &self.metrics {
            self.record_io(metrics, conn);
        }
        self.inner.has_broken(conn)
    }
}

/// Read one of SQLite's per-connection counters and reset it to zero, so the
/// next read returns only what happened in between.
fn take_db_status(conn: &Connection, op: c_int) -> u64 {
    let mut current: c_int = 0;
    let mut highwater: c_int = 0;
    // SAFETY: the handle stays valid while `conn` is borrowed, and r2d2 only
    // passes a connection to the manager while no one else is using it.
    let rc = unsafe { ffi::sqlite3_db_status(conn.handle(), op, &mut current, &mut highwater, 1) };
    if rc == ffi::SQLITE_OK {
        current.max(0) as u64
    } else {
        0
    }
}

#[cfg(all(test, feature = "metrics"))]
mod tests {
    use super::*;

    /// Sum of the samples for `name` in the rendered output.
    fn sample(metrics: &Metrics, name: &str) -> anyhow::Result<f64> {
        let mut total = 0.0;
        for (series, value) in metrics
            .render()
            .lines()
            .filter(|l| !l.starts_with('#'))
            .filter_map(|l| l.rsplit_once(' '))
        {
            if series == name {
                total += value.parse::<f64>()?;
            }
        }
        Ok(total)
    }

    #[test]
    fn checkin_records_page_io() -> anyhow::Result<()> {
        let td = tempfile::TempDir::with_prefix("kiki_")?;
        let path = td.path().join("kiki.db");
        let metrics = Arc::new(Metrics::new()?);
        let manager = ConnectionManager::new(
            SqliteConnectionManager::file(&path)
                .with_init(|c| c.execute_batch("PRAGMA page_size=8192; PRAGMA journal_mode=WAL;")),
        )
        .with_metrics(metrics.clone());
        let pool: Pool = r2d2::Pool::builder().max_size(1).build(manager)?;

        pool.get()?.execute_batch(
            "CREATE TABLE t (x BLOB);
             WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 100)
             INSERT INTO t SELECT randomblob(1000) FROM n;",
        )?;
        let writes = sample(&metrics, "kiki_db_page_writes_total")?;
        assert!(writes > 0.0, "expected page writes, got {writes}");
        assert_eq!(
            sample(&metrics, "kiki_db_write_bytes_total")?,
            writes * 8192.0
        );

        // Counters are reset on checkin, so a read-only checkout adds no
        // writes, only reads (from the cache or the file).
        let n: i64 = pool
            .get()?
            .query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))?;
        assert_eq!(n, 100);
        assert_eq!(sample(&metrics, "kiki_db_page_writes_total")?, writes);
        let reads = sample(&metrics, "kiki_db_page_cache_hits_total")?
            + sample(&metrics, "kiki_db_page_reads_total")?;
        assert!(reads > 0.0, "expected page reads, got {reads}");
        assert_eq!(
            sample(&metrics, "kiki_db_read_bytes_total")?,
            sample(&metrics, "kiki_db_page_reads_total")? * 8192.0
        );
        Ok(())
    }
}
