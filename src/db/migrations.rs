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
/// 2. Append an entry to this array, numbering it after the last one
///    (the first is `0001_...`)
/// 3. Make the same change to `src/db/include/init.sql`, which always holds
///    the complete current schema
pub const MIGRATIONS: &[Migration] = &[
    Migration {
        name: "0001_api_tokens",
        sql: include_str!("include/migrations/0001_api_tokens.sql"),
    },
    Migration {
        name: "0002_adaptive_fetch_plugin",
        sql: include_str!("include/migrations/0002_adaptive_fetch_plugin.sql"),
    },
    Migration {
        name: "0003_entry_search_index",
        sql: include_str!("include/migrations/0003_entry_search_index.sql"),
    },
    Migration {
        name: "0004_http_cache",
        sql: include_str!("include/migrations/0004_http_cache.sql"),
    },
];

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
    pending_in(conn, MIGRATIONS)
}

/// Returns the members of `migrations` that have not yet been applied.
pub(crate) fn pending_in(
    conn: &Connection,
    migrations: &'static [Migration],
) -> Result<Vec<&'static Migration>> {
    conn.execute_batch(CREATE_MIGRATIONS_TABLE)
        .with_context(|| "failed to bootstrap migrations table")?;

    let applied = applied_migration_names(conn)?;
    let pending = migrations
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
    run_pending_in(conn, MIGRATIONS)
}

/// Applies the members of `migrations` that have not yet been applied, as
/// [`run_pending_migrations`] does, and returns how many were applied.
pub(crate) fn run_pending_in(conn: &mut Connection, migrations: &[Migration]) -> Result<usize> {
    conn.execute_batch(CREATE_MIGRATIONS_TABLE)
        .with_context(|| "failed to bootstrap migrations table")?;

    let applied = applied_migration_names(conn)?;
    let mut count: usize = 0;

    for migration in migrations {
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
pub(crate) mod tests {
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

    /// `0001_api_tokens` adds the `api_tokens` table to a database from
    /// before it.
    #[test]
    fn test_0001_api_tokens() -> Result<()> {
        let mut conn = ConnectionBuilder::default().in_memory().create().build()?;
        conn.execute_batch(
            "DROP TABLE api_tokens; DELETE FROM migrations WHERE name = '0001_api_tokens';",
        )?;
        assert_eq!(run_pending_migrations(&mut conn)?, 1);
        let (token, secret) =
            crate::db::tokens::create(&conn, "t", crate::auth::Scopes::all(), None)?;
        assert_eq!(
            crate::db::tokens::authenticate(&conn, &secret, 0)?,
            crate::db::tokens::Authentication::Valid(token)
        );
        Ok(())
    }

    /// `0002_adaptive_fetch_plugin` hands each feed's adaptive level to the
    /// adaptive-fetch plugin's store, and the feeds it was turned off for to
    /// the plugin's `exclude` setting, then drops the columns.
    #[test]
    fn test_0002_adaptive_fetch_plugin() -> Result<()> {
        let mut conn = ConnectionBuilder::default().in_memory().create().build()?;
        conn.execute_batch(
            "ALTER TABLE feeds ADD COLUMN adaptive_fetch INTEGER;
             ALTER TABLE feeds ADD COLUMN adaptive_fetch_level INTEGER NOT NULL DEFAULT 0;
             DELETE FROM migrations WHERE name = '0002_adaptive_fetch_plugin';
             INSERT INTO feeds (id, title, url, adaptive_fetch, adaptive_fetch_level) VALUES
                 (1, 'follows the server', 'http://a/', NULL, 3),
                 (2, 'turned on', 'http://b/', 1, 5),
                 (3, 'turned off', 'http://c/', 0, 2),
                 (4, 'never backed off', 'http://d/', NULL, 0),
                 (5, 'also turned off', 'http://e/', 0, 0);",
        )?;
        assert_eq!(run_pending_migrations(&mut conn)?, 1);

        let levels: Vec<(String, String)> = conn
            .prepare(
                "SELECT key, value FROM plugin_store
                 WHERE plugin = 'adaptive-fetch' ORDER BY key",
            )?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<_, _>>()?;
        assert_eq!(
            levels,
            [
                ("level:1".to_string(), "3".to_string()),
                ("level:2".to_string(), "5".to_string()),
            ]
        );
        let config = crate::db::plugins::get_config_overrides(&conn, "adaptive-fetch")?;
        assert_eq!(config["exclude"], serde_json::json!([3, 5]));

        let columns: Vec<String> = conn
            .prepare("SELECT name FROM pragma_table_info('feeds')")?
            .query_map([], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        assert!(!columns.iter().any(|c| c.starts_with("adaptive")));
        Ok(())
    }

    /// Without feeds turned off, `0002_adaptive_fetch_plugin` leaves the
    /// adaptive-fetch plugin with no config overrides.
    #[test]
    fn test_0002_adaptive_fetch_plugin_without_overrides() -> Result<()> {
        let mut conn = ConnectionBuilder::default().in_memory().create().build()?;
        conn.execute_batch(
            "ALTER TABLE feeds ADD COLUMN adaptive_fetch INTEGER;
             ALTER TABLE feeds ADD COLUMN adaptive_fetch_level INTEGER NOT NULL DEFAULT 0;
             DELETE FROM migrations WHERE name = '0002_adaptive_fetch_plugin';
             INSERT INTO feeds (title, url) VALUES ('feed', 'http://a/');",
        )?;
        assert_eq!(run_pending_migrations(&mut conn)?, 1);
        let rows: i64 = conn.query_row("SELECT count(*) FROM plugins", [], |row| row.get(0))?;
        assert_eq!(rows, 0);
        Ok(())
    }

    /// Migrations used to exercise the runner itself.
    pub(crate) const TEST_MIGRATIONS: &[Migration] = &[
        Migration {
            name: "0001_first",
            sql: "CREATE TABLE first (id INTEGER PRIMARY KEY);",
        },
        Migration {
            name: "0002_second",
            sql: "CREATE TABLE second (id INTEGER PRIMARY KEY);",
        },
    ];

    fn has_table(conn: &Connection, name: &str) -> Result<bool> {
        Ok(conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name = ?1)",
            [name],
            |row| row.get(0),
        )?)
    }

    /// Pending migrations are applied in order and recorded, and are not
    /// applied again.
    #[test]
    fn test_run_pending_applies_each_migration_once() -> Result<()> {
        let mut conn = ConnectionBuilder::default().in_memory().create().build()?;
        let names = |pending: Vec<&Migration>| pending.iter().map(|m| m.name).collect::<Vec<_>>();
        assert_eq!(
            names(pending_in(&conn, TEST_MIGRATIONS)?),
            ["0001_first", "0002_second"]
        );

        assert_eq!(run_pending_in(&mut conn, TEST_MIGRATIONS)?, 2);
        assert!(has_table(&conn, "first")?);
        assert!(has_table(&conn, "second")?);
        assert!(pending_in(&conn, TEST_MIGRATIONS)?.is_empty());
        let applied = applied_migration_names(&conn)?;
        assert!(applied.contains("0001_first") && applied.contains("0002_second"));

        assert_eq!(run_pending_in(&mut conn, TEST_MIGRATIONS)?, 0);
        Ok(())
    }

    /// A migration that fails is rolled back and left pending, while the
    /// ones before it stay applied.
    #[test]
    fn test_failed_migration_is_rolled_back() -> Result<()> {
        const FAILING: &[Migration] = &[
            Migration {
                name: "0001_first",
                sql: "CREATE TABLE first (id INTEGER PRIMARY KEY);",
            },
            Migration {
                name: "0002_broken",
                sql: "CREATE TABLE broken (id INTEGER); NOT VALID SQL;",
            },
        ];
        let mut conn = ConnectionBuilder::default().in_memory().create().build()?;
        assert!(run_pending_in(&mut conn, FAILING).is_err());

        assert!(has_table(&conn, "first")?);
        assert!(!has_table(&conn, "broken")?);
        let pending: Vec<_> = pending_in(&conn, FAILING)?.iter().map(|m| m.name).collect();
        assert_eq!(pending, ["0002_broken"]);
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

    /// `0003_entry_search_index` adds the index that full-text searches
    /// name with INDEXED BY, without which they would fail.
    #[test]
    fn test_0003_entry_search_index() -> Result<()> {
        let mut conn = ConnectionBuilder::default().in_memory().create().build()?;
        conn.execute_batch(
            "DROP INDEX idx_entry_search;
             DELETE FROM migrations WHERE name = '0003_entry_search_index';",
        )?;
        assert_eq!(run_pending_migrations(&mut conn)?, 1);
        conn.query_row(
            "SELECT COUNT(*) FROM entries e INDEXED BY idx_entry_search
             JOIN entries_fts ON e.id = entries_fts.rowid
             WHERE entries_fts MATCH 'news'",
            [],
            |row| row.get::<_, i64>(0),
        )?;
        Ok(())
    }

    /// `0004_http_cache` moves the cache state of feeds and assets into
    /// `http_cache`, keyed by URL, and drops the old columns.
    #[test]
    fn test_0004_http_cache() -> Result<()> {
        let mut conn = ConnectionBuilder::default().in_memory().create().build()?;
        conn.execute_batch(
            "DROP TRIGGER http_cache_feed_ad;
             DROP TRIGGER http_cache_feed_au;
             DROP TRIGGER http_cache_asset_ad;
             DROP TABLE http_cache;
             ALTER TABLE feeds ADD COLUMN header_etag VARCHAR;
             ALTER TABLE feeds ADD COLUMN header_last_modified VARCHAR;
             ALTER TABLE feeds ADD COLUMN header_body_hash VARCHAR;
             ALTER TABLE feeds ADD COLUMN last_full_refresh_at INTEGER;
             ALTER TABLE feeds ADD COLUMN header_expires INTEGER;
             ALTER TABLE feeds ADD COLUMN header_immutable_until INTEGER;
             ALTER TABLE feed_assets ADD COLUMN etag TEXT;
             ALTER TABLE feed_assets ADD COLUMN last_modified TEXT;
             DELETE FROM migrations WHERE name = '0004_http_cache';
             INSERT INTO feeds (id, title, url, header_etag, header_last_modified,
                                header_body_hash, last_full_refresh_at, header_expires,
                                header_immutable_until) VALUES
                 (1, 'cached', 'http://a/', '\"a\"', 'Mon', 'hash', 10, 20, 30),
                 (2, 'never fetched', 'http://b/', NULL, NULL, NULL, NULL, NULL, NULL);
             INSERT INTO feed_assets (blake3, original_url, size_bytes, etag, last_modified)
             VALUES
                 ('h1', 'http://x/a.png', 1, '\"x1\"', NULL),
                 ('h2', 'http://x/a.png', 1, '\"x2\"', NULL),
                 ('h3', 'http://x/b.png', 1, NULL, NULL);",
        )?;
        assert_eq!(run_pending_migrations(&mut conn)?, 1);

        use crate::db::http_cache::{lookup, CacheEntry};
        assert_eq!(
            lookup(&conn, "http://a/")?,
            Some(CacheEntry {
                etag: Some("\"a\"".into()),
                last_modified: Some("Mon".into()),
                expires_at: Some(20),
                immutable_until: Some(30),
                body_hash: Some("hash".into()),
                last_full_refresh_at: Some(10),
            })
        );
        assert_eq!(lookup(&conn, "http://b/")?, None);
        assert_eq!(
            lookup(&conn, "http://x/a.png")?.and_then(|e| e.etag),
            Some("\"x1\"".into())
        );
        assert_eq!(lookup(&conn, "http://x/b.png")?, None);

        for table in ["feeds", "feed_assets"] {
            let columns: Vec<String> = conn
                .prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))?
                .query_map([], |row| row.get(0))?
                .collect::<Result<_, _>>()?;
            assert!(
                !columns.iter().any(|c| c.starts_with("header_")
                    || c == "etag"
                    || c == "last_modified"
                    || c == "last_full_refresh_at"),
                "{table} still has cache columns: {columns:?}"
            );
        }

        // The triggers came with it.
        conn.execute("DELETE FROM feeds WHERE id = 1", [])?;
        assert_eq!(lookup(&conn, "http://a/")?, None);
        Ok(())
    }
}
