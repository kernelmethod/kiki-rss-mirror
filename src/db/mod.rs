/// Database-related functionality for Kiki.
use anyhow::{bail, Context, Result};
use rusqlite::{Connection, OpenFlags};
use std::path::Path;

pub mod assets;
pub mod favicons;
pub mod feeds;
mod handle;
pub mod log;
pub mod migrations;
pub mod plugins;
mod pool;
pub mod profile;
pub mod retention;
pub mod tags;
pub mod task_queue;

pub use handle::{Db, DbError, DbOptions, DEFAULT_READERS};
pub use pool::{blocking, ConnectionManager};

enum ConnectionType<'a> {
    DefaultConnection,
    Memory,
    File(&'a Path),
}

pub struct ConnectionBuilder<'a> {
    conntype: ConnectionType<'a>,
    create: bool,
    flags: OpenFlags,
}

impl<'a> ConnectionBuilder<'a> {
    /// Connect to an in-memory database.
    pub fn in_memory(mut self) -> Self {
        self.conntype = ConnectionType::Memory;
        self
    }

    /// Create database if it does not already exists.
    pub fn create(mut self) -> Self {
        self.create = true;
        self.flags |= OpenFlags::SQLITE_OPEN_CREATE;
        self.read_write()
    }

    /// Use database at the specified path.
    pub fn at_path(mut self, path: &'a Path) -> Self {
        self.conntype = ConnectionType::File(path);
        self
    }

    /// Open connection in read-write mode.
    pub fn read_write(mut self) -> Self {
        self.flags |= OpenFlags::SQLITE_OPEN_READ_WRITE;
        self.flags &= !OpenFlags::SQLITE_OPEN_READ_ONLY;
        self
    }

    /// Build connection instance.
    pub fn build(&self) -> Result<Connection> {
        let mut conn = match self.conntype {
            ConnectionType::Memory => Connection::open_in_memory_with_flags(self.flags)
                .with_context(|| "unable to open in-memory database connection")?,
            ConnectionType::DefaultConnection | ConnectionType::File(_) => {
                let path = match self.conntype {
                    ConnectionType::DefaultConnection => Path::new("kiki.db"),
                    ConnectionType::File(p) => p,
                    _ => bail!("unreachable code"),
                };

                if !self.create && !path.exists() {
                    bail!(format!("trying to establish connection to database at {:?} that doesn't exist; you may need to call `create()`", path))
                }

                Connection::open_with_flags(path, self.flags).with_context(|| {
                    format!("unable to open connection to database at {:?}", path)
                })?
            }
        };

        conn.execute("PRAGMA foreign_keys = ON;", ())
            .with_context(|| "unable to enable foreign keys on database connection")?;

        if self.create {
            // Initialize the database in a single transaction. Otherwise each
            // statement commits (and syncs to disk) on its own, which makes
            // initialization an order of magnitude slower, and a failure
            // partway through leaves a half-built schema behind.
            //
            // init.sql's `PRAGMA auto_vacuum` still takes effect in the
            // transaction, since no tables exist yet; its `PRAGMA
            // foreign_keys` is a no-op there, but foreign keys were already
            // enabled on this connection above.
            let tx = conn
                .transaction()
                .with_context(|| "unable to start database initialization")?;
            tx.execute_batch(include_str!("include/init.sql"))
                .with_context(|| "Failed to initialize database")?;

            // Mark all known migrations as applied since init.sql
            // contains the complete current schema
            for migration in migrations::MIGRATIONS {
                tx.execute(
                    "INSERT INTO migrations (name) VALUES (?1)",
                    (migration.name,),
                )
                .with_context(|| {
                    format!("unable to record migration {} during init", migration.name)
                })?;
            }
            tx.commit()
                .with_context(|| "unable to commit database initialization")?;
        }

        Ok(conn)
    }
}

impl Default for ConnectionBuilder<'_> {
    fn default() -> Self {
        let flags = OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_READ_ONLY;
        ConnectionBuilder {
            conntype: ConnectionType::DefaultConnection,
            create: false,
            flags,
        }
    }
}

/// Size of the database in bytes, computed as `page_count * page_size`.
///
/// This is the size of the main database file as SQLite sees it, including
/// pages on the freelist that have not been vacuumed yet. It does not include
/// the write-ahead log, whose contents only reach the main file when the WAL
/// is checkpointed.
///
/// # Errors
///
/// Returns an error if the pragmas cannot be queried.
///
/// # Examples
///
/// ```
/// use kiki_rss::db::{self, ConnectionBuilder};
///
/// let conn = ConnectionBuilder::default().in_memory().create().build()?;
/// assert!(db::size_bytes(&conn)? > 0);
/// # Ok::<(), anyhow::Error>(())
/// ```
pub fn size_bytes(conn: &Connection) -> Result<u64> {
    let bytes: i64 = conn
        .query_row(
            "SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()",
            [],
            |row| row.get(0),
        )
        .with_context(|| "unable to query database size")?;
    Ok(bytes.max(0) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use itertools::Itertools;

    #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
    struct QueryStringResult {
        text: String,
    }

    #[test]
    fn test_init_database() -> Result<()> {
        let conn = ConnectionBuilder::default()
            .in_memory()
            .create()
            .build()
            .with_context(|| "unable to open connection to database")?;

        // Check that tables were all correctly constructed
        let mut stmt = conn.prepare("SELECT tbl_name FROM sqlite_master")?;
        let tables = stmt
            .query_map([], |row| Ok(QueryStringResult { text: row.get(0)? }))?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .unique()
            .sorted()
            .map(|r| r.text)
            .collect::<Vec<_>>();

        let expected_tables = [
            "atom_categories",
            "atom_entry_authors",
            "atom_entry_categories",
            "atom_entry_contributors",
            "atom_entry_contributors_all",
            "atom_entry_rights",
            "atom_feed_authors",
            "atom_feed_categories",
            "atom_feed_contributors",
            "atom_feed_data",
            "atom_feed_generators",
            "atom_feed_icons",
            "atom_feed_logos",
            "atom_feed_rights",
            "atom_source_data",
            "entries",
            "entries_fts",
            "entries_fts_config",
            "entries_fts_data",
            "entries_fts_docsize",
            "entries_fts_idx",
            "entry_assets",
            "entry_sources",
            "entry_tags",
            "feed_assets",
            "feed_favicons",
            "feed_tags",
            "feeds",
            "migrations",
            "plugin_store",
            "plugins",
            "rss_categories",
            "rss_entry_data",
            "tags",
            "task_queue",
        ]
        .into_iter()
        .map(String::from)
        .sorted()
        .collect::<Vec<_>>();

        assert_eq!(tables, expected_tables);

        // Check that all known migrations are recorded
        let mut stmt = conn.prepare("SELECT name FROM migrations ORDER BY id")?;
        let applied = stmt
            .query_map([], |row| Ok(QueryStringResult { text: row.get(0)? }))?
            .collect::<Result<Vec<_>, _>>()?;

        let expected_migrations: Vec<String> = migrations::MIGRATIONS
            .iter()
            .map(|m| String::from(m.name))
            .collect();
        let applied_names: Vec<String> = applied.into_iter().map(|r| r.text).collect();
        assert_eq!(applied_names, expected_migrations);

        Ok(())
    }

    /// The pragmas at the top of init.sql still apply to a database file
    /// even though initialization runs inside a transaction.
    #[test]
    fn test_init_database_file_settings() -> Result<()> {
        let td = tempfile::TempDir::with_prefix("kiki_")?;
        let path = td.path().join("kiki.db");
        ConnectionBuilder::default()
            .at_path(&path)
            .create()
            .build()?;

        let conn = ConnectionBuilder::default().at_path(&path).build()?;
        // 2 = INCREMENTAL
        let auto_vacuum: i64 = conn.query_row("PRAGMA auto_vacuum", [], |r| r.get(0))?;
        assert_eq!(auto_vacuum, 2);
        let foreign_keys: i64 = conn.query_row("PRAGMA foreign_keys", [], |r| r.get(0))?;
        assert_eq!(foreign_keys, 1);

        Ok(())
    }

    /// Once the WAL is checkpointed, the reported size matches the size of
    /// the database file on disk.
    #[test]
    fn test_size_bytes_matches_file() -> Result<()> {
        let td = tempfile::TempDir::with_prefix("kiki_")?;
        let path = td.path().join("kiki.db");
        let conn = ConnectionBuilder::default()
            .at_path(&path)
            .create()
            .build()?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_checkpoint(TRUNCATE);")?;

        let size = size_bytes(&conn)?;
        assert!(size > 0);
        assert_eq!(size, std::fs::metadata(&path)?.len());

        Ok(())
    }
}
