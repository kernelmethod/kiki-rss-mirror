//! The HTTP cache state Kiki keeps for each URL it fetches.
//!
//! The `http_cache` table holds, per URL, the validators to revalidate it
//! with, how long its last response stays fresh, and a fingerprint of its
//! last body. Feeds and cached assets share it: a feed's row is found by
//! `feeds.url`, an asset's by `feed_assets.original_url`. Triggers remove a
//! row once no feed or asset has its URL. Deciding what to store from a
//! response's headers is done by `crate::tasks::cache`.
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

/// The cache state stored for one URL.
///
/// Every field is optional: a URL with no row reads as
/// [`CacheEntry::default`], i.e. nothing cached.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CacheEntry {
    /// `ETag` of the last response, sent as `If-None-Match`.
    pub etag: Option<String>,
    /// `Last-Modified` of the last response, sent as `If-Modified-Since`.
    pub last_modified: Option<String>,
    /// Unix timestamp the last response is fresh until: `max-age`
    /// (age-corrected) or else `Expires`.
    pub expires_at: Option<i64>,
    /// End of an RFC 8246 `immutable` window, as a Unix timestamp.
    pub immutable_until: Option<i64>,
    /// Blake3 hex digest of the body of the last `200 OK`.
    pub body_hash: Option<String>,
    /// Unix timestamp of the last `200 OK`.
    pub last_full_refresh_at: Option<i64>,
}

/// Look up the cache state stored for `url`.
///
/// Returns `None` if nothing is stored for it.
///
/// # Errors
///
/// Returns an error if the query fails.
pub fn lookup(conn: &Connection, url: &str) -> Result<Option<CacheEntry>> {
    conn.query_row(
        "SELECT etag, last_modified, expires_at, immutable_until, body_hash,
                last_full_refresh_at
         FROM http_cache WHERE url = ?1",
        [url],
        |row| {
            Ok(CacheEntry {
                etag: row.get(0)?,
                last_modified: row.get(1)?,
                expires_at: row.get(2)?,
                immutable_until: row.get(3)?,
                body_hash: row.get(4)?,
                last_full_refresh_at: row.get(5)?,
            })
        },
    )
    .optional()
    .with_context(|| format!("failed to look up the cache state of {}", url))
}

/// Store `entry` as the cache state of `url`, replacing whatever was
/// stored for it.
///
/// Nothing is stored unless a feed or an asset has `url`, since the row
/// would otherwise never be removed; that happens when a feed moves or is
/// deleted while it is being fetched. Returns whether the row was stored.
///
/// # Errors
///
/// Returns an error if the write fails.
///
/// # Examples
///
/// ```ignore
/// let entry = CacheEntry {
///     etag: Some("\"v1\"".into()),
///     ..CacheEntry::default()
/// };
/// http_cache::store(&conn, "https://example.com/feed.xml", &entry)?;
/// ```
pub fn store(conn: &Connection, url: &str, entry: &CacheEntry) -> Result<bool> {
    let stored = conn
        .execute(
            "INSERT INTO http_cache
                (url, etag, last_modified, expires_at, immutable_until, body_hash,
                 last_full_refresh_at)
             SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7
             WHERE EXISTS (SELECT 1 FROM feeds WHERE url = ?1)
                OR EXISTS (SELECT 1 FROM feed_assets WHERE original_url = ?1)
             ON CONFLICT (url) DO UPDATE SET
                etag = excluded.etag,
                last_modified = excluded.last_modified,
                expires_at = excluded.expires_at,
                immutable_until = excluded.immutable_until,
                body_hash = excluded.body_hash,
                last_full_refresh_at = excluded.last_full_refresh_at",
            params![
                url,
                entry.etag,
                entry.last_modified,
                entry.expires_at,
                entry.immutable_until,
                entry.body_hash,
                entry.last_full_refresh_at,
            ],
        )
        .with_context(|| format!("failed to store the cache state of {}", url))?;
    Ok(stored > 0)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
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

    fn entry(etag: &str) -> CacheEntry {
        CacheEntry {
            etag: Some(etag.to_string()),
            expires_at: Some(100),
            ..CacheEntry::default()
        }
    }

    fn add_feed(conn: &Connection, url: &str) -> i64 {
        conn.execute("INSERT INTO feeds (title, url) VALUES ('F', ?1)", [url])
            .unwrap();
        conn.last_insert_rowid()
    }

    #[test]
    fn store_and_lookup_round_trip() {
        let conn = mem_conn();
        add_feed(&conn, "http://a/");
        assert_eq!(lookup(&conn, "http://a/").unwrap(), None);

        assert!(store(&conn, "http://a/", &entry("\"1\"")).unwrap());
        assert_eq!(lookup(&conn, "http://a/").unwrap(), Some(entry("\"1\"")));

        // A second store replaces every field.
        let replaced = CacheEntry {
            body_hash: Some("abc".into()),
            ..CacheEntry::default()
        };
        assert!(store(&conn, "http://a/", &replaced).unwrap());
        assert_eq!(lookup(&conn, "http://a/").unwrap(), Some(replaced));
    }

    #[test]
    fn store_skips_unknown_urls() {
        let conn = mem_conn();
        assert!(!store(&conn, "http://nobody/", &entry("\"1\"")).unwrap());
        assert_eq!(lookup(&conn, "http://nobody/").unwrap(), None);
    }

    #[test]
    fn deleting_or_moving_a_feed_drops_its_row() {
        let conn = mem_conn();
        let a = add_feed(&conn, "http://a/");
        let b = add_feed(&conn, "http://b/");
        store(&conn, "http://a/", &entry("\"a\"")).unwrap();
        store(&conn, "http://b/", &entry("\"b\"")).unwrap();

        conn.execute("DELETE FROM feeds WHERE id = ?1", [a])
            .unwrap();
        assert_eq!(lookup(&conn, "http://a/").unwrap(), None);

        // Other updates leave the row alone; a new URL drops it.
        conn.execute("UPDATE feeds SET title = 'G' WHERE id = ?1", [b])
            .unwrap();
        assert!(lookup(&conn, "http://b/").unwrap().is_some());
        conn.execute("UPDATE feeds SET url = 'http://c/' WHERE id = ?1", [b])
            .unwrap();
        assert_eq!(lookup(&conn, "http://b/").unwrap(), None);
    }

    #[test]
    fn evicting_an_asset_drops_its_row_once_unused() {
        let conn = mem_conn();
        let insert = |hash: &str| {
            crate::db::assets::insert_asset(&conn, hash, "http://x/a.png", None, 1).unwrap()
        };
        insert("h1");
        insert("h2");
        store(&conn, "http://x/a.png", &entry("\"x\"")).unwrap();

        // Another asset still has the URL.
        crate::db::assets::delete_by_hash(&conn, "h1").unwrap();
        assert!(lookup(&conn, "http://x/a.png").unwrap().is_some());

        crate::db::assets::delete_by_hash(&conn, "h2").unwrap();
        assert_eq!(lookup(&conn, "http://x/a.png").unwrap(), None);
    }

    #[test]
    fn a_feed_keeps_a_row_its_asset_namesake_loses() {
        let conn = mem_conn();
        add_feed(&conn, "http://x/feed");
        crate::db::assets::insert_asset(&conn, "h", "http://x/feed", None, 1).unwrap();
        store(&conn, "http://x/feed", &entry("\"x\"")).unwrap();
        crate::db::assets::delete_by_hash(&conn, "h").unwrap();
        assert!(lookup(&conn, "http://x/feed").unwrap().is_some());
    }
}
