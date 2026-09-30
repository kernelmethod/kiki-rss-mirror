/// Merging duplicate feeds.
///
/// Feed URLs are unique, but two feeds can still turn out to be the same
/// feed: one subscribed at a URL that permanently redirects to the URL of
/// another (say, the same feed listed under an older URL in an OPML file).
/// Once a fetch finds that out, [`merge_feed_into`] folds the redirected
/// feed into the one that has the URL already.
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

/// The feed that was merged away by [`merge_feed_into`].
#[derive(Debug, PartialEq, Eq)]
pub struct MergedFeed {
    /// The id of the feed the duplicate was merged into.
    pub into: i64,
    /// The duplicate's URL before it was deleted.
    pub url: String,
    /// The duplicate's title before it was deleted.
    pub title: String,
}

/// Merge feed `feed_id` into the feed whose URL is `url`, if there is one
/// other than `feed_id` itself, and delete it.
///
/// The surviving feed gains the duplicate's tags. The duplicate's entries
/// move to it, except those it has already (by guid), whose tags, such as
/// `system:read`, are copied onto the surviving feed's copy instead. The
/// duplicate's own settings, such as its credentials and fetch interval,
/// are dropped.
///
/// ```
/// use kiki_rss::db::{feeds::merge_feed_into, ConnectionBuilder};
///
/// let conn = ConnectionBuilder::default().in_memory().create().build()?;
/// conn.execute_batch(
///     "INSERT INTO feeds (id, title, url) VALUES
///         (1, 'new', 'https://example.com/feed'),
///         (2, 'old', 'https://example.com/feed/');",
/// )?;
/// let merged = merge_feed_into(&conn, 2, "https://example.com/feed")?;
/// assert_eq!(merged.map(|m| m.into), Some(1));
/// # Ok::<(), anyhow::Error>(())
/// ```
///
/// # Errors
///
/// Returns an error if a query fails, in which case nothing is changed.
pub fn merge_feed_into(conn: &Connection, feed_id: i64, url: &str) -> Result<Option<MergedFeed>> {
    // Take the write lock up front, so that a refresh of the surviving feed
    // can't store entries between the lookups and the moves below.
    let tx = rusqlite::Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;

    let into: Option<i64> = tx
        .query_row(
            "SELECT id FROM feeds WHERE url = ?1 AND id != ?2",
            rusqlite::params![url, feed_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(into) = into else {
        return Ok(None);
    };
    let (dup_url, title): (Option<String>, String) = tx.query_row(
        "SELECT url, title FROM feeds WHERE id = ?1",
        [feed_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;

    let params = rusqlite::named_params! { ":dup": feed_id, ":into": into };
    tx.execute(
        "INSERT OR IGNORE INTO feed_tags (feed_id, tag_id)
         SELECT :into, tag_id FROM feed_tags WHERE feed_id = :dup",
        params,
    )?;
    tx.execute(
        "INSERT OR IGNORE INTO entry_tags (entry_id, tag_id)
         SELECT kept.id, et.tag_id
         FROM entries dup
         JOIN entries kept ON kept.feed_id = :into AND kept.guid = dup.guid
         JOIN entry_tags et ON et.entry_id = dup.id
         WHERE dup.feed_id = :dup",
        params,
    )?;
    tx.execute(
        "DELETE FROM entries WHERE feed_id = :dup
           AND guid IN (SELECT guid FROM entries WHERE feed_id = :into)",
        params,
    )?;
    tx.execute(
        "UPDATE entries SET feed_id = :into WHERE feed_id = :dup",
        params,
    )?;
    tx.execute("DELETE FROM feeds WHERE id = ?1", [feed_id])?;
    tx.commit()
        .with_context(|| format!("failed to merge feed {feed_id} into feed {into}"))?;

    Ok(Some(MergedFeed {
        into,
        url: dup_url.unwrap_or_default(),
        title,
    }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::db::ConnectionBuilder;

    fn setup() -> Connection {
        let conn = ConnectionBuilder::default()
            .in_memory()
            .create()
            .build()
            .unwrap();
        conn.execute_batch(
            "INSERT INTO feeds (id, title, url) VALUES
                (1, 'kept', 'https://example.com/feed'),
                (2, 'dup', 'https://example.com/feed/');
             INSERT INTO tags (id, name) VALUES (100, 'news'), (101, 'papers');
             INSERT INTO feed_tags (feed_id, tag_id) VALUES (1, 100), (2, 100), (2, 101);
             INSERT INTO entries (id, feed_id, syndication_format, guid, published_at, title, url)
             VALUES
                (10, 1, 'rss', 'both', 0, 'both', 'http://x/both'),
                (20, 2, 'rss', 'both', 0, 'both', 'http://x/both'),
                (21, 2, 'rss', 'dup-only', 0, 'dup-only', 'http://x/dup-only');
             INSERT INTO entry_tags (entry_id, tag_id)
             SELECT 20, id FROM tags WHERE name = 'system:read';
             INSERT INTO entry_tags (entry_id, tag_id)
             SELECT 21, id FROM tags WHERE name = 'system:saved';",
        )
        .unwrap();
        conn
    }

    fn ids(conn: &Connection, sql: &str) -> Vec<i64> {
        let mut stmt = conn.prepare(sql).unwrap();
        stmt.query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    fn tag_names(conn: &Connection, entry_id: i64) -> Vec<String> {
        let mut stmt = conn
            .prepare(
                "SELECT t.name FROM entry_tags et JOIN tags t ON t.id = et.tag_id
                 WHERE et.entry_id = ?1 ORDER BY t.name",
            )
            .unwrap();
        stmt.query_map([entry_id], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    #[test]
    fn merges_tags_and_entries() {
        let conn = setup();
        let merged = merge_feed_into(&conn, 2, "https://example.com/feed").unwrap();
        assert_eq!(
            merged,
            Some(MergedFeed {
                into: 1,
                url: "https://example.com/feed/".into(),
                title: "dup".into(),
            })
        );

        assert_eq!(ids(&conn, "SELECT id FROM feeds"), [1]);
        assert_eq!(
            ids(&conn, "SELECT tag_id FROM feed_tags ORDER BY tag_id"),
            [100, 101]
        );
        // The entry both feeds had keeps the kept feed's copy, which gains
        // the duplicate's read tag; the other entry moves over as it is.
        assert_eq!(
            ids(
                &conn,
                "SELECT id FROM entries WHERE feed_id = 1 ORDER BY id"
            ),
            [10, 21]
        );
        assert!(ids(&conn, "SELECT id FROM entries WHERE feed_id IS NULL").is_empty());
        assert_eq!(tag_names(&conn, 10), ["system:read"]);
        assert_eq!(tag_names(&conn, 21), ["system:saved"]);
    }

    #[test]
    fn leaves_feed_alone_without_another_feed_at_url() {
        let conn = setup();
        assert_eq!(
            merge_feed_into(&conn, 2, "https://example.com/elsewhere").unwrap(),
            None
        );
        // Its own URL is not another feed's.
        assert_eq!(
            merge_feed_into(&conn, 2, "https://example.com/feed/").unwrap(),
            None
        );
        assert_eq!(ids(&conn, "SELECT id FROM feeds ORDER BY id"), [1, 2]);
        assert_eq!(
            ids(
                &conn,
                "SELECT id FROM entries WHERE feed_id = 2 ORDER BY id"
            ),
            [20, 21]
        );
    }
}
