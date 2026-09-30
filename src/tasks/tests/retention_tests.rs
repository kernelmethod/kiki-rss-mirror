//! Retention as driven by feed refreshes: entries are updated in place,
//! marked dropped when their feed stops listing them, and only deleted
//! once they have been dropped for longer than the retention period.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::super::*;
use crate::db::retention;
use crate::test::{TestBuilder, TestConfig};
use anyhow::Result;
use rusqlite::Connection;
use std::path::PathBuf;

const DAY: i64 = 86400;

fn make_pool(path: &std::path::Path) -> Result<crate::db::Db> {
    crate::db::Db::open(path, Default::default())
}

/// Write an RSS document listing `items`, as `(guid, title)` pairs. Every
/// item is dated 2001, far older than any retention period.
fn write_feed(path: &std::path::Path, items: &[(&str, &str)]) {
    let items: String = items
        .iter()
        .map(|(guid, title)| {
            format!(
                "<item><guid>{guid}</guid><title>{title}</title>\
                 <link>http://example.com/{guid}</link>\
                 <pubDate>Mon, 01 Jan 2001 00:00:00 +0000</pubDate></item>"
            )
        })
        .collect();
    let doc = format!(
        "<?xml version=\"1.0\"?><rss version=\"2.0\"><channel>\
         <title>t</title><link>http://example.com/</link><description>d</description>\
         {items}</channel></rss>"
    );
    std::fs::write(path, doc).unwrap();
}

/// A test database with one feed backed by a local file.
struct FileFeed {
    tc: TestConfig,
    path: PathBuf,
    feed_id: i64,
}

impl FileFeed {
    fn new() -> Result<Self> {
        let tc = TestBuilder::default().init_database().build()?;
        let path = tc.config_dir().join("feed.xml");
        let conn = tc.database_conn()?;
        conn.execute(
            "INSERT INTO feeds (title, url) VALUES ('test feed', ?1)",
            [format!("file://{}", path.display())],
        )?;
        let feed_id = conn.last_insert_rowid();
        Ok(Self { tc, path, feed_id })
    }

    fn conn(&self) -> Connection {
        self.tc.database_conn().unwrap()
    }

    /// Serve `items` from the feed file and refresh the feed.
    async fn refresh(&self, items: &[(&str, &str)]) -> Result<()> {
        write_feed(&self.path, items);
        // Make the feed eligible again regardless of the last schedule.
        self.conn()
            .execute("UPDATE feeds SET next_fetch_at = NULL", [])?;
        refresh_feed(
            &reqwest::Client::new(),
            self.feed_id,
            make_pool(&self.tc.database_path())?,
            None,
            &super::test_metrics(),
            &super::test_tx(),
        )
        .await
    }

    fn entry_id(&self, guid: &str) -> i64 {
        self.conn()
            .query_row(
                "SELECT id FROM entries WHERE feed_id = ?1 AND guid = ?2",
                rusqlite::params![self.feed_id, guid],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn dropped_at(&self, guid: &str) -> Option<i64> {
        self.conn()
            .query_row(
                "SELECT dropped_at FROM entries WHERE feed_id = ?1 AND guid = ?2",
                rusqlite::params![self.feed_id, guid],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn guids(&self) -> Vec<String> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare("SELECT guid FROM entries WHERE feed_id = ?1 ORDER BY guid")
            .unwrap();
        stmt.query_map([self.feed_id], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }
}

#[tokio::test]
async fn refresh_updates_entries_in_place() -> Result<()> {
    let f = FileFeed::new()?;
    f.refresh(&[("a", "firsttitle")]).await?;
    let id = f.entry_id("a");

    // Attach a tag by hand, as a user would through the API.
    let conn = f.conn();
    conn.execute("INSERT INTO tags (name) VALUES ('keep')", [])?;
    conn.execute(
        "INSERT INTO entry_tags (entry_id, tag_id) VALUES (?1, last_insert_rowid())",
        [id],
    )?;

    f.refresh(&[("a", "secondtitle")]).await?;

    // Same row, updated content, tag intact.
    assert_eq!(f.entry_id("a"), id);
    let title: String = conn.query_row("SELECT title FROM entries WHERE id = ?1", [id], |r| {
        r.get(0)
    })?;
    assert_eq!(title, "secondtitle");
    let tags: i64 = conn.query_row(
        "SELECT COUNT(*) FROM entry_tags WHERE entry_id = ?1",
        [id],
        |r| r.get(0),
    )?;
    assert_eq!(tags, 1);

    // The search index follows the update rather than keeping the old title.
    let matches = |term: &str| -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM entries_fts WHERE entries_fts MATCH ?1",
            [term],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(matches("firsttitle"), 0);
    assert_eq!(matches("secondtitle"), 1);

    Ok(())
}

#[tokio::test]
async fn entries_are_marked_dropped_and_restored() -> Result<()> {
    let f = FileFeed::new()?;
    f.refresh(&[("a", "A"), ("b", "B")]).await?;
    assert_eq!(f.dropped_at("a"), None);
    assert_eq!(f.dropped_at("b"), None);

    // "b" drops off the feed.
    f.refresh(&[("a", "A")]).await?;
    assert_eq!(f.dropped_at("a"), None);
    let first_drop = f.dropped_at("b").expect("b marked dropped");

    // Staying off the feed doesn't restart the clock.
    f.conn().execute(
        "UPDATE entries SET dropped_at = dropped_at - 100 WHERE guid = 'b'",
        [],
    )?;
    f.refresh(&[("a", "A")]).await?;
    assert_eq!(f.dropped_at("b"), Some(first_drop - 100));

    // Reappearing clears the mark.
    f.refresh(&[("a", "A"), ("b", "B")]).await?;
    assert_eq!(f.dropped_at("b"), None);

    Ok(())
}

#[tokio::test]
async fn empty_feed_marks_nothing_dropped() -> Result<()> {
    let f = FileFeed::new()?;
    f.refresh(&[("a", "A")]).await?;
    f.refresh(&[]).await?;
    assert_eq!(f.dropped_at("a"), None);
    Ok(())
}

#[tokio::test]
async fn retention_deletes_only_long_dropped_entries() -> Result<()> {
    let f = FileFeed::new()?;
    f.refresh(&[("current", "C"), ("recent", "R"), ("old", "O")])
        .await?;
    f.refresh(&[("current", "C")]).await?;

    // "old" left the feed ten days ago; "recent" just now. All three were
    // published in 2001.
    f.conn().execute(
        "UPDATE entries SET dropped_at = dropped_at - 10 * ?1 WHERE guid = 'old'",
        [DAY],
    )?;

    let conn = f.conn();
    assert_eq!(retention::cleanup_feed(&conn, f.feed_id, Some(7))?, 1);
    assert_eq!(f.guids(), ["current", "recent"]);
    assert_eq!(retention::cleanup_all(&conn, Some(7))?, 0);
    assert_eq!(f.guids(), ["current", "recent"]);

    Ok(())
}

/// A saved entry the feed stopped listing survives cleanup for as long as it
/// stays saved.
#[tokio::test]
async fn cleanup_keeps_saved_entries_dropped_from_the_feed() -> Result<()> {
    let f = FileFeed::new()?;
    f.refresh(&[("saved", "s"), ("unsaved", "u"), ("current", "c")])
        .await?;
    let conn = f.conn();
    conn.execute(
        "INSERT INTO entry_tags (entry_id, tag_id)
         SELECT ?1, id FROM tags WHERE name = 'system:saved'",
        [f.entry_id("saved")],
    )?;

    // Both drop off the feed, long enough ago to be past the cutoff.
    f.refresh(&[("current", "c")]).await?;
    assert!(f.dropped_at("saved").is_some());
    conn.execute(
        "UPDATE entries SET dropped_at = dropped_at - 10 * ?1",
        [DAY],
    )?;

    assert_eq!(retention::cleanup_all(&conn, Some(7))?, 1);
    assert_eq!(f.guids(), ["current", "saved"]);
    Ok(())
}
