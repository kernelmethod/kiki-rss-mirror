/// Database-related functionality for Kiki.
use anyhow::{bail, Context, Result};
use rusqlite::{Connection, OpenFlags};
use std::path::Path;

pub const SCHEMA_VERSION: &'static str = "1.0";

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
        self.flags = self.flags | OpenFlags::SQLITE_OPEN_CREATE;
        self.read_write()
    }

    /// Use database at the specified path.
    pub fn at_path(mut self, path: &'a Path) -> Self {
        self.conntype = ConnectionType::File(path);
        self
    }

    /// Open connection in read-write mode.
    pub fn read_write(mut self) -> Self {
        self.flags = self.flags | OpenFlags::SQLITE_OPEN_READ_WRITE;
        self.flags = self.flags & (!OpenFlags::SQLITE_OPEN_READ_ONLY);
        self
    }

    /// Build connection instance.
    pub fn build(&self) -> Result<Connection> {
        let conn = match self.conntype {
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
            // Initialize database
            conn.execute_batch(include_str!("include/init.sql"))
                .with_context(|| "Failed to initialize database")?;
            conn.execute(
                "INSERT INTO schema_version (version) VALUES (?1)",
                (SCHEMA_VERSION,),
            )
            .with_context(|| "unable to add schema version metadata to database")?;
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
            "schema_version",
            "tags",
            "scripts",
            "feeds",
            "feed_tags",
            "feed_scripts",
            "entries",
            "entry_tags",
        ]
        .into_iter()
        .map(|s| String::from(s))
        .sorted()
        .collect::<Vec<_>>();

        assert_eq!(tables, expected_tables);

        // Check that schema information was set correctly
        let mut stmt = conn.prepare("SELECT version FROM schema_version")?;
        let schema_info = stmt
            .query_row([], |row| Ok(QueryStringResult { text: row.get(0)? }))
            .with_context(|| "Could not retrieve version information from schema_version")?;

        assert_eq!(schema_info.text, SCHEMA_VERSION);

        Ok(())
    }
}
