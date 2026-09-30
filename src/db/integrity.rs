//! Checking the database for corruption, and backing it up before its
//! schema changes.

use crate::db::Db;
use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::{Path, PathBuf};

/// The most problems [`quick_check`] reports; SQLite stops looking after
/// this many.
const MAX_PROBLEMS: u32 = 20;

/// Run `PRAGMA quick_check` on a read connection, and return the problems
/// it found, if any.
///
/// `quick_check` reads the whole database, but does not check that indexes
/// match their tables, so it runs in time linear in the database's size.
/// Under WAL it does not hold up writes.
///
/// # Errors
///
/// Returns an error if no connection can be had, or the check cannot run.
pub fn quick_check(db: &Db) -> Result<Vec<String>> {
    let rows = db.read_blocking(|conn| -> rusqlite::Result<Vec<String>> {
        conn.prepare(&format!("PRAGMA quick_check({MAX_PROBLEMS})"))?
            .query_map([], |row| row.get(0))?
            .collect()
    })??;
    Ok(problems(rows))
}

/// The problems in the rows `quick_check` returned: none if it returned
/// the single row `ok`.
fn problems(rows: Vec<String>) -> Vec<String> {
    if rows.len() == 1 && rows.first().is_some_and(|r| r == "ok") {
        Vec::new()
    } else {
        rows
    }
}

/// Where to back up the database at `db_path` before applying
/// `migration`, taken at `now` (a Unix timestamp): next to it, named after
/// both, so that backups taken before different migrations, or at
/// different times, never collide.
pub fn backup_path(db_path: &Path, migration: &str, now: i64) -> PathBuf {
    let mut name = db_path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_else(|| "kiki.db".into());
    name.push(format!(".pre-{migration}.{now}.bak"));
    db_path.with_file_name(name)
}

/// Write a consistent copy of the database `conn` is open on to `dest`,
/// with `VACUUM INTO`, which also compacts it.
///
/// # Errors
///
/// Returns an error if `dest` already exists, or the copy cannot be
/// written.
pub fn backup(conn: &Connection, dest: &Path) -> Result<()> {
    let dest_str = dest
        .to_str()
        .with_context(|| format!("backup path {dest:?} is not valid UTF-8"))?;
    conn.execute("VACUUM INTO ?1", [dest_str])
        .with_context(|| format!("failed to back up the database to {dest:?}"))?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::db::ConnectionBuilder;

    #[test]
    fn a_healthy_database_has_no_problems() -> Result<()> {
        let td = tempfile::TempDir::with_prefix("kiki_")?;
        let path = td.path().join("kiki.db");
        ConnectionBuilder::default()
            .at_path(&path)
            .create()
            .build()?;
        let db = Db::open(&path, Default::default())?;
        assert!(quick_check(&db)?.is_empty());
        Ok(())
    }

    #[test]
    fn anything_but_a_lone_ok_is_a_problem() {
        assert!(problems(vec!["ok".into()]).is_empty());
        assert_eq!(
            problems(vec!["row 3 missing from index x".into()]),
            vec!["row 3 missing from index x".to_string()]
        );
    }

    #[test]
    fn backups_are_named_after_the_database_and_migration() {
        assert_eq!(
            backup_path(Path::new("/var/lib/kiki/kiki.db"), "0009_x", 1700000000),
            Path::new("/var/lib/kiki/kiki.db.pre-0009_x.1700000000.bak")
        );
    }

    #[test]
    fn backups_hold_the_data_and_are_never_overwritten() -> Result<()> {
        let td = tempfile::TempDir::with_prefix("kiki_")?;
        let path = td.path().join("kiki.db");
        let conn = ConnectionBuilder::default()
            .at_path(&path)
            .create()
            .build()?;
        conn.execute(
            "INSERT INTO feeds (title, url) VALUES ('t', 'https://example.com/')",
            [],
        )?;

        let dest = backup_path(&path, "0009_x", 1);
        backup(&conn, &dest)?;
        let copy = Connection::open(&dest)?;
        let n: i64 = copy.query_row("SELECT COUNT(*) FROM feeds", [], |r| r.get(0))?;
        assert_eq!(n, 1);

        assert!(backup(&conn, &dest).is_err());
        Ok(())
    }
}
