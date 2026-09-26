#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
//! End-to-end tests for feed asset caching: serves an RSS feed with an
//! inline `<img>` and an enclosure, refreshes the feed through the worker,
//! then asserts the assets are fetched, indexed, and served back.

use super::super::*;
use crate::test::TestBuilder;
use anyhow::Result;
use axum::{routing::get, Router};
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::OpenFlags;
use std::net::SocketAddr;
use std::sync::Arc;

fn make_pool(path: &std::path::Path) -> Result<r2d2::Pool<SqliteConnectionManager>> {
    let manager = SqliteConnectionManager::file(path)
        .with_flags(OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .with_init(|c| c.execute_batch("PRAGMA foreign_keys=ON;"));
    Ok(r2d2::Pool::new(manager)?)
}

/// One-pixel PNG (89 bytes) — a valid image the test server can return.
const TINY_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x08, 0x99, 0x63, 0xF8, 0x0F, 0x04, 0x00,
    0x09, 0xFB, 0x03, 0xFD, 0x04, 0xA5, 0xB1, 0x27, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44,
    0xAE, 0x42, 0x60, 0x82,
];

/// Fake enclosure bytes.
const ENCLOSURE: &[u8] = b"fake-mp3-bytes-for-test";

/// Spawn a test server that serves:
/// - `/feed.xml` — an RSS feed whose single item references `/img.png` and
///   `/audio.mp3` via enclosure
/// - `/img.png` — `TINY_PNG`
/// - `/audio.mp3` — `ENCLOSURE`
async fn start_asset_server() -> Result<SocketAddr> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let base = format!("http://{}", addr);

    let feed_body = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0">
  <channel>
    <title>asset test</title>
    <link>{base}/</link>
    <description>d</description>
    <item>
      <title>with assets</title>
      <link>{base}/article/1</link>
      <guid isPermaLink="false">asset-entry-1</guid>
      <pubDate>Mon, 01 Jan 2024 00:00:00 GMT</pubDate>
      <description>&lt;p&gt;See &lt;img src="{base}/img.png"&gt;&lt;/p&gt;</description>
      <enclosure url="{base}/audio.mp3" length="23" type="audio/mpeg"/>
    </item>
  </channel>
</rss>"#
    );

    let feed_body = Arc::new(feed_body);
    let feed_clone = feed_body.clone();

    let app = Router::new()
        .route(
            "/feed.xml",
            get(move || {
                let body = feed_clone.as_ref().clone();
                async move { ([("content-type", "application/rss+xml")], body) }
            }),
        )
        .route(
            "/img.png",
            get(|| async { ([("content-type", "image/png")], TINY_PNG) }),
        )
        .route(
            "/audio.mp3",
            get(|| async { ([("content-type", "audio/mpeg")], ENCLOSURE) }),
        );

    tokio::spawn(async move { axum::serve(listener, app).await.ok() });
    Ok(addr)
}

/// Full path refresh -> cache: inserts an entry with an inline <img> and an
/// enclosure, then asserts the asset files, index rows, and join rows all
/// land correctly.
#[tokio::test]
async fn asset_cache_refresh_ingests_inline_and_enclosure() -> Result<()> {
    let tc = TestBuilder::default().init_database().build()?;
    let data_dir = tc.config_dir().to_path_buf();
    let addr = start_asset_server().await?;
    let feed_url = format!("http://{}/feed.xml", addr);

    let conn = tc.database_conn()?;
    conn.execute(
        "INSERT INTO feeds (title, url) VALUES ('assets feed', ?1)",
        [&feed_url],
    )?;
    let feed_id = conn.last_insert_rowid();
    drop(conn);

    let client = reqwest::Client::builder()
        .user_agent(crate::http::USER_AGENT)
        .build()?;
    let pool = make_pool(&tc.database_path())?;

    // Wire up a real channel so post-refresh `CacheEntryAssets` tasks are
    // buffered for us to execute manually.
    let (tx, rx) = async_channel::bounded::<TaskManagerCommand>(64);
    refresh_feed(
        &client,
        feed_id,
        pool.clone(),
        None,
        &super::test_metrics(),
        &tx,
    )
    .await?;

    // Drain the queued CacheEntryAssets commands and run each.
    while let Ok(cmd) = rx.try_recv() {
        if let TaskManagerCommand::CacheEntryAssets { entry_id } = cmd {
            super::super::cache_entry_assets(&client, &pool, &data_dir, entry_id).await?;
        }
    }

    // Verify: two assets (inline img + enclosure) were cached.
    let conn = tc.database_conn()?;
    let asset_count: i64 = conn.query_row("SELECT COUNT(*) FROM feed_assets", [], |r| r.get(0))?;
    assert_eq!(asset_count, 2, "expected 2 cached assets");

    let link_count: i64 = conn.query_row("SELECT COUNT(*) FROM entry_assets", [], |r| r.get(0))?;
    assert_eq!(link_count, 2, "expected 2 entry_assets links");

    let kinds: Vec<String> = {
        let mut stmt = conn.prepare("SELECT kind FROM entry_assets ORDER BY kind")?;
        let rows: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<_>>()?;
        rows
    };
    assert_eq!(kinds, vec!["enclosure", "inline_img"]);

    // Verify the files actually exist on disk at the content-addressed
    // paths.
    let hashes: Vec<String> = {
        let mut stmt = conn.prepare("SELECT blake3 FROM feed_assets")?;
        let rows: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<_>>()?;
        rows
    };
    for h in &hashes {
        let path = crate::tasks::assets::asset_path(&data_dir, h);
        assert!(path.exists(), "asset file missing: {:?}", path);
    }

    // Sizes were recorded.
    let total: i64 = conn.query_row(
        "SELECT COALESCE(SUM(size_bytes), 0) FROM feed_assets",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(total as usize, TINY_PNG.len() + ENCLOSURE.len());

    Ok(())
}

/// Disabling the cache via settings prevents fetches.
#[tokio::test]
async fn asset_cache_disabled_skips_fetches() -> Result<()> {
    let tc = TestBuilder::default().init_database().build()?;
    let data_dir = tc.config_dir().to_path_buf();
    let addr = start_asset_server().await?;
    let feed_url = format!("http://{}/feed.xml", addr);

    let conn = tc.database_conn()?;
    conn.execute(
        "INSERT INTO feeds (title, url) VALUES ('assets off', ?1)",
        [&feed_url],
    )?;
    let feed_id = conn.last_insert_rowid();
    crate::db::assets::set_cache_enabled(&conn, false)?;
    drop(conn);

    let client = reqwest::Client::builder()
        .user_agent(crate::http::USER_AGENT)
        .build()?;
    let pool = make_pool(&tc.database_path())?;

    let (tx, rx) = async_channel::bounded::<TaskManagerCommand>(64);
    refresh_feed(
        &client,
        feed_id,
        pool.clone(),
        None,
        &super::test_metrics(),
        &tx,
    )
    .await?;

    while let Ok(cmd) = rx.try_recv() {
        if let TaskManagerCommand::CacheEntryAssets { entry_id } = cmd {
            super::super::cache_entry_assets(&client, &pool, &data_dir, entry_id).await?;
        }
    }

    let conn = tc.database_conn()?;
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM feed_assets", [], |r| r.get(0))?;
    assert_eq!(n, 0, "cache disabled — no assets should be stored");

    Ok(())
}

/// A feed that lies about the Content-Type (serving HTML for an `<img src>`)
/// should not be cached: the allowlist rejects anything that isn't an image.
#[tokio::test]
async fn asset_cache_rejects_disallowed_content_type() -> Result<()> {
    let tc = TestBuilder::default().init_database().build()?;
    let data_dir = tc.config_dir().to_path_buf();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let base = format!("http://{}", addr);
    let feed_body = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0">
  <channel>
    <title>malicious</title>
    <link>{base}/</link>
    <description>d</description>
    <item>
      <title>html-in-img</title>
      <link>{base}/article/1</link>
      <guid isPermaLink="false">evil-1</guid>
      <pubDate>Mon, 01 Jan 2024 00:00:00 GMT</pubDate>
      <description>&lt;p&gt;&lt;img src="{base}/evil.png"&gt;&lt;/p&gt;</description>
    </item>
  </channel>
</rss>"#
    );
    let feed_body = Arc::new(feed_body);
    let feed_clone = feed_body.clone();
    let app = Router::new()
        .route(
            "/feed.xml",
            get(move || {
                let body = feed_clone.as_ref().clone();
                async move { ([("content-type", "application/rss+xml")], body) }
            }),
        )
        .route(
            "/evil.png",
            get(|| async { ([("content-type", "text/html")], "<script>alert(1)</script>") }),
        );
    tokio::spawn(async move { axum::serve(listener, app).await.ok() });

    let feed_url = format!("http://{}/feed.xml", addr);
    let conn = tc.database_conn()?;
    conn.execute(
        "INSERT INTO feeds (title, url) VALUES ('evil', ?1)",
        [&feed_url],
    )?;
    let feed_id = conn.last_insert_rowid();
    drop(conn);

    let client = reqwest::Client::builder()
        .user_agent(crate::http::USER_AGENT)
        .build()?;
    let pool = make_pool(&tc.database_path())?;

    let (tx, rx) = async_channel::bounded::<TaskManagerCommand>(64);
    refresh_feed(
        &client,
        feed_id,
        pool.clone(),
        None,
        &super::test_metrics(),
        &tx,
    )
    .await?;
    while let Ok(cmd) = rx.try_recv() {
        if let TaskManagerCommand::CacheEntryAssets { entry_id } = cmd {
            super::super::cache_entry_assets(&client, &pool, &data_dir, entry_id).await?;
        }
    }

    let conn = tc.database_conn()?;
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM feed_assets", [], |r| r.get(0))?;
    assert_eq!(n, 0, "text/html asset should be rejected by allowlist");

    Ok(())
}

/// Setting a max_bytes lower than the enclosure size causes inline eviction
/// right after the over-cap insert.
#[tokio::test]
async fn asset_cache_evicts_when_over_cap() -> Result<()> {
    let tc = TestBuilder::default().init_database().build()?;
    let data_dir = tc.config_dir().to_path_buf();
    let addr = start_asset_server().await?;
    let feed_url = format!("http://{}/feed.xml", addr);

    let conn = tc.database_conn()?;
    conn.execute(
        "INSERT INTO feeds (title, url) VALUES ('tiny cap', ?1)",
        [&feed_url],
    )?;
    let feed_id = conn.last_insert_rowid();
    // Cap the cache at 1 byte so every insert triggers eviction back to 0.
    crate::db::assets::set_cache_max_bytes(&conn, 1)?;
    drop(conn);

    let client = reqwest::Client::builder()
        .user_agent(crate::http::USER_AGENT)
        .build()?;
    let pool = make_pool(&tc.database_path())?;

    let (tx, rx) = async_channel::bounded::<TaskManagerCommand>(64);
    refresh_feed(
        &client,
        feed_id,
        pool.clone(),
        None,
        &super::test_metrics(),
        &tx,
    )
    .await?;
    while let Ok(cmd) = rx.try_recv() {
        if let TaskManagerCommand::CacheEntryAssets { entry_id } = cmd {
            super::super::cache_entry_assets(&client, &pool, &data_dir, entry_id).await?;
        }
    }

    let conn = tc.database_conn()?;
    // Both assets get inserted and then evicted: the cache ends empty, and
    // the on-disk files have been unlinked.
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM feed_assets", [], |r| r.get(0))?;
    assert_eq!(
        n, 0,
        "with max_bytes=1 the cache should evict everything it just inserted"
    );

    let assets_dir = data_dir.join("assets");
    if assets_dir.exists() {
        // Walk assets/** and confirm no regular files remain.
        let mut found = Vec::new();
        for shard in std::fs::read_dir(&assets_dir)? {
            let shard = shard?;
            if shard.file_type()?.is_dir() {
                for entry in std::fs::read_dir(shard.path())? {
                    let entry = entry?;
                    if entry.file_type()?.is_file() {
                        found.push(entry.path());
                    }
                }
            }
        }
        assert!(found.is_empty(), "orphaned asset files: {:?}", found);
    }

    Ok(())
}
