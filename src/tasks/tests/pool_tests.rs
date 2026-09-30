//! A refresh must never hold more than one pooled connection at a time, or
//! hold one while it waits on the network: otherwise concurrent refreshes
//! exhaust the pool and wait on each other until the pool times out.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::super::*;
use crate::test::TestBuilder;
use anyhow::Result;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::{routing::get, Router};
use std::time::Duration;

const RSS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0"><channel>
  <title>Pool test</title><link>http://example.com/</link><description>d</description>
  <item><title>One</title><link>http://example.com/1</link><guid>one</guid></item>
</channel></rss>"#;

/// A database with `readers` read connections (and, as always, one
/// writer) that gives up on a checkout after a second, so a refresh that
/// waits on a connection it holds fails quickly instead of hanging for
/// r2d2's default 30 seconds.
fn small_db(path: &std::path::Path, readers: u32) -> Result<crate::db::Db> {
    crate::db::Db::open(
        path,
        crate::db::DbOptions {
            readers: Some(readers),
            connection_timeout: Some(Duration::from_secs(1)),
            ..Default::default()
        },
    )
}

fn client() -> Result<reqwest::Client> {
    #[allow(clippy::disallowed_methods)]
    Ok(reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}

/// Serve `app` on a local port and return its base URL.
async fn serve(app: Router) -> Result<String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move { axum::serve(listener, app).await.ok() });
    Ok(format!("http://{}", addr))
}

fn add_feed(conn: &rusqlite::Connection, url: &str) -> Result<i64> {
    conn.execute(
        "INSERT INTO feeds (title, url) VALUES ('pool test', ?1)",
        [url],
    )?;
    Ok(conn.last_insert_rowid())
}

/// With a single connection, every kind of refresh outcome completes: a
/// new feed, a 304, a failed request, and a body that is not a feed.
#[tokio::test]
async fn refresh_needs_only_one_connection() -> Result<()> {
    let tc = TestBuilder::default().init_database().build()?;
    let app = Router::new()
        .route(
            "/rss",
            get(|headers: HeaderMap| async move {
                if headers.contains_key("if-none-match") {
                    StatusCode::NOT_MODIFIED.into_response()
                } else {
                    (
                        [("content-type", "application/rss+xml"), ("etag", "\"v1\"")],
                        RSS,
                    )
                        .into_response()
                }
            }),
        )
        .route(
            "/error",
            get(|| async { StatusCode::INTERNAL_SERVER_ERROR }),
        )
        .route("/html", get(|| async { "<html></html>" }));
    let base = serve(app).await?;

    let conn = tc.database_conn()?;
    let rss = add_feed(&conn, &format!("{base}/rss"))?;
    let error = add_feed(&conn, &format!("{base}/error"))?;
    let html = add_feed(&conn, &format!("{base}/html"))?;

    let pool = small_db(&tc.database_path(), 1)?;
    let (client, metrics, tx) = (client()?, super::test_metrics(), super::test_tx());

    refresh_feed(&client, rss, pool.clone(), None, &metrics, &tx).await?;
    let entries: i64 = conn.query_row(
        "SELECT COUNT(*) FROM entries WHERE feed_id = ?1",
        [rss],
        |r| r.get(0),
    )?;
    assert_eq!(entries, 1);

    // Sends the stored ETag, so the server answers 304.
    refresh_feed_manual(&client, rss, pool.clone(), &metrics, &tx).await?;
    refresh_feed(&client, error, pool.clone(), None, &metrics, &tx).await?;
    refresh_feed(&client, html, pool.clone(), None, &metrics, &tx).await?;

    for (feed_id, failed) in [(rss, false), (error, true), (html, true)] {
        let stored: Option<String> = conn.query_row(
            "SELECT last_fetch_error FROM feeds WHERE id = ?1",
            [feed_id],
            |r| r.get(0),
        )?;
        assert_eq!(stored.is_some(), failed, "feed {feed_id}: {stored:?}");
    }
    Ok(())
}

/// More refreshes in flight than the pool has connections, each waiting on
/// a slow server, all complete: none holds a connection over the fetch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_refreshes_outnumbering_the_pool() -> Result<()> {
    let tc = TestBuilder::default().init_database().build()?;
    let app = Router::new().route(
        "/rss/{n}",
        get(|| async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            ([("content-type", "application/rss+xml")], RSS)
        }),
    );
    let base = serve(app).await?;

    let conn = tc.database_conn()?;
    let feed_ids = (0..8)
        .map(|n| add_feed(&conn, &format!("{base}/rss/{n}")))
        .collect::<Result<Vec<_>>>()?;

    let pool = small_db(&tc.database_path(), 2)?;
    let client = client()?;
    let metrics = std::sync::Arc::new(super::test_metrics());
    let tx = super::test_tx();
    let mut refreshes = tokio::task::JoinSet::new();
    for &id in &feed_ids {
        let (client, pool, metrics, tx) =
            (client.clone(), pool.clone(), metrics.clone(), tx.clone());
        refreshes.spawn(async move { refresh_feed(&client, id, pool, None, &metrics, &tx).await });
    }
    while let Some(result) = refreshes.join_next().await {
        result??;
    }

    let refreshed: i64 = conn.query_row(
        "SELECT COUNT(*) FROM feeds WHERE last_checked IS NOT NULL AND last_fetch_error IS NULL",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(refreshed, feed_ids.len() as i64);
    Ok(())
}
