#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::super::*;
use crate::test::TestBuilder;
use anyhow::Result;
use axum::{routing::get, Router};
use rusqlite::OpenFlags;

fn make_pool(path: &std::path::Path) -> Result<r2d2::Pool<SqliteConnectionManager>> {
    let manager = SqliteConnectionManager::file(path)
        .with_flags(OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .with_init(|c| c.execute_batch("PRAGMA foreign_keys=ON;"));
    Ok(r2d2::Pool::new(manager)?)
}

/// Helper: insert a feed pointing at the given URL and return the feed id,
/// an HTTP client (with manual redirect policy), and a connection pool.
fn setup_feed(
    tc: &crate::test::TestConfig,
    feed_url: &str,
) -> Result<(i64, reqwest::Client, r2d2::Pool<SqliteConnectionManager>)> {
    let conn = tc.database_conn()?;
    conn.execute(
        "INSERT INTO feeds (title, url) VALUES ('error test feed', ?1)",
        [feed_url],
    )?;
    let feed_id = conn.last_insert_rowid();

    // Use Policy::none() to match the production client in `manager()`,
    // so that redirect handling is done by our code, not reqwest.
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(crate::http::USER_AGENT)
        .build()?;
    let pool = make_pool(&tc.database_path())?;
    Ok((feed_id, client, pool))
}

/// Read the stored FetchError JSON from the database for the given feed.
fn read_stored_error(
    conn: &rusqlite::Connection,
    feed_id: i64,
) -> Result<(Option<FetchError>, Option<i64>)> {
    let (json, at): (Option<String>, Option<i64>) = conn.query_row(
        "SELECT last_fetch_error, last_fetch_error_at FROM feeds WHERE id = ?1",
        [feed_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let error = json.and_then(|s| serde_json::from_str(&s).ok());
    Ok((error, at))
}

/// When a server returns HTML instead of a feed, the fetcher stores an
/// `InvalidFeed` error and inserts no entries.
#[tokio::test]
async fn test_invalid_feed_error() -> Result<()> {
    let tc = TestBuilder::default().init_database().build()?;

    let app = Router::new().route(
        "/feed",
        get(|| async {
            (
                [("content-type", "text/html")],
                "<html><body>Not a feed</body></html>",
            )
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move { axum::serve(listener, app).await.ok() });

    let feed_url = format!("http://{}/feed", addr);
    let (feed_id, client, pool) = setup_feed(&tc, &feed_url)?;

    refresh_feed(&client, feed_id, pool, None, &super::test_metrics()).await?;

    let conn = tc.database_conn()?;
    let entry_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM entries WHERE feed_id = ?1",
        [feed_id],
        |row| row.get(0),
    )?;
    assert_eq!(entry_count, 0, "HTML response should not produce entries");

    let (error, error_at) = read_stored_error(&conn, feed_id)?;
    assert_eq!(
        error,
        Some(FetchError::InvalidFeed {
            url: feed_url.clone()
        }),
    );
    assert!(error_at.is_some());

    Ok(())
}

/// When a server returns a non-success HTTP status, the fetcher stores an
/// `HttpStatus` error.
#[tokio::test]
async fn test_http_status_error() -> Result<()> {
    use axum::http::StatusCode;

    let tc = TestBuilder::default().init_database().build()?;

    let app = Router::new().route("/feed", get(|| async { StatusCode::INTERNAL_SERVER_ERROR }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move { axum::serve(listener, app).await.ok() });

    let feed_url = format!("http://{}/feed", addr);
    let (feed_id, client, pool) = setup_feed(&tc, &feed_url)?;

    refresh_feed(&client, feed_id, pool, None, &super::test_metrics()).await?;

    let conn = tc.database_conn()?;
    let (error, error_at) = read_stored_error(&conn, feed_id)?;
    assert_eq!(
        error,
        Some(FetchError::HttpStatus {
            url: feed_url.clone(),
            status: 500,
        }),
    );
    assert!(error_at.is_some());

    Ok(())
}

/// When a server sends an endless redirect loop, the fetcher stores a
/// `TooManyRedirects` error.
#[tokio::test]
async fn test_too_many_redirects_error() -> Result<()> {
    let tc = TestBuilder::default().init_database().build()?;

    // Server that always redirects back to itself.
    let app = Router::new().route(
        "/feed",
        get(|| async {
            (
                axum::http::StatusCode::MOVED_PERMANENTLY,
                [("location", "/feed")],
                "",
            )
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move { axum::serve(listener, app).await.ok() });

    let feed_url = format!("http://{}/feed", addr);
    let (feed_id, client, pool) = setup_feed(&tc, &feed_url)?;

    refresh_feed(&client, feed_id, pool, None, &super::test_metrics()).await?;

    let conn = tc.database_conn()?;
    let (error, error_at) = read_stored_error(&conn, feed_id)?;
    assert_eq!(
        error,
        Some(FetchError::TooManyRedirects {
            url: feed_url.clone(),
        }),
    );
    assert!(error_at.is_some());

    Ok(())
}

/// After an error is stored, a successful fetch clears it.
#[tokio::test]
async fn test_successful_fetch_clears_error() -> Result<()> {
    let tc = TestBuilder::default().init_database().build()?;

    // Start with a server returning HTML (causes InvalidFeed error).
    let app = Router::new().route(
        "/feed",
        get(|| async {
            (
                [("content-type", "text/html")],
                "<html><body>Not a feed</body></html>",
            )
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move { axum::serve(listener, app).await.ok() });

    let feed_url = format!("http://{}/feed", addr);
    let (feed_id, client, pool) = setup_feed(&tc, &feed_url)?;

    refresh_feed(&client, feed_id, pool, None, &super::test_metrics()).await?;

    let conn = tc.database_conn()?;
    let (error, _) = read_stored_error(&conn, feed_id)?;
    assert!(error.is_some(), "error should be set after HTML response");

    // Now point the feed at a valid RSS source and refresh again.
    let valid_url = tc.example_feed_url();
    conn.execute(
        "UPDATE feeds SET url = ?1, last_checked = NULL, next_fetch_at = NULL WHERE id = ?2",
        rusqlite::params![valid_url, feed_id],
    )?;

    let pool2 = make_pool(&tc.database_path())?;
    refresh_feed(&client, feed_id, pool2, None, &super::test_metrics()).await?;

    let (error, error_at) = read_stored_error(&conn, feed_id)?;
    assert!(
        error.is_none(),
        "error should be cleared after successful fetch"
    );
    assert!(
        error_at.is_none(),
        "error_at should be cleared after successful fetch"
    );

    Ok(())
}

/// Read the scheduling columns for a feed.
fn read_schedule(conn: &rusqlite::Connection, feed_id: i64) -> Result<(Option<i64>, i64)> {
    let row: (Option<i64>, i64) = conn.query_row(
        "SELECT next_fetch_at, consecutive_failures FROM feeds WHERE id = ?1",
        [feed_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok(row)
}

/// A permanent 404 schedules the next fetch at `now + max_feed_backoff`.
#[tokio::test]
async fn test_permanent_404_schedules_max_backoff() -> Result<()> {
    use axum::http::StatusCode;

    let tc = TestBuilder::default().init_database().build()?;

    let app = Router::new().route("/feed", get(|| async { StatusCode::NOT_FOUND }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move { axum::serve(listener, app).await.ok() });

    let feed_url = format!("http://{}/feed", addr);
    let (feed_id, client, pool) = setup_feed(&tc, &feed_url)?;

    let before = Utc::now().timestamp();
    refresh_feed(&client, feed_id, pool, None, &super::test_metrics()).await?;
    let after = Utc::now().timestamp();

    let conn = tc.database_conn()?;
    let (next_fetch_at, failures) = read_schedule(&conn, feed_id)?;
    assert_eq!(failures, 1);
    let nf = next_fetch_at.expect("next_fetch_at should be set");
    assert!(
        nf >= before + 86_400 && nf <= after + 86_400,
        "404 is permanent; should schedule ~max_backoff out, got offset {}",
        nf - before
    );
    let (error, _) = read_stored_error(&conn, feed_id)?;
    let stored = error.expect("error should be recorded");
    assert!(!stored.is_transient());
    Ok(())
}

/// `TooManyRedirects` is permanent — scheduled at `max_feed_backoff`.
#[tokio::test]
async fn test_too_many_redirects_is_permanent() -> Result<()> {
    let tc = TestBuilder::default().init_database().build()?;

    let app = Router::new().route(
        "/feed",
        get(|| async {
            (
                axum::http::StatusCode::MOVED_PERMANENTLY,
                [("location", "/feed")],
                "",
            )
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move { axum::serve(listener, app).await.ok() });

    let feed_url = format!("http://{}/feed", addr);
    let (feed_id, client, pool) = setup_feed(&tc, &feed_url)?;

    let before = Utc::now().timestamp();
    refresh_feed(&client, feed_id, pool, None, &super::test_metrics()).await?;
    let after = Utc::now().timestamp();

    let conn = tc.database_conn()?;
    let (next_fetch_at, _) = read_schedule(&conn, feed_id)?;
    let nf = next_fetch_at.expect("next_fetch_at should be set");
    assert!(
        nf >= before + 86_400 && nf <= after + 86_400,
        "redirect loop is permanent; should schedule ~max_backoff out, got offset {}",
        nf - before
    );
    Ok(())
}

/// `InvalidFeed` is permanent — scheduled at `max_feed_backoff`.
#[tokio::test]
async fn test_invalid_feed_is_permanent() -> Result<()> {
    let tc = TestBuilder::default().init_database().build()?;

    let app = Router::new().route(
        "/feed",
        get(|| async {
            (
                [("content-type", "text/html")],
                "<html><body>Not a feed</body></html>",
            )
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move { axum::serve(listener, app).await.ok() });

    let feed_url = format!("http://{}/feed", addr);
    let (feed_id, client, pool) = setup_feed(&tc, &feed_url)?;

    let before = Utc::now().timestamp();
    refresh_feed(&client, feed_id, pool, None, &super::test_metrics()).await?;
    let after = Utc::now().timestamp();

    let conn = tc.database_conn()?;
    let (next_fetch_at, _) = read_schedule(&conn, feed_id)?;
    let nf = next_fetch_at.expect("next_fetch_at should be set");
    assert!(
        nf >= before + 86_400 && nf <= after + 86_400,
        "invalid feed is permanent; should schedule ~max_backoff out, got offset {}",
        nf - before
    );
    Ok(())
}

/// A network failure (connecting to a closed port) is transient and
/// triggers exponential backoff.
#[tokio::test]
async fn test_network_error_transient_with_backoff() -> Result<()> {
    let tc = TestBuilder::default().init_database().build()?;

    // Reserve and then drop a listener so the port is very likely closed
    // for the duration of this test.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    drop(listener);

    let feed_url = format!("http://{}/feed", addr);
    let (feed_id, client, pool) = setup_feed(&tc, &feed_url)?;

    let before = Utc::now().timestamp();
    refresh_feed(&client, feed_id, pool, None, &super::test_metrics()).await?;
    let after = Utc::now().timestamp();

    let conn = tc.database_conn()?;
    let (next_fetch_at, failures) = read_schedule(&conn, feed_id)?;
    assert_eq!(failures, 1);
    let nf = next_fetch_at.expect("next_fetch_at should be set");
    assert!(
        nf >= before + 60 && nf <= after + 60,
        "first transient failure should schedule ~60s out, got offset {}",
        nf - before
    );
    let (error, _) = read_stored_error(&conn, feed_id)?;
    let stored = error.expect("error should be recorded");
    assert!(stored.is_transient());
    Ok(())
}
