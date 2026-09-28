/// Database migration infrastructure for Kiki.
///
/// Migrations are sequential SQL scripts that evolve the database schema.
/// Each migration is embedded at compile time and tracked in the `migrations`
/// table so it is only applied once.
use anyhow::{Context, Result};
use rusqlite::Connection;
use std::collections::HashSet;

/// A single database migration.
pub struct Migration {
    /// Unique name for this migration (e.g. "0001_initial").
    pub name: &'static str,
    /// SQL to execute when applying this migration.
    pub sql: &'static str,
}

/// All known migrations, in order.
///
/// When adding a new migration:
/// 1. Create the SQL file in `src/db/include/migrations/`
/// 2. Append an entry to this array
pub const MIGRATIONS: &[Migration] = &[Migration {
    name: "0001_system_tags",
    sql: include_str!("include/migrations/0001_system_tags.sql"),
}];

/// SQL to create the migrations table. Safe to run on databases that already
/// have it (uses `IF NOT EXISTS`).
const CREATE_MIGRATIONS_TABLE: &str = "
    CREATE TABLE IF NOT EXISTS migrations (
        id          INTEGER PRIMARY KEY,
        name        VARCHAR NOT NULL UNIQUE,
        applied_at  DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
    );
";

/// Returns the set of migration names that have already been applied.
fn applied_migration_names(conn: &Connection) -> Result<HashSet<String>> {
    let mut stmt = conn
        .prepare("SELECT name FROM migrations ORDER BY id")
        .with_context(|| "failed to query applied migrations")?;
    let names = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .with_context(|| "failed to read applied migrations")?
        .collect::<Result<HashSet<_>, _>>()
        .with_context(|| "failed to collect applied migration names")?;
    Ok(names)
}

/// Returns the list of migrations that have not yet been applied.
///
/// This bootstraps the `migrations` table if it does not exist, making it
/// safe to call on legacy databases that only have `schema_version`.
pub fn pending_migrations(conn: &Connection) -> Result<Vec<&'static Migration>> {
    conn.execute_batch(CREATE_MIGRATIONS_TABLE)
        .with_context(|| "failed to bootstrap migrations table")?;

    let applied = applied_migration_names(conn)?;
    let pending = MIGRATIONS
        .iter()
        .filter(|m| !applied.contains(m.name))
        .collect();
    Ok(pending)
}

/// Applies all pending migrations to the database and returns how many were
/// applied.
///
/// Each migration runs in its own transaction. If a migration fails, its
/// transaction is rolled back and the error is returned — all previously
/// applied migrations in this call remain committed.
pub fn run_pending_migrations(conn: &mut Connection) -> Result<usize> {
    conn.execute_batch(CREATE_MIGRATIONS_TABLE)
        .with_context(|| "failed to bootstrap migrations table")?;

    let applied = applied_migration_names(conn)?;
    let mut count: usize = 0;

    for migration in MIGRATIONS {
        if applied.contains(migration.name) {
            continue;
        }

        let tx = conn.transaction().with_context(|| {
            format!(
                "failed to begin transaction for migration {}",
                migration.name
            )
        })?;

        tx.execute_batch(migration.sql)
            .with_context(|| format!("failed to execute migration {}", migration.name))?;

        tx.execute(
            "INSERT INTO migrations (name) VALUES (?1)",
            (migration.name,),
        )
        .with_context(|| {
            format!(
                "failed to record migration {} in migrations table",
                migration.name
            )
        })?;

        tx.commit()
            .with_context(|| format!("failed to commit migration {}", migration.name))?;

        tracing::info!("Applied migration: {}", migration.name);
        count += 1;
    }

    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::ConnectionBuilder;
    use anyhow::Result;

    /// A freshly-initialized database should have all known migrations
    /// recorded in the migrations table.
    #[test]
    fn test_fresh_db_has_all_migrations() -> Result<()> {
        let conn = ConnectionBuilder::default().in_memory().create().build()?;
        let applied = applied_migration_names(&conn)?;

        for migration in MIGRATIONS {
            assert!(
                applied.contains(migration.name),
                "migration {} should be recorded in fresh database",
                migration.name
            );
        }

        Ok(())
    }

    /// Running the migration runner on a fresh database should be a no-op
    /// since all migrations are already marked as applied during init.
    #[test]
    fn test_run_pending_on_fresh_db_is_noop() -> Result<()> {
        let mut conn = ConnectionBuilder::default().in_memory().create().build()?;
        let count = run_pending_migrations(&mut conn)?;
        assert_eq!(
            count, 0,
            "no migrations should be applied on fresh database"
        );
        Ok(())
    }

    /// The tag-related parts of the schema before any migrations.
    const LEGACY_TAG_SCHEMA: &str = "
        CREATE TABLE tags (
            id      INTEGER PRIMARY KEY,
            name    VARCHAR UNIQUE NOT NULL
        );
        CREATE TABLE feed_tags (feed_id INTEGER NOT NULL, tag_id INTEGER NOT NULL);
        CREATE TABLE entry_tags (entry_id INTEGER NOT NULL, tag_id INTEGER NOT NULL);
        CREATE INDEX idx_entry_tags_entry_id ON entry_tags(entry_id);
        CREATE INDEX idx_entry_tags_tag_id ON entry_tags(tag_id);
    ";

    /// `0001_system_tags` adds the tag kind to a database created before
    /// system tags existed, renaming any user tags that used the now-reserved
    /// `system:` prefix.
    #[test]
    fn test_system_tags_migration() -> Result<()> {
        let mut conn = rusqlite::Connection::open_in_memory()?;
        conn.execute_batch(LEGACY_TAG_SCHEMA)?;
        conn.execute_batch(
            "INSERT INTO tags (name) VALUES ('news'), ('system:read'), ('System:Other');",
        )?;

        let count = run_pending_migrations(&mut conn)?;
        assert_eq!(count, MIGRATIONS.len());

        let mut stmt = conn.prepare("SELECT name, kind FROM tags ORDER BY id")?;
        let tags = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let expected = [
            ("news", "user"),
            ("user:system:read", "user"),
            ("user:System:Other", "user"),
            ("system:read", "system"),
            ("system:saved", "system"),
            ("system:hidden", "system"),
        ]
        .map(|(n, k)| (n.to_string(), k.to_string()));
        assert_eq!(tags, expected);

        Ok(())
    }

    /// `0001_system_tags` also removes duplicate tag links and prevents new
    /// ones.
    #[test]
    fn test_unique_tag_links_migration() -> Result<()> {
        let mut conn = rusqlite::Connection::open_in_memory()?;
        conn.execute_batch(LEGACY_TAG_SCHEMA)?;
        conn.execute_batch(
            "INSERT INTO entry_tags VALUES (1, 1), (1, 1), (1, 2), (2, 1), (1, 1);
             INSERT INTO feed_tags VALUES (1, 1), (1, 1), (2, 1);",
        )?;

        run_pending_migrations(&mut conn)?;

        let links = |table: &str| -> Result<Vec<(i64, i64)>> {
            let mut stmt = conn.prepare(&format!("SELECT * FROM {table} ORDER BY 1, 2"))?;
            let rows = stmt
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        };
        assert_eq!(links("entry_tags")?, [(1, 1), (1, 2), (2, 1)]);
        assert_eq!(links("feed_tags")?, [(1, 1), (2, 1)]);

        assert!(conn
            .execute("INSERT INTO entry_tags VALUES (1, 2)", [])
            .is_err());
        assert!(conn
            .execute("INSERT INTO feed_tags VALUES (2, 1)", [])
            .is_err());

        Ok(())
    }
}
