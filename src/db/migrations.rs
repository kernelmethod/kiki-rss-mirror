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
pub const MIGRATIONS: &[Migration] = &[
    Migration {
        name: "0001_system_tags",
        sql: include_str!("include/migrations/0001_system_tags.sql"),
    },
    Migration {
        name: "0002_script_config",
        sql: include_str!("include/migrations/0002_script_config.sql"),
    },
    Migration {
        name: PLUGINS_MIGRATION,
        sql: include_str!("include/migrations/0003_plugins.sql"),
    },
    Migration {
        name: "0004_plugin_config",
        sql: include_str!("include/migrations/0004_plugin_config.sql"),
    },
    Migration {
        name: "0005_plugin_store",
        sql: include_str!("include/migrations/0005_plugin_store.sql"),
    },
    Migration {
        name: "0006_feed_favicons",
        sql: include_str!("include/migrations/0006_feed_favicons.sql"),
    },
    Migration {
        name: "0007_protect_system_tags",
        sql: include_str!("include/migrations/0007_protect_system_tags.sql"),
    },
    Migration {
        name: "0008_unique_feed_urls",
        sql: include_str!("include/migrations/0008_unique_feed_urls.sql"),
    },
    Migration {
        name: "0009_pending_entry_assets",
        sql: include_str!("include/migrations/0009_pending_entry_assets.sql"),
    },
    Migration {
        name: "0010_entry_sync",
        sql: include_str!("include/migrations/0010_entry_sync.sql"),
    },
    Migration {
        name: "0011_unread_entries",
        sql: include_str!("include/migrations/0011_unread_entries.sql"),
    },
    Migration {
        name: "0012_fts_unchanged_entries",
        sql: include_str!("include/migrations/0012_fts_unchanged_entries.sql"),
    },
];

/// The migration that drops the `scripts` table in favour of plugins.
pub const PLUGINS_MIGRATION: &str = "0003_plugins";

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

    /// The parts of the schema that migrations touch, as they were before
    /// any migrations.
    const LEGACY_SCHEMA: &str = "
        CREATE TABLE feeds (
            id      INTEGER PRIMARY KEY,
            title   VARCHAR NOT NULL,
            url     VARCHAR
        );
        CREATE TABLE scripts (
            id      INTEGER PRIMARY KEY,
            engine  VARCHAR NOT NULL,
            text    VARCHAR NOT NULL,
            kind    VARCHAR NOT NULL
        );
        CREATE TABLE tags (
            id      INTEGER PRIMARY KEY,
            name    VARCHAR UNIQUE NOT NULL
        );
        CREATE TABLE feed_tags (feed_id INTEGER NOT NULL, tag_id INTEGER NOT NULL);
        CREATE TABLE entries (
            id              INTEGER PRIMARY KEY,
            feed_id         INTEGER,
            published_at    DATETIME NOT NULL
        );
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
        conn.execute_batch(LEGACY_SCHEMA)?;
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
        conn.execute_batch(LEGACY_SCHEMA)?;
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

    /// `0002_script_config` gives existing scripts an empty config.
    #[test]
    fn test_script_config_migration() -> Result<()> {
        let conn = rusqlite::Connection::open_in_memory()?;
        conn.execute_batch(LEGACY_SCHEMA)?;
        conn.execute_batch(
            "INSERT INTO scripts (engine, text, kind) VALUES ('lua', '-- x', 'user');",
        )?;

        for migration in MIGRATIONS
            .iter()
            .take_while(|m| m.name != PLUGINS_MIGRATION)
        {
            conn.execute_batch(migration.sql)?;
        }

        let config: String = conn.query_row("SELECT config FROM scripts", [], |row| row.get(0))?;
        assert_eq!(config, "{}");

        Ok(())
    }

    /// `0003_plugins` drops the tables scripts used to be stored in.
    #[test]
    fn test_plugins_migration() -> Result<()> {
        let mut conn = rusqlite::Connection::open_in_memory()?;
        conn.execute_batch(LEGACY_SCHEMA)?;
        conn.execute_batch(
            "CREATE TABLE feed_scripts (feed_id INTEGER NOT NULL, script_id INTEGER NOT NULL);",
        )?;

        run_pending_migrations(&mut conn)?;

        let remaining: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name IN ('scripts', 'feed_scripts')",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(remaining, 0);

        Ok(())
    }

    /// `0004_plugin_config` adds the table plugin config overrides are kept
    /// in, with the same shape as a freshly-initialized database's.
    #[test]
    fn test_plugin_config_migration() -> Result<()> {
        let mut conn = rusqlite::Connection::open_in_memory()?;
        conn.execute_batch(LEGACY_SCHEMA)?;
        run_pending_migrations(&mut conn)?;

        let fresh = ConnectionBuilder::default().in_memory().create().build()?;
        let columns = |conn: &Connection| -> Result<Vec<(String, String)>> {
            let mut stmt = conn.prepare("SELECT name, type FROM pragma_table_info('plugins')")?;
            let rows = stmt
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        };
        assert_eq!(columns(&conn)?, columns(&fresh)?);
        assert!(!columns(&conn)?.is_empty());

        crate::db::plugins::set_config_overrides(
            &conn,
            "p",
            serde_json::json!({"a": 1})
                .as_object()
                .ok_or_else(|| anyhow::anyhow!("object"))?,
        )?;

        Ok(())
    }

    /// `0005_plugin_store` adds the table plugins' key-value stores are
    /// kept in, with the same shape as a freshly-initialized database's.
    #[test]
    fn test_plugin_store_migration() -> Result<()> {
        let mut conn = rusqlite::Connection::open_in_memory()?;
        conn.execute_batch(LEGACY_SCHEMA)?;
        run_pending_migrations(&mut conn)?;

        let fresh = ConnectionBuilder::default().in_memory().create().build()?;
        let sql = |conn: &Connection| -> Result<String> {
            Ok(conn.query_row(
                "SELECT sql FROM sqlite_master WHERE name = 'plugin_store'",
                [],
                |row| row.get(0),
            )?)
        };
        assert_eq!(sql(&conn)?, sql(&fresh)?);

        crate::db::plugins::store_set(&conn, "p", "k", Some(&serde_json::json!(1)))?;
        Ok(())
    }

    /// `0006_feed_favicons` adds `feeds.site_url` and the table favicons
    /// are recorded in, with the same shape as a freshly-initialized
    /// database's.
    #[test]
    fn test_feed_favicons_migration() -> Result<()> {
        let mut conn = rusqlite::Connection::open_in_memory()?;
        conn.execute_batch(LEGACY_SCHEMA)?;
        conn.execute_batch("INSERT INTO feeds (title, url) VALUES ('f', 'http://x/');")?;
        run_pending_migrations(&mut conn)?;

        let fresh = ConnectionBuilder::default().in_memory().create().build()?;
        let sql = |conn: &Connection, name: &str| -> Result<String> {
            Ok(conn.query_row(
                "SELECT sql FROM sqlite_master WHERE name = ?1",
                [name],
                |row| row.get(0),
            )?)
        };
        for name in ["feed_favicons", "idx_feed_favicons_asset"] {
            assert_eq!(sql(&conn, name)?, sql(&fresh, name)?);
        }

        let site_url: Option<String> =
            conn.query_row("SELECT site_url FROM feeds", [], |row| row.get(0))?;
        assert_eq!(site_url, None);
        Ok(())
    }

    /// `0007_protect_system_tags` adds the trigger that stops system tags
    /// from being deleted, with the same shape as a freshly-initialized
    /// database's.
    #[test]
    fn test_protect_system_tags_migration() -> Result<()> {
        let mut conn = rusqlite::Connection::open_in_memory()?;
        conn.execute_batch(LEGACY_SCHEMA)?;
        conn.execute_batch("INSERT INTO tags (name) VALUES ('news');")?;
        run_pending_migrations(&mut conn)?;

        let fresh = ConnectionBuilder::default().in_memory().create().build()?;
        let sql = |conn: &Connection| -> Result<String> {
            Ok(conn.query_row(
                "SELECT sql FROM sqlite_master WHERE name = 'protect_system_tags'",
                [],
                |row| row.get(0),
            )?)
        };
        assert_eq!(sql(&conn)?, sql(&fresh)?);

        assert!(conn
            .execute("DELETE FROM tags WHERE name = 'system:read'", [])
            .is_err());
        assert_eq!(conn.execute("DELETE FROM tags WHERE name = 'news'", [])?, 1);
        Ok(())
    }

    /// `0008_unique_feed_urls` adds the unique index on `feeds.url`, with
    /// the same shape as a freshly-initialized database's.
    #[test]
    fn test_unique_feed_urls_migration() -> Result<()> {
        let mut conn = rusqlite::Connection::open_in_memory()?;
        conn.execute_batch(LEGACY_SCHEMA)?;
        conn.execute_batch("INSERT INTO feeds (title, url) VALUES ('f', 'http://x/');")?;
        run_pending_migrations(&mut conn)?;

        let fresh = ConnectionBuilder::default().in_memory().create().build()?;
        let sql = |conn: &Connection| -> Result<String> {
            Ok(conn.query_row(
                "SELECT sql FROM sqlite_master WHERE name = 'idx_feeds_url_unique'",
                [],
                |row| row.get(0),
            )?)
        };
        assert_eq!(sql(&conn)?, sql(&fresh)?);

        assert!(conn
            .execute(
                "INSERT INTO feeds (title, url) VALUES ('g', 'http://x/')",
                []
            )
            .is_err());
        Ok(())
    }

    /// `0010_entry_sync` adds `entries.ingested_at`, backfilled from each
    /// entry's publication time but never later than now, and the id
    /// high-water mark, starting at the highest id in use, with the same
    /// shape as a freshly-initialized database's.
    #[test]
    fn test_entry_sync_migration() -> Result<()> {
        let mut conn = rusqlite::Connection::open_in_memory()?;
        conn.execute_batch(LEGACY_SCHEMA)?;
        let far_future = 32503680000i64; // the year 3000
        conn.execute(
            "INSERT INTO entries (id, published_at) VALUES (1, 1700000000), (7, ?1)",
            [far_future],
        )?;
        run_pending_migrations(&mut conn)?;

        let fresh = ConnectionBuilder::default().in_memory().create().build()?;
        let sql = |conn: &Connection, name: &str| -> Result<String> {
            Ok(conn.query_row(
                "SELECT sql FROM sqlite_master WHERE name = ?1",
                [name],
                |row| row.get(0),
            )?)
        };
        for name in [
            "idx_entry_ingested_at",
            "entry_id_high_water",
            "entries_id_high_water",
        ] {
            assert_eq!(sql(&conn, name)?, sql(&fresh, name)?);
        }

        let ingested = |id: i64| -> Result<i64> {
            Ok(conn.query_row(
                "SELECT ingested_at FROM entries WHERE id = ?1",
                [id],
                |row| row.get(0),
            )?)
        };
        assert_eq!(ingested(1)?, 1700000000);
        assert!(ingested(7)? < far_future);

        let high_water = |conn: &Connection| -> Result<i64> {
            Ok(conn.query_row("SELECT id FROM entry_id_high_water", [], |row| row.get(0))?)
        };
        assert_eq!(high_water(&conn)?, 7);
        conn.execute("DELETE FROM entries WHERE id = 7", [])?;
        assert_eq!(high_water(&conn)?, 7);
        conn.execute("INSERT INTO entries (id, published_at) VALUES (9, 0)", [])?;
        assert_eq!(high_water(&conn)?, 9);
        Ok(())
    }

    /// The `sql` of the schema object `name`.
    fn schema_sql(conn: &Connection, name: &str) -> Result<String> {
        Ok(conn.query_row(
            "SELECT sql FROM sqlite_master WHERE name = ?1",
            [name],
            |row| row.get(0),
        )?)
    }

    /// `0011_unread_entries` adds `entries.unread_visible`, backfilled from
    /// the entries' read and hidden tags, with the same indexes and triggers
    /// as a freshly-initialized database's, which then keep it in step.
    #[test]
    fn test_unread_entries_migration() -> Result<()> {
        let mut conn = rusqlite::Connection::open_in_memory()?;
        conn.execute_batch(LEGACY_SCHEMA)?;
        conn.execute_batch(
            "INSERT INTO entries (id, published_at) VALUES (1, 0), (2, 0), (3, 0), (4, 0);
             INSERT INTO tags (id, name) VALUES (1, 'news');",
        )?;
        run_pending_migrations(&mut conn)?;

        let fresh = ConnectionBuilder::default().in_memory().create().build()?;
        for name in [
            "idx_entry_unread_visible",
            "idx_entry_feed_unread_visible",
            "entry_tags_unread_visible_ai",
            "entry_tags_unread_visible_ad",
            "entry_tags_unread_visible_au",
        ] {
            assert_eq!(
                schema_sql(&conn, name)?,
                schema_sql(&fresh, name)?,
                "{name}"
            );
        }

        let tag = |name: &str| -> Result<i64> {
            Ok(
                conn.query_row("SELECT id FROM tags WHERE name = ?1", [name], |row| {
                    row.get(0)
                })?,
            )
        };
        let (news, read, hidden, saved) = (
            tag("news")?,
            tag("system:read")?,
            tag("system:hidden")?,
            tag("system:saved")?,
        );
        let unread_visible = |conn: &Connection| -> Result<Vec<i64>> {
            let mut stmt =
                conn.prepare("SELECT id FROM entries WHERE unread_visible = 1 ORDER BY id")?;
            let ids = stmt.query_map([], |row| row.get(0))?;
            Ok(ids.collect::<Result<_, _>>()?)
        };

        // Nothing was tagged, so every entry is listed; the triggers then
        // follow the tags added afterwards.
        assert_eq!(unread_visible(&conn)?, [1, 2, 3, 4]);
        conn.execute(
            "INSERT INTO entry_tags VALUES (1, ?1), (2, ?2), (3, ?3), (4, ?4)",
            [news, read, hidden, saved],
        )?;
        assert_eq!(unread_visible(&conn)?, [1, 4]);
        Ok(())
    }

    /// `0011_unread_entries` backfills `unread_visible` from tags that were
    /// already there.
    #[test]
    fn test_unread_entries_migration_backfill() -> Result<()> {
        let mut conn = rusqlite::Connection::open_in_memory()?;
        conn.execute_batch(LEGACY_SCHEMA)?;
        conn.execute_batch(
            "INSERT INTO entries (id, published_at) VALUES (1, 0), (2, 0), (3, 0);",
        )?;
        // Apply the migrations up to 0011, tag the entries as a database of
        // that version would have them, and then apply 0011 itself.
        conn.execute_batch(CREATE_MIGRATIONS_TABLE)?;
        for m in MIGRATIONS
            .iter()
            .take_while(|m| m.name != "0011_unread_entries")
        {
            conn.execute_batch(m.sql)?;
            conn.execute("INSERT INTO migrations (name) VALUES (?1)", [m.name])?;
        }
        conn.execute_batch(
            "INSERT INTO entry_tags (entry_id, tag_id)
                 SELECT 1, id FROM tags WHERE name = 'system:read';
             INSERT INTO entry_tags (entry_id, tag_id)
                 SELECT 2, id FROM tags WHERE name = 'system:hidden';",
        )?;
        run_pending_migrations(&mut conn)?;
        let flags = conn
            .prepare("SELECT unread_visible FROM entries ORDER BY id")?
            .query_map([], |row| row.get::<_, i64>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(flags, [0, 0, 1]);
        Ok(())
    }

    /// `0012_fts_unchanged_entries` gives the full-text index's update
    /// trigger the same shape as a freshly-initialized database's.
    #[test]
    fn test_fts_unchanged_entries_migration() -> Result<()> {
        let mut conn = rusqlite::Connection::open_in_memory()?;
        conn.execute_batch(LEGACY_SCHEMA)?;
        run_pending_migrations(&mut conn)?;
        let fresh = ConnectionBuilder::default().in_memory().create().build()?;
        assert_eq!(
            schema_sql(&conn, "entries_fts_au")?,
            schema_sql(&fresh, "entries_fts_au")?
        );
        Ok(())
    }

    /// However entry_tags rows are added, removed or changed, an entry's
    /// `unread_visible` says whether it has neither `system:read` nor
    /// `system:hidden`.
    #[test]
    fn test_unread_visible_tracks_entry_tags() -> Result<()> {
        let conn = ConnectionBuilder::default().in_memory().create().build()?;
        conn.execute_batch(
            "INSERT INTO feeds (id, title, url) VALUES (1, 'f', 'http://f/');
             INSERT INTO tags (name) VALUES ('news');",
        )?;
        for id in 1..=6 {
            conn.execute(
                "INSERT INTO entries (id, feed_id, syndication_format, guid, published_at, title, url)
                 VALUES (?1, 1, 'rss', ?1, 0, 't', 'u')",
                [id],
            )?;
        }
        let tag_ids: Vec<i64> = conn
            .prepare("SELECT id FROM tags WHERE name IN ('news', 'system:read', 'system:saved', 'system:hidden')")?
            .query_map([], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        assert_eq!(tag_ids.len(), 4);

        let check = |step: usize| -> Result<()> {
            let wrong: Vec<i64> = conn
                .prepare(
                    "SELECT e.id FROM entries e WHERE e.unread_visible IS NOT NOT EXISTS (
                         SELECT 1 FROM entry_tags et JOIN tags t ON t.id = et.tag_id
                         WHERE et.entry_id = e.id
                           AND t.name IN ('system:read', 'system:hidden'))",
                )?
                .query_map([], |row| row.get(0))?
                .collect::<Result<_, _>>()?;
            assert!(
                wrong.is_empty(),
                "step {step}: out of step for entries {wrong:?}"
            );
            Ok(())
        };

        // A fixed pseudo-random walk over adds, removes, retags and moves.
        let mut x: u64 = 0x2545_f491_4f6c_dd1d;
        for step in 0..2000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let entry = (x % 6 + 1) as i64;
            let other = ((x >> 8) % 6 + 1) as i64;
            let pick = |bits: u64| -> Result<i64> {
                tag_ids
                    .get((bits % tag_ids.len() as u64) as usize)
                    .copied()
                    .context("a tag")
            };
            let tag = pick(x >> 16)?;
            let other_tag = pick(x >> 24)?;
            match (x >> 32) % 4 {
                0 | 1 => {
                    conn.execute(
                        "INSERT OR IGNORE INTO entry_tags (entry_id, tag_id) VALUES (?1, ?2)",
                        [entry, tag],
                    )?;
                }
                2 => {
                    conn.execute(
                        "DELETE FROM entry_tags WHERE entry_id = ?1 AND tag_id = ?2",
                        [entry, tag],
                    )?;
                }
                _ => {
                    conn.execute(
                        "UPDATE OR IGNORE entry_tags SET entry_id = ?3, tag_id = ?4
                         WHERE entry_id = ?1 AND tag_id = ?2",
                        [entry, tag, other, other_tag],
                    )?;
                }
            }
            check(step)?;
        }

        // Deleting an entry takes its tags with it, leaving the rest alone.
        conn.execute("DELETE FROM entries WHERE id = 1", [])?;
        check(2000)?;
        Ok(())
    }
}
