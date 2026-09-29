//! The favicons of the websites feeds belong to.
//!
//! A favicon is a cached asset like any other (see [`crate::db::assets`]);
//! the `feed_favicons` table records which asset, if any, is each feed's
//! favicon and when Kiki last looked for one. Finding and downloading
//! favicons is done by [`crate::tasks::favicons`].
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

/// How long a favicon that was found is kept before Kiki looks again, in
/// case the site has changed it.
pub const RECHECK_FOUND_SECS: i64 = 7 * 24 * 60 * 60;

/// How long Kiki waits before looking again for the favicon of a site that
/// did not have one, or could not be reached.
pub const RECHECK_MISSING_SECS: i64 = 24 * 60 * 60;

/// The outcome of the last look for a feed's favicon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FaviconCheck {
    /// Unix timestamp of the look.
    pub checked_at: i64,
    /// The `feed_assets` row holding the favicon, or `None` if none was
    /// found.
    pub asset_id: Option<i64>,
}

impl FaviconCheck {
    /// Whether it is time to look for the favicon again at Unix time `now`.
    ///
    /// # Examples
    ///
    /// ```
    /// use kiki_rss::db::favicons::{FaviconCheck, RECHECK_MISSING_SECS};
    ///
    /// let check = FaviconCheck { checked_at: 1_000, asset_id: None };
    /// assert!(!check.is_due(1_000 + RECHECK_MISSING_SECS - 1));
    /// assert!(check.is_due(1_000 + RECHECK_MISSING_SECS));
    /// ```
    pub fn is_due(&self, now: i64) -> bool {
        let wait = match self.asset_id {
            Some(_) => RECHECK_FOUND_SECS,
            None => RECHECK_MISSING_SECS,
        };
        now.saturating_sub(self.checked_at) >= wait
    }
}

/// An SQL expression for the blake3 hash of the favicon of the feed whose
/// id is the SQL expression `feed_id`, or NULL if it has none.
///
/// # Examples
///
/// ```
/// use kiki_rss::db::favicons::favicon_hash_sql;
///
/// let sql = format!("SELECT {} FROM entries e", favicon_hash_sql("e.feed_id"));
/// assert!(sql.contains("feed_favicons"));
/// ```
pub fn favicon_hash_sql(feed_id: &str) -> String {
    format!(
        "(SELECT fa.blake3 FROM feed_favicons ff
          JOIN feed_assets fa ON fa.id = ff.asset_id
          WHERE ff.feed_id = {feed_id})"
    )
}

/// Record the website feed `feed_id` belongs to. `site_url` should already
/// have been checked to be an absolute `http(s)` URL.
pub fn set_site_url(conn: &Connection, feed_id: i64, site_url: Option<&str>) -> Result<()> {
    conn.execute(
        "UPDATE feeds SET site_url = ?2 WHERE id = ?1 AND site_url IS NOT ?2",
        params![feed_id, site_url],
    )
    .with_context(|| format!("failed to set site_url of feed {}", feed_id))?;
    Ok(())
}

/// The outcome of the last look for feed `feed_id`'s favicon, or `None` if
/// Kiki has not looked yet (or the favicon it found has been evicted).
pub fn last_check(conn: &Connection, feed_id: i64) -> Result<Option<FaviconCheck>> {
    conn.query_row(
        "SELECT checked_at, asset_id FROM feed_favicons WHERE feed_id = ?1",
        [feed_id],
        |row| {
            Ok(FaviconCheck {
                checked_at: row.get(0)?,
                asset_id: row.get(1)?,
            })
        },
    )
    .optional()
    .with_context(|| format!("failed to read favicon of feed {}", feed_id))
}

/// Record that Kiki has just looked for feed `feed_id`'s favicon, and found
/// the asset `asset_id`, or none.
pub fn record_check(conn: &Connection, feed_id: i64, asset_id: Option<i64>) -> Result<()> {
    conn.execute(
        "INSERT INTO feed_favicons (feed_id, asset_id, checked_at)
         VALUES (?1, ?2, unixepoch())
         ON CONFLICT(feed_id) DO UPDATE SET
            asset_id = excluded.asset_id,
            checked_at = excluded.checked_at",
        params![feed_id, asset_id],
    )
    .with_context(|| format!("failed to record favicon of feed {}", feed_id))?;
    Ok(())
}

/// The blake3 hash of feed `feed_id`'s favicon, or `None` if it has none.
pub fn favicon_hash(conn: &Connection, feed_id: i64) -> Result<Option<String>> {
    conn.query_row(
        &format!("SELECT {}", favicon_hash_sql("?1")),
        [feed_id],
        |row| row.get(0),
    )
    .with_context(|| format!("failed to read favicon of feed {}", feed_id))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::db::assets::{evict_to, insert_asset};
    use crate::db::ConnectionBuilder;

    fn conn_with_feed() -> Connection {
        let conn = ConnectionBuilder::default()
            .in_memory()
            .create()
            .build()
            .expect("build in-memory conn");
        conn.execute(
            "INSERT INTO feeds (title, url) VALUES ('F', 'http://example.com/feed')",
            [],
        )
        .unwrap();
        conn
    }

    #[test]
    fn record_and_read_back() {
        let conn = conn_with_feed();
        assert_eq!(last_check(&conn, 1).unwrap(), None);
        assert_eq!(favicon_hash(&conn, 1).unwrap(), None);

        record_check(&conn, 1, None).unwrap();
        let check = last_check(&conn, 1).unwrap().unwrap();
        assert_eq!(check.asset_id, None);
        assert_eq!(favicon_hash(&conn, 1).unwrap(), None);

        let asset = insert_asset(&conn, "abc", "http://x/i.ico", None, 1, None, None).unwrap();
        record_check(&conn, 1, Some(asset)).unwrap();
        assert_eq!(last_check(&conn, 1).unwrap().unwrap().asset_id, Some(asset));
        assert_eq!(favicon_hash(&conn, 1).unwrap().as_deref(), Some("abc"));
    }

    #[test]
    fn evicting_the_asset_forgets_the_favicon() {
        let mut conn = conn_with_feed();
        let asset = insert_asset(&conn, "abc", "http://x/i.ico", None, 10, None, None).unwrap();
        record_check(&conn, 1, Some(asset)).unwrap();

        evict_to(&mut conn, 0).unwrap();
        assert_eq!(last_check(&conn, 1).unwrap(), None);
    }

    #[test]
    fn deleting_the_feed_forgets_the_favicon() {
        let conn = conn_with_feed();
        record_check(&conn, 1, None).unwrap();
        conn.execute("DELETE FROM feeds WHERE id = 1", []).unwrap();
        assert_eq!(last_check(&conn, 1).unwrap(), None);
    }

    #[test]
    fn found_favicons_are_rechecked_less_often() {
        let found = FaviconCheck {
            checked_at: 0,
            asset_id: Some(1),
        };
        let missing = FaviconCheck {
            checked_at: 0,
            asset_id: None,
        };
        assert!(missing.is_due(RECHECK_MISSING_SECS));
        assert!(!found.is_due(RECHECK_MISSING_SECS));
        assert!(found.is_due(RECHECK_FOUND_SECS));
    }

    #[test]
    fn set_site_url_round_trips() {
        let conn = conn_with_feed();
        set_site_url(&conn, 1, Some("https://example.com/")).unwrap();
        let url: Option<String> = conn
            .query_row("SELECT site_url FROM feeds WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(url.as_deref(), Some("https://example.com/"));
        set_site_url(&conn, 1, None).unwrap();
        let url: Option<String> = conn
            .query_row("SELECT site_url FROM feeds WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(url, None);
    }
}
