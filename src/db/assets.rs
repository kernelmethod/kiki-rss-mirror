//! Helpers for the feed asset cache.
//!
//! Cached asset bytes live on the filesystem; this module owns the SQLite
//! index that maps `blake3` hashes and original URLs to those files, plus
//! the two settings that govern the cache (enabled flag and size cap).
use anyhow::{Context, Result};
use rusqlite::{params, Connection};

const ENABLED_KEY: &str = "feed_asset_cache_enabled";
const MAX_BYTES_KEY: &str = "feed_asset_cache_max_bytes";

/// One row from the `feed_assets` table.
#[derive(Debug, Clone)]
pub struct AssetRow {
    pub id: i64,
    pub blake3: String,
    pub original_url: String,
    pub content_type: Option<String>,
    pub size_bytes: i64,
}

/// One row from the `entry_assets` join, enriched with the linked asset.
#[derive(Debug, Clone)]
pub struct EntryAssetRow {
    pub asset: AssetRow,
    pub kind: String,
}

/// Returns whether the asset cache is enabled. Defaults to `true` if the
/// setting row is missing (schema seeds it on init).
pub fn get_cache_enabled(conn: &Connection) -> Result<bool> {
    let value: Option<String> = conn
        .query_row(
            "SELECT value FROM settings WHERE key = ?1",
            [ENABLED_KEY],
            |row| row.get(0),
        )
        .ok();
    Ok(value.as_deref().map(|v| v == "true").unwrap_or(true))
}

/// Enable or disable asset caching.
pub fn set_cache_enabled(conn: &Connection, enabled: bool) -> Result<()> {
    let value = if enabled { "true" } else { "false" };
    conn.execute(
        "INSERT INTO settings (key, value, type) VALUES (?1, ?2, 'boolean')
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![ENABLED_KEY, value],
    )
    .with_context(|| "failed to upsert feed_asset_cache_enabled")?;
    Ok(())
}

/// Returns the configured cache size cap in bytes.
pub fn get_cache_max_bytes(conn: &Connection) -> Result<i64> {
    let value: String = conn
        .query_row(
            "SELECT value FROM settings WHERE key = ?1",
            [MAX_BYTES_KEY],
            |row| row.get(0),
        )
        .with_context(|| "failed to read feed_asset_cache_max_bytes setting")?;
    value
        .parse::<i64>()
        .with_context(|| format!("invalid feed_asset_cache_max_bytes value: {}", value))
}

/// Set the cache size cap in bytes.
pub fn set_cache_max_bytes(conn: &Connection, bytes: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO settings (key, value, type) VALUES (?1, ?2, 'integer')
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![MAX_BYTES_KEY, bytes.to_string()],
    )
    .with_context(|| "failed to upsert feed_asset_cache_max_bytes")?;
    Ok(())
}

/// Sum of `size_bytes` over all cached assets.
pub fn total_cache_size(conn: &Connection) -> Result<i64> {
    conn.query_row(
        "SELECT COALESCE(SUM(size_bytes), 0) FROM feed_assets",
        [],
        |row| row.get::<_, i64>(0),
    )
    .with_context(|| "failed to compute total asset cache size")
}

/// Look up an asset by its blake3 hex hash.
pub fn lookup_by_hash(conn: &Connection, blake3: &str) -> Result<Option<AssetRow>> {
    conn.query_row(
        "SELECT id, blake3, original_url, content_type, size_bytes
         FROM feed_assets WHERE blake3 = ?1",
        [blake3],
        row_to_asset,
    )
    .map(Some)
    .or_else(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => Ok(None),
        other => Err(other.into()),
    })
}

/// Look up an asset by its original source URL.
pub fn lookup_by_url(conn: &Connection, url: &str) -> Result<Option<AssetRow>> {
    conn.query_row(
        "SELECT id, blake3, original_url, content_type, size_bytes
         FROM feed_assets WHERE original_url = ?1 LIMIT 1",
        [url],
        row_to_asset,
    )
    .map(Some)
    .or_else(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => Ok(None),
        other => Err(other.into()),
    })
}

fn row_to_asset(row: &rusqlite::Row) -> rusqlite::Result<AssetRow> {
    Ok(AssetRow {
        id: row.get(0)?,
        blake3: row.get(1)?,
        original_url: row.get(2)?,
        content_type: row.get(3)?,
        size_bytes: row.get(4)?,
    })
}

/// Insert a new `feed_assets` row and return its rowid.
pub fn insert_asset(
    conn: &Connection,
    blake3: &str,
    original_url: &str,
    content_type: Option<&str>,
    size_bytes: i64,
    etag: Option<&str>,
    last_modified: Option<&str>,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO feed_assets
            (blake3, original_url, content_type, size_bytes, etag, last_modified)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            blake3,
            original_url,
            content_type,
            size_bytes,
            etag,
            last_modified
        ],
    )
    .with_context(|| format!("failed to insert feed_assets row for {}", blake3))?;
    Ok(conn.last_insert_rowid())
}

/// Associate an asset with an entry. Idempotent: duplicate (entry, asset)
/// pairs are ignored.
pub fn link_entry_asset(conn: &Connection, entry_id: i64, asset_id: i64, kind: &str) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO entry_assets (entry_id, asset_id, kind)
         VALUES (?1, ?2, ?3)",
        params![entry_id, asset_id, kind],
    )
    .with_context(|| {
        format!(
            "failed to link entry {} to asset {} ({})",
            entry_id, asset_id, kind
        )
    })?;
    Ok(())
}

/// Bump `last_accessed_at` to the current time for the asset with the given
/// blake3 hash. No-op if the hash is unknown.
pub fn touch(conn: &Connection, blake3: &str) -> Result<()> {
    conn.execute(
        "UPDATE feed_assets SET last_accessed_at = unixepoch() WHERE blake3 = ?1",
        [blake3],
    )
    .with_context(|| format!("failed to touch asset {}", blake3))?;
    Ok(())
}

/// List the assets associated with an entry, joined with their `feed_assets`
/// rows. Stable ordering by the asset row id (insertion order).
pub fn list_entry_assets(conn: &Connection, entry_id: i64) -> Result<Vec<EntryAssetRow>> {
    let mut stmt = conn.prepare(
        "SELECT fa.id, fa.blake3, fa.original_url, fa.content_type, fa.size_bytes, ea.kind
         FROM entry_assets ea
         JOIN feed_assets fa ON fa.id = ea.asset_id
         WHERE ea.entry_id = ?1
         ORDER BY fa.id ASC",
    )?;
    let rows = stmt.query_map([entry_id], |row| {
        Ok(EntryAssetRow {
            asset: AssetRow {
                id: row.get(0)?,
                blake3: row.get(1)?,
                original_url: row.get(2)?,
                content_type: row.get(3)?,
                size_bytes: row.get(4)?,
            },
            kind: row.get(5)?,
        })
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// Evict least-recently-accessed assets until the total cache size is at or
/// below `target_bytes`. Returns the blake3 hashes of the deleted rows; the
/// caller is responsible for unlinking the corresponding files from disk.
pub fn evict_to(conn: &mut Connection, target_bytes: i64) -> Result<Vec<String>> {
    let mut deleted: Vec<String> = Vec::new();
    let tx = conn.transaction()?;
    let mut total: i64 = tx
        .query_row(
            "SELECT COALESCE(SUM(size_bytes), 0) FROM feed_assets",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);

    if total <= target_bytes {
        tx.commit()?;
        return Ok(deleted);
    }

    // Pull oldest-accessed rows and drop them one at a time until under cap.
    let mut stmt = tx.prepare(
        "SELECT id, blake3, size_bytes FROM feed_assets
         ORDER BY last_accessed_at ASC, id ASC",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);

    for (id, blake3, size) in rows {
        if total <= target_bytes {
            break;
        }
        tx.execute("DELETE FROM feed_assets WHERE id = ?1", [id])?;
        total -= size;
        deleted.push(blake3);
    }

    tx.commit()?;
    Ok(deleted)
}

/// Delete a single asset row by blake3 hash. Returns the row if it existed.
pub fn delete_by_hash(conn: &Connection, blake3: &str) -> Result<Option<AssetRow>> {
    let row = lookup_by_hash(conn, blake3)?;
    if row.is_some() {
        conn.execute("DELETE FROM feed_assets WHERE blake3 = ?1", [blake3])?;
    }
    Ok(row)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::db::ConnectionBuilder;

    fn mem_conn() -> Connection {
        ConnectionBuilder::default()
            .in_memory()
            .create()
            .build()
            .expect("build in-memory conn")
    }

    #[test]
    fn default_enabled_is_true() {
        let conn = mem_conn();
        assert!(get_cache_enabled(&conn).unwrap());
    }

    #[test]
    fn set_and_get_enabled() {
        let conn = mem_conn();
        set_cache_enabled(&conn, false).unwrap();
        assert!(!get_cache_enabled(&conn).unwrap());
        set_cache_enabled(&conn, true).unwrap();
        assert!(get_cache_enabled(&conn).unwrap());
    }

    #[test]
    fn default_max_bytes_seeded() {
        let conn = mem_conn();
        let n = get_cache_max_bytes(&conn).unwrap();
        assert!(n > 0);
    }

    #[test]
    fn set_and_get_max_bytes() {
        let conn = mem_conn();
        set_cache_max_bytes(&conn, 42).unwrap();
        assert_eq!(get_cache_max_bytes(&conn).unwrap(), 42);
    }

    #[test]
    fn insert_lookup_touch_delete() {
        let conn = mem_conn();
        let id = insert_asset(
            &conn,
            "abc",
            "http://x/y.png",
            Some("image/png"),
            10,
            None,
            None,
        )
        .unwrap();
        let row = lookup_by_hash(&conn, "abc").unwrap().unwrap();
        assert_eq!(row.id, id);
        assert_eq!(row.original_url, "http://x/y.png");
        let by_url = lookup_by_url(&conn, "http://x/y.png").unwrap().unwrap();
        assert_eq!(by_url.id, id);
        touch(&conn, "abc").unwrap();
        let deleted = delete_by_hash(&conn, "abc").unwrap().unwrap();
        assert_eq!(deleted.id, id);
        assert!(lookup_by_hash(&conn, "abc").unwrap().is_none());
    }

    #[test]
    fn total_cache_size_sums() {
        let conn = mem_conn();
        insert_asset(&conn, "a", "http://x/a", None, 100, None, None).unwrap();
        insert_asset(&conn, "b", "http://x/b", None, 250, None, None).unwrap();
        assert_eq!(total_cache_size(&conn).unwrap(), 350);
    }

    #[test]
    fn evict_to_drops_oldest_first() {
        let mut conn = mem_conn();
        // Insert three rows with explicit last_accessed_at values so the
        // order is deterministic.
        insert_asset(&conn, "a", "http://x/a", None, 100, None, None).unwrap();
        insert_asset(&conn, "b", "http://x/b", None, 200, None, None).unwrap();
        insert_asset(&conn, "c", "http://x/c", None, 300, None, None).unwrap();
        conn.execute(
            "UPDATE feed_assets SET last_accessed_at = 100 WHERE blake3 = 'a'",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE feed_assets SET last_accessed_at = 200 WHERE blake3 = 'b'",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE feed_assets SET last_accessed_at = 300 WHERE blake3 = 'c'",
            [],
        )
        .unwrap();

        // Target 350 bytes: start at 600, drop 'a' (100, total 500 > 350),
        // drop 'b' (200, total 300 <= 350), stop. 'c' survives.
        let deleted = evict_to(&mut conn, 350).unwrap();
        assert_eq!(deleted, vec!["a", "b"]);
        assert_eq!(total_cache_size(&conn).unwrap(), 300);
    }

    #[test]
    fn evict_to_noop_when_under_cap() {
        let mut conn = mem_conn();
        insert_asset(&conn, "a", "http://x/a", None, 10, None, None).unwrap();
        let deleted = evict_to(&mut conn, 100).unwrap();
        assert!(deleted.is_empty());
    }

    #[test]
    fn list_entry_assets_joins_kind() {
        let conn = mem_conn();
        // Seed a minimal feed + entry so FK constraints pass.
        conn.execute(
            "INSERT INTO feeds (title, url, syndication_format) VALUES (?, ?, ?)",
            ["F", "http://example.com", "rss"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO entries (feed_id, syndication_format, guid, published_at, title, url)
             VALUES (?, ?, ?, ?, ?, ?)",
            rusqlite::params![1i64, "rss", "g", 1i64, "t", "http://example.com/e"],
        )
        .unwrap();
        let a = insert_asset(
            &conn,
            "a",
            "http://x/a.png",
            Some("image/png"),
            10,
            None,
            None,
        )
        .unwrap();
        let b = insert_asset(
            &conn,
            "b",
            "http://x/b.mp3",
            Some("audio/mpeg"),
            20,
            None,
            None,
        )
        .unwrap();
        link_entry_asset(&conn, 1, a, "inline_img").unwrap();
        link_entry_asset(&conn, 1, b, "enclosure").unwrap();
        // Duplicate link is a no-op.
        link_entry_asset(&conn, 1, a, "inline_img").unwrap();

        let rows = list_entry_assets(&conn, 1).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].asset.blake3, "a");
        assert_eq!(rows[0].kind, "inline_img");
        assert_eq!(rows[1].asset.blake3, "b");
        assert_eq!(rows[1].kind, "enclosure");
    }
}
