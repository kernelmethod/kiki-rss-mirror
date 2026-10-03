#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
//! End-to-end tests for feed asset caching: serves an RSS feed with an
//! inline `<img>` and an enclosure, refreshes the feed through the worker,
//! then asserts the assets are fetched, indexed, and served back.

use super::super::*;
use crate::test::TestBuilder;
use anyhow::Result;
use axum::{routing::get, Router};
use std::net::SocketAddr;
use std::sync::Arc;

fn make_pool(path: &std::path::Path) -> Result<crate::db::Db> {
    crate::db::Db::open(path, Default::default())
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
    let tx = crate::tasks::TaskSender::from(tx);
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
            super::super::cache_entry_assets(
                &test_fetcher(&client),
                &Default::default(),
                &pool,
                &data_dir,
                &crate::config::Settings::default().asset_cache,
                entry_id,
            )
            .await?;
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
    let mut cache = crate::config::Settings::default().asset_cache;
    cache.enabled = false;
    drop(conn);

    let client = reqwest::Client::builder()
        .user_agent(crate::http::USER_AGENT)
        .build()?;
    let pool = make_pool(&tc.database_path())?;

    let (tx, rx) = async_channel::bounded::<TaskManagerCommand>(64);
    let tx = crate::tasks::TaskSender::from(tx);
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
            super::super::cache_entry_assets(
                &test_fetcher(&client),
                &Default::default(),
                &pool,
                &data_dir,
                &cache,
                entry_id,
            )
            .await?;
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
    let tx = crate::tasks::TaskSender::from(tx);
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
            super::super::cache_entry_assets(
                &test_fetcher(&client),
                &Default::default(),
                &pool,
                &data_dir,
                &crate::config::Settings::default().asset_cache,
                entry_id,
            )
            .await?;
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
    let mut cache = crate::config::Settings::default().asset_cache;
    cache.max_bytes = 1;
    drop(conn);

    let client = reqwest::Client::builder()
        .user_agent(crate::http::USER_AGENT)
        .build()?;
    let pool = make_pool(&tc.database_path())?;

    let (tx, rx) = async_channel::bounded::<TaskManagerCommand>(64);
    let tx = crate::tasks::TaskSender::from(tx);
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
            super::super::cache_entry_assets(
                &test_fetcher(&client),
                &Default::default(),
                &pool,
                &data_dir,
                &cache,
                entry_id,
            )
            .await?;
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

/// How a [`start_stalling_server`] misbehaves.
#[derive(Clone, Copy)]
enum Stall {
    /// Never answers the request.
    Response,
    /// Sends the headers and part of the body, then nothing more.
    Body,
    /// Sends the headers, then one byte of the body every 50 ms, for ever.
    Drip,
}

/// Spawn a server that answers every connection as `stall` says, holding
/// connections open until the test ends.
async fn start_stalling_server(stall: Stall) -> Result<SocketAddr> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut request = [0u8; 4096];
                let _ = stream.read(&mut request).await;
                let headers = "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\n\
                               Content-Length: 1000000\r\n\r\n";
                match stall {
                    Stall::Response => {}
                    Stall::Body => {
                        let _ = stream.write_all(headers.as_bytes()).await;
                        let _ = stream.write_all(&TINY_PNG[..16]).await;
                    }
                    Stall::Drip => {
                        let _ = stream.write_all(headers.as_bytes()).await;
                        while stream.write_all(b"x").await.is_ok() {
                            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        }
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                drop(stream);
            });
        }
    });
    Ok(addr)
}

/// Try to cache the image at `addr` with a client limited by `timeouts`,
/// returning whether it was cached and how long that took to decide.
async fn cache_with_timeouts(
    timeouts: crate::fetcher::assets::AssetTimeouts,
    addr: SocketAddr,
) -> Result<(bool, std::time::Duration)> {
    let tc = TestBuilder::default().init_database().build()?;
    let pool = make_pool(&tc.database_path())?;
    let fetcher = crate::fetcher::Fetcher::InProcess {
        feeds: crate::fetcher::ProxiedClient::new(crate::fetcher::client_builder)?,
        assets: crate::fetcher::ProxiedClient::new(move || {
            crate::fetcher::assets::asset_client_builder(timeouts)
        })?,
    };
    let cache = super::super::assets::AssetCache {
        fetcher: &fetcher,
        proxy: &Default::default(),
        db: &pool,
        data_dir: tc.config_dir(),
        max_bytes: i64::MAX,
    };
    let url = reqwest::Url::parse(&format!("http://{addr}/img.png"))?;

    let start = std::time::Instant::now();
    let cached = super::super::assets::store_asset(
        &cache,
        &url,
        super::super::assets::AssetKind::InlineImg,
        |_, _| Ok(()),
    )
    .await?;
    Ok((cached, start.elapsed()))
}

/// A server that stops sending, before its response or part-way through
/// the body, is given up on once it has been quiet for the read timeout.
#[tokio::test]
async fn asset_downloads_time_out_when_the_server_stalls() -> Result<()> {
    let timeouts = crate::fetcher::assets::AssetTimeouts {
        connect: std::time::Duration::from_secs(5),
        read: std::time::Duration::from_millis(300),
        total: std::time::Duration::from_secs(60),
    };
    for stall in [Stall::Response, Stall::Body] {
        let addr = start_stalling_server(stall).await?;
        let (cached, elapsed) = cache_with_timeouts(timeouts, addr).await?;
        assert!(!cached);
        assert!(elapsed < std::time::Duration::from_secs(10), "{elapsed:?}");
    }
    Ok(())
}

/// A server that sends just often enough to keep the read timeout from
/// firing is cut off by the overall timeout.
#[tokio::test]
async fn asset_downloads_time_out_when_the_server_drips() -> Result<()> {
    let timeouts = crate::fetcher::assets::AssetTimeouts {
        connect: std::time::Duration::from_secs(5),
        read: std::time::Duration::from_secs(5),
        total: std::time::Duration::from_millis(500),
    };
    let addr = start_stalling_server(Stall::Drip).await?;
    let (cached, elapsed) = cache_with_timeouts(timeouts, addr).await?;
    assert!(!cached);
    assert!(elapsed < std::time::Duration::from_secs(5), "{elapsed:?}");
    Ok(())
}

/// The limits Kiki runs with leave room for a full-size enclosure.
#[test]
fn default_asset_timeouts_allow_large_downloads() {
    let t = crate::fetcher::assets::AssetTimeouts::DEFAULT;
    assert!(t.connect < t.total && t.read < t.total);
    let bits = super::super::assets::MAX_ASSET_BYTES * 8;
    assert!(bits / t.total.as_secs() <= 1_000_000);
}

/// The server does not take the fetcher's word for what it downloaded: an
/// asset of a type not allowed for its kind is refused even when the
/// fetcher claims it is fine, as a compromised fetcher might.
#[cfg(unix)]
#[tokio::test]
async fn assets_the_fetcher_should_have_refused_are_not_stored() -> Result<()> {
    use crate::fetcher::assets::{AssetReply, FetchedAsset};
    use crate::process::feed_fetcher::{FeedFetcherHost, Job, JobResult};

    let tc = TestBuilder::default().init_database().build()?;
    let pool = make_pool(&tc.database_path())?;
    let host = FeedFetcherHost::with_fake_worker(|job| match job {
        Job::FetchAsset(_) => JobResult::Asset(AssetReply::Fetched(Box::new(FetchedAsset {
            content_type: "text/html".into(),
            etag: None,
            last_modified: None,
            bytes: b"<script>alert(1)</script>".to_vec(),
        }))),
        _ => JobResult::Failed {
            message: "unexpected job".into(),
        },
    });
    let fetcher = crate::fetcher::Fetcher::Isolated(Arc::new(host));
    let cache = super::super::assets::AssetCache {
        fetcher: &fetcher,
        proxy: &Default::default(),
        db: &pool,
        data_dir: tc.config_dir(),
        max_bytes: i64::MAX,
    };
    let url = reqwest::Url::parse("http://example.invalid/img.png")?;

    let stored = super::super::assets::store_asset(
        &cache,
        &url,
        super::super::assets::AssetKind::InlineImg,
        |_, _| Ok(()),
    )
    .await?;
    assert!(!stored);
    let n: i64 = tc
        .database_conn()?
        .query_row("SELECT COUNT(*) FROM feed_assets", [], |r| r.get(0))?;
    assert_eq!(n, 0);
    Ok(())
}

/// Many tasks storing the same bytes at once, as when several feeds from
/// one website look for its favicon together, all succeed and share one
/// asset row and one file.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_stores_of_the_same_bytes_share_one_asset() -> Result<()> {
    use crate::fetcher::assets::{AssetReply, FetchedAsset};
    use crate::process::feed_fetcher::{FeedFetcherHost, Job, JobResult};
    use std::sync::atomic::{AtomicUsize, Ordering};

    let tc = TestBuilder::default().init_database().build()?;
    let pool = make_pool(&tc.database_path())?;
    let host = FeedFetcherHost::with_fake_worker(|job| match job {
        Job::FetchAsset(_) => JobResult::Asset(AssetReply::Fetched(Box::new(FetchedAsset {
            content_type: "image/png".into(),
            etag: None,
            last_modified: None,
            bytes: TINY_PNG.to_vec(),
        }))),
        _ => JobResult::Failed {
            message: "unexpected job".into(),
        },
    });
    let fetcher = crate::fetcher::Fetcher::Isolated(Arc::new(host));
    let linked = Arc::new(AtomicUsize::new(0));

    let mut stores = tokio::task::JoinSet::new();
    for i in 0..16 {
        let (fetcher, pool, linked) = (fetcher.clone(), pool.clone(), linked.clone());
        let data_dir = tc.config_dir().to_path_buf();
        stores.spawn(async move {
            let cache = super::super::assets::AssetCache {
                fetcher: &fetcher,
                proxy: &Default::default(),
                db: &pool,
                data_dir: &data_dir,
                max_bytes: i64::MAX,
            };
            let url = reqwest::Url::parse(&format!("http://site{i}.invalid/favicon.ico"))?;
            super::super::assets::store_asset(
                &cache,
                &url,
                super::super::assets::AssetKind::Favicon,
                move |_, _| {
                    linked.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
            )
            .await
        });
    }
    while let Some(stored) = stores.join_next().await {
        assert!(stored??);
    }
    assert_eq!(linked.load(Ordering::SeqCst), 16);

    let n: i64 = tc
        .database_conn()?
        .query_row("SELECT COUNT(*) FROM feed_assets", [], |r| r.get(0))?;
    assert_eq!(n, 1);
    let hash = blake3::hash(TINY_PNG).to_hex().to_string();
    let path = super::super::assets::asset_path(tc.config_dir(), &hash);
    assert_eq!(std::fs::read(&path)?, TINY_PNG);
    let leftovers = std::fs::read_dir(path.parent().unwrap())?
        .filter(|e| e.as_ref().is_ok_and(|e| e.path() != path))
        .count();
    assert_eq!(leftovers, 0);
    Ok(())
}

/// Storing new entries records their asset caching as pending, even when
/// the queue drops the task, so it can be retried; refreshing the same
/// entries again adds nothing more.
#[tokio::test(flavor = "multi_thread")]
async fn new_entries_are_recorded_as_pending_asset_caching() -> Result<()> {
    let tc = TestBuilder::default().init_database().build()?;
    let conn = tc.database_conn()?;
    conn.execute(
        "INSERT INTO feeds (title, url) VALUES ('pending', ?1)",
        [tc.example_feed_url()],
    )?;
    let feed_id = conn.last_insert_rowid();
    let client = reqwest::Client::new();
    let pool = make_pool(&tc.database_path())?;
    let pending = || -> Result<i64> {
        Ok(
            conn.query_row("SELECT COUNT(*) FROM pending_entry_assets", [], |r| {
                r.get(0)
            })?,
        )
    };

    // A queue with no receiver, which drops every task.
    let tx = super::test_tx();
    tx.close();
    refresh_feed_manual(&client, feed_id, pool.clone(), &super::test_metrics(), &tx).await?;
    let entries: i64 = conn.query_row(
        "SELECT COUNT(*) FROM entries WHERE feed_id = ?1",
        [feed_id],
        |r| r.get(0),
    )?;
    assert!(entries > 0);
    assert_eq!(pending()?, entries);

    conn.execute("DELETE FROM pending_entry_assets", [])?;
    conn.execute(
        "UPDATE feeds SET header_body_hash = NULL WHERE id = ?1",
        [feed_id],
    )?;
    refresh_feed_manual(&client, feed_id, pool, &super::test_metrics(), &tx).await?;
    assert_eq!(pending()?, 0, "entries already stored are not new");
    Ok(())
}
