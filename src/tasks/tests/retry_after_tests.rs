#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::super::*;
use crate::test::TestBuilder;
use anyhow::Result;
use axum::{
    http::{HeaderMap, HeaderValue, StatusCode},
    response::IntoResponse,
    routing::get,
    Router,
};
use rusqlite::OpenFlags;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

fn make_pool(path: &std::path::Path) -> Result<r2d2::Pool<SqliteConnectionManager>> {
    let manager = SqliteConnectionManager::file(path)
        .with_flags(OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .with_init(|c| c.execute_batch("PRAGMA foreign_keys=ON;"));
    Ok(r2d2::Pool::new(manager)?)
}

fn setup_feed(
    tc: &crate::test::TestConfig,
    feed_url: &str,
) -> Result<(i64, reqwest::Client, r2d2::Pool<SqliteConnectionManager>)> {
    let conn = tc.database_conn()?;
    conn.execute(
        "INSERT INTO feeds (title, url) VALUES ('retry-after test feed', ?1)",
        [feed_url],
    )?;
    let feed_id = conn.last_insert_rowid();

    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(crate::http::USER_AGENT)
        .build()?;
    let pool = make_pool(&tc.database_path())?;
    Ok((feed_id, client, pool))
}

fn read_schedule(
    conn: &rusqlite::Connection,
    feed_id: i64,
) -> Result<(Option<i64>, Option<i64>, i64)> {
    let row: (Option<i64>, Option<i64>, i64) = conn.query_row(
        "SELECT next_fetch_at, retry_after_at, consecutive_failures FROM feeds WHERE id = ?1",
        [feed_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    Ok(row)
}

/// Spawn an axum server that replies with a fixed status and optional
/// Retry-After header. Returns the feed URL.
async fn spawn_retry_after_server(status: StatusCode, retry_after: Option<&str>) -> Result<String> {
    let retry_after = retry_after.map(|s| s.to_string());
    let app = Router::new().route(
        "/feed",
        get(move || {
            let retry_after = retry_after.clone();
            async move {
                let mut headers = HeaderMap::new();
                if let Some(v) = retry_after {
                    headers.insert("retry-after", HeaderValue::from_str(&v).unwrap());
                }
                (status, headers).into_response()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move { axum::serve(listener, app).await.ok() });
    Ok(format!("http://{}/feed", addr))
}

// ----------------- pure parser tests -----------------

#[test]
fn test_parse_retry_after_delta_seconds() {
    let now = Utc::now();
    let ts = parse_retry_after("120", now).expect("should parse delta-seconds");
    assert_eq!(ts, now.timestamp() + 120);
}

#[test]
fn test_parse_retry_after_http_date() {
    let now = Utc::now();
    let expected = chrono::NaiveDate::from_ymd_opt(2099, 10, 21)
        .unwrap()
        .and_hms_opt(7, 28, 0)
        .unwrap()
        .and_utc()
        .timestamp();
    let ts =
        parse_retry_after("Wed, 21 Oct 2099 07:28:00 GMT", now).expect("should parse IMF-fixdate");
    assert_eq!(ts, expected);
}

#[test]
fn test_parse_retry_after_invalid_returns_none() {
    let now = Utc::now();
    assert!(parse_retry_after("not a real value", now).is_none());
    assert!(parse_retry_after("", now).is_none());
}

// ----------------- end-to-end tests -----------------

/// A 429 with numeric `Retry-After` schedules the next fetch at now+delta.
#[tokio::test]
async fn test_429_schedules_from_retry_after() -> Result<()> {
    let tc = TestBuilder::default().init_database().build()?;
    let feed_url = spawn_retry_after_server(StatusCode::TOO_MANY_REQUESTS, Some("300")).await?;
    let (feed_id, client, pool) = setup_feed(&tc, &feed_url)?;

    let before = Utc::now().timestamp();
    refresh_feed(&client, feed_id, pool, None, &super::test_metrics()).await?;

    let conn = tc.database_conn()?;
    let (next_fetch_at, retry_after_at, failures) = read_schedule(&conn, feed_id)?;
    let after = Utc::now().timestamp();

    let ra = retry_after_at.expect("retry_after_at should be set");
    assert!(ra >= before + 300 && ra <= after + 300);
    let nf = next_fetch_at.expect("next_fetch_at should be set");
    assert!(nf >= before + 300 && nf <= after + 300);
    assert_eq!(failures, 1);

    let stored_error: Option<String> = conn.query_row(
        "SELECT last_fetch_error FROM feeds WHERE id = ?1",
        [feed_id],
        |row| row.get(0),
    )?;
    assert!(stored_error.is_some());
    Ok(())
}

/// A 503 with an HTTP-date `Retry-After` is honored.
#[tokio::test]
async fn test_503_honors_retry_after_http_date() -> Result<()> {
    let tc = TestBuilder::default().init_database().build()?;
    // Use a far-future absolute time so the test is robust to clock skew.
    let http_date = (Utc::now() + chrono::Duration::hours(2))
        .format("%a, %d %b %Y %H:%M:%S GMT")
        .to_string();
    let feed_url =
        spawn_retry_after_server(StatusCode::SERVICE_UNAVAILABLE, Some(&http_date)).await?;
    let (feed_id, client, pool) = setup_feed(&tc, &feed_url)?;

    let before = Utc::now().timestamp();
    refresh_feed(&client, feed_id, pool, None, &super::test_metrics()).await?;

    let conn = tc.database_conn()?;
    let (next_fetch_at, retry_after_at, _) = read_schedule(&conn, feed_id)?;
    let nf = next_fetch_at.expect("next_fetch_at should be set");
    let ra = retry_after_at.expect("retry_after_at should be set");
    // Should be roughly 2 hours in the future (±1 minute for test timing).
    assert!(nf >= before + 7_140 && nf <= before + 7_260);
    assert!(ra >= before + 7_140 && ra <= before + 7_260);
    Ok(())
}

/// `Retry-After: 5` is below the 60s floor and must be clamped.
#[tokio::test]
async fn test_retry_after_below_min_cadence_floored() -> Result<()> {
    let tc = TestBuilder::default().init_database().build()?;
    let feed_url = spawn_retry_after_server(StatusCode::TOO_MANY_REQUESTS, Some("5")).await?;
    let (feed_id, client, pool) = setup_feed(&tc, &feed_url)?;

    let before = Utc::now().timestamp();
    refresh_feed(&client, feed_id, pool, None, &super::test_metrics()).await?;

    let conn = tc.database_conn()?;
    let (next_fetch_at, _, _) = read_schedule(&conn, feed_id)?;
    let after = Utc::now().timestamp();

    let nf = next_fetch_at.expect("next_fetch_at should be set");
    assert!(
        nf >= before + 60 && nf <= after + 60,
        "next_fetch_at should be floored to now+60s, got offset {}",
        nf - before
    );
    Ok(())
}

/// `Retry-After` beyond `max_feed_backoff_seconds` (default 24h) is
/// capped.
#[tokio::test]
async fn test_retry_after_above_max_backoff_capped() -> Result<()> {
    let tc = TestBuilder::default().init_database().build()?;
    // 48 hours in seconds, well above the default 86400 ceiling.
    let feed_url = spawn_retry_after_server(StatusCode::TOO_MANY_REQUESTS, Some("172800")).await?;
    let (feed_id, client, pool) = setup_feed(&tc, &feed_url)?;

    let before = Utc::now().timestamp();
    refresh_feed(&client, feed_id, pool, None, &super::test_metrics()).await?;

    let conn = tc.database_conn()?;
    let (next_fetch_at, _, _) = read_schedule(&conn, feed_id)?;
    let after = Utc::now().timestamp();

    let nf = next_fetch_at.expect("next_fetch_at should be set");
    assert!(
        nf >= before + 86_400 && nf <= after + 86_400,
        "next_fetch_at should be capped at now+max_backoff (86400s), got offset {}",
        nf - before
    );
    Ok(())
}

/// A 429 without `Retry-After` falls back to exponential backoff.
#[tokio::test]
async fn test_429_without_retry_after_uses_backoff() -> Result<()> {
    let tc = TestBuilder::default().init_database().build()?;
    let feed_url = spawn_retry_after_server(StatusCode::TOO_MANY_REQUESTS, None).await?;
    let (feed_id, client, pool) = setup_feed(&tc, &feed_url)?;

    let before = Utc::now().timestamp();
    refresh_feed(&client, feed_id, pool, None, &super::test_metrics()).await?;

    let conn = tc.database_conn()?;
    let (next_fetch_at, retry_after_at, failures) = read_schedule(&conn, feed_id)?;
    let after = Utc::now().timestamp();

    assert!(retry_after_at.is_none());
    assert_eq!(failures, 1);
    let nf = next_fetch_at.expect("next_fetch_at should be set");
    // First failure: backoff = min_cadence * 2^0 = 60s.
    assert!(
        nf >= before + 60 && nf <= after + 60,
        "first transient failure should schedule at now+60s, got offset {}",
        nf - before
    );
    Ok(())
}

/// A counter-backed mock server lets us verify the second attempt doubles
/// the backoff window.
#[tokio::test]
async fn test_consecutive_500s_schedule_exponential_backoff() -> Result<()> {
    let tc = TestBuilder::default().init_database().build()?;

    let hits = Arc::new(AtomicUsize::new(0));
    let hits_clone = hits.clone();
    let app = Router::new().route(
        "/feed",
        get(move || {
            let hits = hits_clone.clone();
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                StatusCode::INTERNAL_SERVER_ERROR
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move { axum::serve(listener, app).await.ok() });
    let feed_url = format!("http://{}/feed", addr);

    let (feed_id, client, pool) = setup_feed(&tc, &feed_url)?;

    // First attempt: records one transient failure.
    let t1 = Utc::now().timestamp();
    refresh_feed(&client, feed_id, pool.clone(), None, &super::test_metrics()).await?;

    let conn = tc.database_conn()?;
    let (next_after_1, _, failures_1) = read_schedule(&conn, feed_id)?;
    assert_eq!(failures_1, 1);
    let nf1 = next_after_1.unwrap();
    assert!(
        nf1 >= t1 + 60 && nf1 <= t1 + 61,
        "first backoff should be ~60s from now, got offset {}",
        nf1 - t1
    );

    // Clear the gate so the next refresh is eligible, without resetting
    // the failure counter (which is the whole point — we want to verify
    // the counter drives the next backoff).
    conn.execute(
        "UPDATE feeds SET next_fetch_at = NULL WHERE id = ?1",
        [feed_id],
    )?;

    // Second attempt: records another transient failure. Backoff now
    // uses consecutive_failures = 2, so schedule should be ~120s out.
    let t2 = Utc::now().timestamp();
    refresh_feed(&client, feed_id, pool, None, &super::test_metrics()).await?;

    let (next_after_2, _, failures_2) = read_schedule(&conn, feed_id)?;
    assert_eq!(failures_2, 2);
    let nf2 = next_after_2.unwrap();
    assert!(
        nf2 >= t2 + 120 && nf2 <= t2 + 121,
        "second backoff should be ~120s from now, got offset {}",
        nf2 - t2
    );

    assert_eq!(hits.load(Ordering::SeqCst), 2);
    Ok(())
}
