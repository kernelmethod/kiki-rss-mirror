use anyhow::Result;
use r2d2::{Pool, PooledConnection};
use r2d2_sqlite::SqliteConnectionManager;
use tracing::{info, warn};

/// Run a maintenance operation and, on success, update its `task_queue`
/// row so the schedule survives server restarts.
pub(crate) fn run_maintenance<F>(
    pool: &Pool<SqliteConnectionManager>,
    task_type: &str,
    label: &str,
    op: F,
) where
    F: FnOnce(&PooledConnection<SqliteConnectionManager>) -> Result<()>,
{
    let conn = match pool.get() {
        Ok(c) => c,
        Err(e) => {
            warn!(
                "{} skipped: failed to acquire DB connection: {:?}",
                label, e
            );
            return;
        }
    };
    match op(&conn) {
        Ok(()) => {
            info!("{} completed", label);
            if let Err(e) = crate::db::task_queue::record_run(&conn, task_type) {
                warn!("Failed to record {} run: {:?}", label, e);
            }
        }
        Err(e) => warn!("{} failed: {:?}", label, e),
    }
}
