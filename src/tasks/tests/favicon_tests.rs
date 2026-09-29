#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
//! End-to-end tests for favicon caching: serves a feed and the website it
//! belongs to, refreshes the feed, runs the queued favicon task, and checks
//! which icon ends up cached.

use super::super::*;
use crate::test::TestBuilder;
use anyhow::Result;
use axum::{response::IntoResponse, routing::get, Router};
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::OpenFlags;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

fn make_pool(path: &std::path::Path) -> Result<r2d2::Pool<SqliteConnectionManager>> {
    let manager = SqliteConnectionManager::file(path)
        .with_flags(OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .with_init(|c| c.execute_batch("PRAGMA foreign_keys=ON;"));
    Ok(r2d2::Pool::new(manager)?)
}

/// One-pixel PNG.
const TINY_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x08, 0x99, 0x63, 0xF8, 0x0F, 0x04, 0x00,
    0x09, 0xFB, 0x03, 0xFD, 0x04, 0xA5, 0xB1, 0x27, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44,
    0xAE, 0x42, 0x60, 0x82,
];

/// Stand-in bytes for an `.ico` file; only the content type is checked.
const FAKE_ICO: &[u8] = b"not-really-an-ico";

/// The test feed's `ETag`.
const FEED_ETAG: &str = "\"v1\"";

/// What the test website serves, besides the feed.
#[derive(Clone, Copy)]
struct Site {
    /// The feed's `<link>` points at `/site/`, which links to icons.
    link_to_home_page: bool,
    /// `/favicon.ico` exists.
    favicon_ico: bool,
    /// The feed is Atom with an `<icon>` of `/atom-icon.png`.
    atom_icon: bool,
}

/// A running test website and how many requests it has served.
struct Server {
    base: String,
    requests: Arc<AtomicUsize>,
}

async fn start_server(site: Site) -> Result<Server> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    let requests = Arc::new(AtomicUsize::new(0));

    let link = if site.link_to_home_page {
        format!("{base}/site/")
    } else {
        String::new()
    };
    let (feed_type, feed) = if site.atom_icon {
        (
            "application/atom+xml",
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <title>t</title><id>urn:t</id><updated>2024-01-01T00:00:00Z</updated>
  <link rel="alternate" type="text/html" href="{link}"/>
  <icon>/atom-icon.png</icon>
  <entry><title>e</title><id>urn:e</id><updated>2024-01-01T00:00:00Z</updated>
    <published>2024-01-01T00:00:00Z</published><link href="{base}/e"/></entry>
</feed>"#
            ),
        )
    } else {
        (
            "application/rss+xml",
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0"><channel>
  <title>t</title><link>{link}</link><description>d</description>
  <item><title>e</title><guid>g</guid><link>{base}/e</link></item>
</channel></rss>"#
            ),
        )
    };
    let home_page = r#"<!doctype html><html><head>
        <link rel="icon" type="image/svg+xml" href="/icon.svg">
        <link rel="icon" sizes="16x16" href="/icons/16.png">
        <link rel="icon" sizes="32x32" href="/icons/32.png">
        </head><body>hi</body></html>"#;

    let mut app = Router::new()
        .route(
            "/feed.xml",
            // The feed never changes, so it is 304 whenever Kiki
            // revalidates it.
            get(move |headers: axum::http::HeaderMap| async move {
                if headers.get("if-none-match").is_some_and(|v| v == FEED_ETAG) {
                    return axum::http::StatusCode::NOT_MODIFIED.into_response();
                }
                ([("content-type", feed_type), ("etag", FEED_ETAG)], feed).into_response()
            }),
        )
        .route(
            "/site/",
            get(move || async move { ([("content-type", "text/html; charset=utf-8")], home_page) }),
        )
        .route(
            "/icon.svg",
            get(|| async { ([("content-type", "image/svg+xml")], "<svg/>") }),
        )
        .route(
            "/icons/32.png",
            get(|| async { ([("content-type", "image/png")], TINY_PNG) }),
        )
        .route(
            "/atom-icon.png",
            get(|| async { ([("content-type", "image/png")], &TINY_PNG[..40]) }),
        );
    if site.favicon_ico {
        app = app.route(
            "/favicon.ico",
            get(|| async { ([("content-type", "image/x-icon")], FAKE_ICO) }),
        );
    }
    let counter = requests.clone();
    let app = app.layer(axum::middleware::from_fn(
        move |req: axum::extract::Request, next: axum::middleware::Next| {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                next.run(req).await
            }
        },
    ));
    tokio::spawn(async move { axum::serve(listener, app).await.ok() });
    Ok(Server { base, requests })
}

/// A test database with one feed, whose favicon tasks are run by hand.
struct Harness {
    tc: crate::test::TestConfig,
    pool: r2d2::Pool<SqliteConnectionManager>,
    client: reqwest::Client,
    feed_id: i64,
    cache: crate::config::AssetCacheSettings,
}

impl Harness {
    async fn new(feed_url: &str) -> Result<Self> {
        let tc = TestBuilder::default().init_database().build()?;
        let conn = tc.database_conn()?;
        conn.execute(
            "INSERT INTO feeds (title, url) VALUES ('favicon feed', ?1)",
            [feed_url],
        )?;
        let feed_id = conn.last_insert_rowid();
        drop(conn);
        let pool = make_pool(&tc.database_path())?;
        let client = reqwest::Client::builder()
            .user_agent(crate::http::USER_AGENT)
            .build()?;
        Ok(Harness {
            tc,
            pool,
            client,
            feed_id,
            cache: crate::config::Settings::default().asset_cache,
        })
    }

    /// Refresh the feed, then run the favicon tasks it queued. Returns how
    /// many were queued.
    async fn refresh(&self) -> Result<usize> {
        // Make the feed due, so that every call really fetches it.
        self.tc.database_conn()?.execute(
            "UPDATE feeds SET next_fetch_at = NULL WHERE id = ?1",
            [self.feed_id],
        )?;
        let settings = crate::config::Settings {
            asset_cache: self.cache.clone(),
            ..Default::default()
        };
        let (tx, rx) = async_channel::bounded::<TaskManagerCommand>(64);
        refresh_feed_with_settings(
            &self.client,
            self.feed_id,
            self.pool.clone(),
            &settings,
            None,
            &super::test_metrics(),
            &tx,
        )
        .await?;
        let mut queued = 0;
        while let Ok(cmd) = rx.try_recv() {
            if let TaskManagerCommand::CacheFeedFavicon { feed_id } = cmd {
                queued += 1;
                self.cache_favicon(feed_id).await?;
            }
        }
        Ok(queued)
    }

    async fn cache_favicon(&self, feed_id: i64) -> Result<()> {
        cache_feed_favicon(
            &self.client,
            &self.pool,
            self.tc.config_dir(),
            &self.cache,
            feed_id,
        )
        .await
    }

    /// The original URL of the cached favicon, if any.
    fn favicon_source(&self) -> Result<Option<String>> {
        let conn = self.tc.database_conn()?;
        Ok(conn.query_row(
            "SELECT fa.original_url FROM feeds f
             LEFT JOIN feed_favicons ff ON ff.feed_id = f.id
             LEFT JOIN feed_assets fa ON fa.id = ff.asset_id
             WHERE f.id = ?1",
            [self.feed_id],
            |row| row.get(0),
        )?)
    }

    fn check(&self) -> Result<Option<crate::db::favicons::FaviconCheck>> {
        crate::db::favicons::last_check(&self.tc.database_conn()?, self.feed_id)
    }
}

/// The feed's website links to its icons; the best raster one is cached,
/// the site link is stored, and the next refresh leaves the favicon alone.
#[tokio::test]
async fn caches_icon_linked_from_home_page() -> Result<()> {
    let server = start_server(Site {
        link_to_home_page: true,
        favicon_ico: true,
        atom_icon: false,
    })
    .await?;
    let h = Harness::new(&format!("{}/feed.xml", server.base)).await?;

    assert_eq!(h.refresh().await?, 1);
    assert_eq!(
        h.favicon_source()?,
        Some(format!("{}/icons/32.png", server.base))
    );

    let conn = h.tc.database_conn()?;
    let site_url: Option<String> = conn.query_row(
        "SELECT site_url FROM feeds WHERE id = ?1",
        [h.feed_id],
        |r| r.get(0),
    )?;
    assert_eq!(site_url, Some(format!("{}/site/", server.base)));
    let hash = crate::db::favicons::favicon_hash(&conn, h.feed_id)?.unwrap();
    assert_eq!(hash, blake3::hash(TINY_PNG).to_hex().to_string());
    assert!(crate::tasks::assets::asset_path(h.tc.config_dir(), &hash).exists());

    // Found recently: no task is queued, and running one anyway fetches
    // nothing.
    let before = server.requests.load(Ordering::SeqCst);
    assert_eq!(h.refresh().await?, 0);
    h.cache_favicon(h.feed_id).await?;
    // Only the feed itself was fetched.
    assert_eq!(server.requests.load(Ordering::SeqCst), before + 1);
    Ok(())
}

/// Without a website link, `/favicon.ico` at the feed's own site is used.
#[tokio::test]
async fn falls_back_to_favicon_ico() -> Result<()> {
    let server = start_server(Site {
        link_to_home_page: false,
        favicon_ico: true,
        atom_icon: false,
    })
    .await?;
    let h = Harness::new(&format!("{}/feed.xml", server.base)).await?;

    h.refresh().await?;
    assert_eq!(
        h.favicon_source()?,
        Some(format!("{}/favicon.ico", server.base))
    );
    Ok(())
}

/// An Atom feed's own `<icon>` comes before anything on the website.
#[tokio::test]
async fn prefers_atom_icon() -> Result<()> {
    let server = start_server(Site {
        link_to_home_page: true,
        favicon_ico: true,
        atom_icon: true,
    })
    .await?;
    let h = Harness::new(&format!("{}/feed.xml", server.base)).await?;

    h.refresh().await?;
    assert_eq!(
        h.favicon_source()?,
        Some(format!("{}/atom-icon.png", server.base))
    );
    Ok(())
}

/// A site with no usable icon is recorded as such, so Kiki does not keep
/// looking on every refresh.
#[tokio::test]
async fn records_missing_favicon() -> Result<()> {
    let server = start_server(Site {
        link_to_home_page: false,
        favicon_ico: false,
        atom_icon: false,
    })
    .await?;
    let h = Harness::new(&format!("{}/feed.xml", server.base)).await?;

    assert_eq!(h.refresh().await?, 1);
    assert_eq!(h.favicon_source()?, None);
    let check = h.check()?.expect("the look is recorded");
    assert_eq!(check.asset_id, None);
    assert_eq!(h.refresh().await?, 0);
    Ok(())
}

/// With the asset cache disabled, favicons are neither queued nor fetched.
#[tokio::test]
async fn disabled_cache_skips_favicons() -> Result<()> {
    let server = start_server(Site {
        link_to_home_page: true,
        favicon_ico: true,
        atom_icon: false,
    })
    .await?;
    let mut h = Harness::new(&format!("{}/feed.xml", server.base)).await?;
    h.cache.enabled = false;

    assert_eq!(h.refresh().await?, 0);
    h.cache_favicon(h.feed_id).await?;
    assert_eq!(h.check()?, None);
    assert_eq!(server.requests.load(Ordering::SeqCst), 1);
    Ok(())
}

/// A feed that was already being revalidated when favicons were introduced
/// gets one even though its server only ever answers 304.
#[tokio::test]
async fn looks_for_favicon_of_unchanged_feed() -> Result<()> {
    let server = start_server(Site {
        link_to_home_page: true,
        favicon_ico: true,
        atom_icon: false,
    })
    .await?;
    let mut h = Harness::new(&format!("{}/feed.xml", server.base)).await?;

    // The first refresh stores the feed and its ETag, but, as before
    // favicons existed, looks for no favicon.
    h.cache.enabled = false;
    assert_eq!(h.refresh().await?, 0);
    h.cache.enabled = true;

    let before = server.requests.load(Ordering::SeqCst);
    assert_eq!(h.refresh().await?, 1);
    assert_eq!(
        h.favicon_source()?,
        Some(format!("{}/icons/32.png", server.base))
    );
    // The feed was revalidated, not downloaded again: one 304, then the
    // home page and the icon.
    assert_eq!(server.requests.load(Ordering::SeqCst), before + 3);

    // Found recently: the next 304 queues nothing.
    assert_eq!(h.refresh().await?, 0);
    Ok(())
}
