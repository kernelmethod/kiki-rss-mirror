use crate::db::Db;
use anyhow::Result;
use rusqlite::Connection;
use tracing::{info, warn};

/// Run a maintenance operation on the writer and, on success, update its
/// `task_queue` row so the schedule survives server restarts.
pub(crate) fn run_maintenance<F>(db: &Db, task_type: &str, label: &str, op: F)
where
    F: FnOnce(&Connection) -> Result<()>,
{
    let ran = db.write_blocking(|conn| match op(conn) {
        Ok(()) => {
            info!("{} completed", label);
            if let Err(e) = crate::db::task_queue::record_run(conn, task_type) {
                warn!("Failed to record {} run: {:?}", label, e);
            }
        }
        Err(e) => warn!("{} failed: {:?}", label, e),
    });
    if let Err(e) = ran {
        warn!(
            "{} skipped: failed to acquire DB connection: {:?}",
            label, e
        );
    }
}
