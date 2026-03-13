#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::super::*;
use crate::test::{FeedServerState, SharedFeedServerState, TestBuilder};
use anyhow::Result;
use rusqlite::OpenFlags;
use std::sync::{Arc, Mutex};

fn make_pool(path: &std::path::Path) -> Result<r2d2::Pool<SqliteConnectionManager>> {
    let manager = SqliteConnectionManager::file(path)
        .with_flags(OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .with_init(|c| c.execute_batch("PRAGMA foreign_keys=ON;"));
    Ok(r2d2::Pool::new(manager)?)
}

/// Insert a feed pointing at the test feed server's RSS URL and return
/// the feed id, an HTTP client, and a connection pool.
async fn setup_feed_for_cache_test(
    tc: &crate::test::TestConfig,
) -> Result<(i64, reqwest::Client, r2d2::Pool<SqliteConnectionManager>)> {
    let conn = tc.database_conn()?;
    conn.execute(
        "INSERT INTO feeds (title, url) VALUES ('cache test feed', ?1)",
        [tc.rss_feed_url()],
    )?;
    let feed_id = conn.last_insert_rowid();

    let client = reqwest::Client::builder()
        .user_agent(crate::http::USER_AGENT)
        .build()?;
    let pool = make_pool(&tc.database_path())?;
    Ok((feed_id, client, pool))
}

/// Reset `last_checked` to 4 hours ago so the 3-hour throttle does not
/// block the next call to `refresh_feed`.
fn reset_last_checked(conn: &rusqlite::Connection, feed_id: i64) {
    conn.execute(
        "UPDATE feeds SET last_checked = ?1 WHERE id = ?2",
        rusqlite::params![Utc::now().timestamp() - 4 * 3600, feed_id],
    )
    .unwrap();
}

/// Server sends ETag → stored in DB → second request sends If-None-Match → gets 304.
#[tokio::test]
async fn test_etag_stored_and_sent() -> Result<()> {
    let mut tc = TestBuilder::default().init_database().build()?;
    let state: SharedFeedServerState = Arc::new(Mutex::new(FeedServerState {
        etag: Some("\"abc123\"".into()),
        ..Default::default()
    }));
    tc.init_feed_server_with_state(state.clone()).await?;

    let (feed_id, client, pool) = setup_feed_for_cache_test(&tc).await?;

    // First fetch: should get 200, store the etag
    refresh_feed(&client, feed_id, pool.clone(), None).await?;

    let conn = tc.database_conn()?;
    let stored_etag: Option<String> = conn.query_row(
        "SELECT header_etag FROM feeds WHERE id = ?1",
        [feed_id],
        |row| row.get(0),
    )?;
    assert_eq!(stored_etag.as_deref(), Some("\"abc123\""));

    // Reset last_checked so the throttle doesn't block us
    reset_last_checked(&conn, feed_id);

    // Second fetch: should send If-None-Match and get 304
    refresh_feed(&client, feed_id, pool, None).await?;

    let s = state.lock().unwrap();
    assert_eq!(
        s.full_response_count, 1,
        "only the first request should get a full response"
    );
    assert_eq!(s.not_modified_count, 1, "second request should get 304");

    Ok(())
}

/// Server sends Last-Modified → stored in DB → second request sends
/// If-Modified-Since → gets 304.
#[tokio::test]
async fn test_last_modified_stored_and_sent() -> Result<()> {
    let mut tc = TestBuilder::default().init_database().build()?;
    let state: SharedFeedServerState = Arc::new(Mutex::new(FeedServerState {
        last_modified: Some("Sat, 01 Jan 2025 00:00:00 GMT".into()),
        ..Default::default()
    }));
    tc.init_feed_server_with_state(state.clone()).await?;

    let (feed_id, client, pool) = setup_feed_for_cache_test(&tc).await?;

    // First fetch: 200, stores Last-Modified
    refresh_feed(&client, feed_id, pool.clone(), None).await?;

    let conn = tc.database_conn()?;
    let stored_lm: Option<String> = conn.query_row(
        "SELECT header_last_modified FROM feeds WHERE id = ?1",
        [feed_id],
        |row| row.get(0),
    )?;
    assert_eq!(stored_lm.as_deref(), Some("Sat, 01 Jan 2025 00:00:00 GMT"));

    reset_last_checked(&conn, feed_id);

    // Second fetch: should send If-Modified-Since and get 304
    refresh_feed(&client, feed_id, pool, None).await?;

    let s = state.lock().unwrap();
    assert_eq!(s.full_response_count, 1);
    assert_eq!(s.not_modified_count, 1);

    Ok(())
}

/// A future Expires header causes refresh_feed to skip the HTTP request entirely.
#[tokio::test]
async fn test_expires_skips_fetch() -> Result<()> {
    let future_expires = (Utc::now() + chrono::Duration::hours(1))
        .format("%a, %d %b %Y %H:%M:%S GMT")
        .to_string();

    let mut tc = TestBuilder::default().init_database().build()?;
    let state: SharedFeedServerState = Arc::new(Mutex::new(FeedServerState {
        expires: Some(future_expires),
        ..Default::default()
    }));
    tc.init_feed_server_with_state(state.clone()).await?;

    let (feed_id, client, pool) = setup_feed_for_cache_test(&tc).await?;

    // First fetch: 200, stores Expires timestamp
    refresh_feed(&client, feed_id, pool.clone(), None).await?;

    let conn = tc.database_conn()?;
    let stored_expires: Option<i64> = conn.query_row(
        "SELECT header_expires FROM feeds WHERE id = ?1",
        [feed_id],
        |row| row.get(0),
    )?;
    assert!(stored_expires.is_some(), "expires should be stored in DB");

    reset_last_checked(&conn, feed_id);

    // Second call: should skip entirely due to unexpired Expires header
    refresh_feed(&client, feed_id, pool, None).await?;

    let s = state.lock().unwrap();
    assert_eq!(
        s.request_count, 1,
        "only one HTTP request should have been made; second should be skipped"
    );

    Ok(())
}

/// Once the Expires time has passed, the fetcher makes a new request.
#[tokio::test]
async fn test_expired_expires_allows_fetch() -> Result<()> {
    let past_expires = (Utc::now() - chrono::Duration::seconds(1))
        .format("%a, %d %b %Y %H:%M:%S GMT")
        .to_string();

    let mut tc = TestBuilder::default().init_database().build()?;
    let state: SharedFeedServerState = Arc::new(Mutex::new(FeedServerState {
        expires: Some(past_expires),
        ..Default::default()
    }));
    tc.init_feed_server_with_state(state.clone()).await?;

    let (feed_id, client, pool) = setup_feed_for_cache_test(&tc).await?;

    // First fetch: 200, stores the already-past Expires timestamp
    refresh_feed(&client, feed_id, pool.clone(), None).await?;

    let conn = tc.database_conn()?;
    reset_last_checked(&conn, feed_id);

    // Second call: expires is in the past, so a new HTTP request should be made
    refresh_feed(&client, feed_id, pool, None).await?;

    let s = state.lock().unwrap();
    assert_eq!(
        s.request_count, 2,
        "both calls should have made HTTP requests since Expires is in the past"
    );

    Ok(())
}

/// On 304 Not Modified, `last_checked` is updated but no new entries are inserted.
#[tokio::test]
async fn test_304_updates_last_checked_only() -> Result<()> {
    let mut tc = TestBuilder::default().init_database().build()?;
    let state: SharedFeedServerState = Arc::new(Mutex::new(FeedServerState {
        etag: Some("\"check304\"".into()),
        ..Default::default()
    }));
    tc.init_feed_server_with_state(state.clone()).await?;

    let (feed_id, client, pool) = setup_feed_for_cache_test(&tc).await?;

    // First fetch: inserts entries
    refresh_feed(&client, feed_id, pool.clone(), None).await?;

    let conn = tc.database_conn()?;
    let entry_count_after_first: i64 = conn.query_row(
        "SELECT COUNT(*) FROM entries WHERE feed_id = ?1",
        [feed_id],
        |row| row.get(0),
    )?;
    assert!(
        entry_count_after_first > 0,
        "first fetch should insert entries"
    );

    reset_last_checked(&conn, feed_id);

    // Second fetch: 304, should not change entry count
    refresh_feed(&client, feed_id, pool, None).await?;

    let entry_count_after_second: i64 = conn.query_row(
        "SELECT COUNT(*) FROM entries WHERE feed_id = ?1",
        [feed_id],
        |row| row.get(0),
    )?;
    assert_eq!(
        entry_count_after_first, entry_count_after_second,
        "304 should not insert new entries"
    );

    // last_checked should be recent (within the last 10 seconds)
    let last_checked: i64 = conn.query_row(
        "SELECT last_checked FROM feeds WHERE id = ?1",
        [feed_id],
        |row| row.get(0),
    )?;
    let now = Utc::now().timestamp();
    assert!(
        now - last_checked < 10,
        "last_checked should have been updated to a recent time"
    );

    Ok(())
}

/// When both etag and last_modified are stored, both conditional headers
/// are sent on the next request.
#[tokio::test]
async fn test_etag_and_last_modified_both_sent() -> Result<()> {
    let mut tc = TestBuilder::default().init_database().build()?;
    let state: SharedFeedServerState = Arc::new(Mutex::new(FeedServerState {
        etag: Some("\"both-test\"".into()),
        last_modified: Some("Sun, 02 Feb 2025 12:00:00 GMT".into()),
        ..Default::default()
    }));
    tc.init_feed_server_with_state(state.clone()).await?;

    let (feed_id, client, pool) = setup_feed_for_cache_test(&tc).await?;

    // First fetch: stores both headers
    refresh_feed(&client, feed_id, pool.clone(), None).await?;

    let conn = tc.database_conn()?;
    let (stored_etag, stored_lm): (Option<String>, Option<String>) = conn.query_row(
        "SELECT header_etag, header_last_modified FROM feeds WHERE id = ?1",
        [feed_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    assert_eq!(stored_etag.as_deref(), Some("\"both-test\""));
    assert_eq!(stored_lm.as_deref(), Some("Sun, 02 Feb 2025 12:00:00 GMT"));

    reset_last_checked(&conn, feed_id);

    // Second fetch: both conditional headers sent, server returns 304
    refresh_feed(&client, feed_id, pool, None).await?;

    let s = state.lock().unwrap();
    assert_eq!(s.full_response_count, 1);
    assert_eq!(s.not_modified_count, 1);
    assert_eq!(s.request_count, 2);

    Ok(())
}

/// Cache-Control: max-age=3600 stores header_expires ≈ now+3600 and skips
/// the second fetch.
#[tokio::test]
async fn test_max_age_sets_expires() -> Result<()> {
    let mut tc = TestBuilder::default().init_database().build()?;
    let state: SharedFeedServerState = Arc::new(Mutex::new(FeedServerState {
        cache_control: Some("max-age=3600".into()),
        ..Default::default()
    }));
    tc.init_feed_server_with_state(state.clone()).await?;

    let (feed_id, client, pool) = setup_feed_for_cache_test(&tc).await?;
    let before = Utc::now().timestamp();

    refresh_feed(&client, feed_id, pool.clone(), None).await?;

    let conn = tc.database_conn()?;
    let stored_expires: Option<i64> = conn.query_row(
        "SELECT header_expires FROM feeds WHERE id = ?1",
        [feed_id],
        |row| row.get(0),
    )?;
    let expires = stored_expires.expect("header_expires should be set");
    assert!(
        expires >= before + 3600 && expires <= before + 3600 + 5,
        "header_expires should be approximately now + 3600, got offset {}",
        expires - before
    );

    reset_last_checked(&conn, feed_id);

    // Second fetch: should be skipped because max-age hasn't expired
    refresh_feed(&client, feed_id, pool, None).await?;

    let s = state.lock().unwrap();
    assert_eq!(
        s.request_count, 1,
        "second fetch should be skipped due to max-age"
    );

    Ok(())
}

/// Cache-Control: max-age takes precedence over a past Expires header.
#[tokio::test]
async fn test_max_age_overrides_expires() -> Result<()> {
    let past_expires = (Utc::now() - chrono::Duration::seconds(60))
        .format("%a, %d %b %Y %H:%M:%S GMT")
        .to_string();

    let mut tc = TestBuilder::default().init_database().build()?;
    let state: SharedFeedServerState = Arc::new(Mutex::new(FeedServerState {
        expires: Some(past_expires),
        cache_control: Some("max-age=3600".into()),
        ..Default::default()
    }));
    tc.init_feed_server_with_state(state.clone()).await?;

    let (feed_id, client, pool) = setup_feed_for_cache_test(&tc).await?;
    let before = Utc::now().timestamp();

    refresh_feed(&client, feed_id, pool.clone(), None).await?;

    let conn = tc.database_conn()?;
    let stored_expires: Option<i64> = conn.query_row(
        "SELECT header_expires FROM feeds WHERE id = ?1",
        [feed_id],
        |row| row.get(0),
    )?;
    let expires = stored_expires.expect("header_expires should be set");
    // max-age should win over the past Expires header
    assert!(
        expires >= before + 3600,
        "max-age should override past Expires; got {} which is only {} from now",
        expires,
        expires - before
    );

    Ok(())
}

/// Cache-Control: no-cache clears header_expires but preserves ETag for
/// conditional requests.
#[tokio::test]
async fn test_no_cache_clears_expires() -> Result<()> {
    let mut tc = TestBuilder::default().init_database().build()?;
    let state: SharedFeedServerState = Arc::new(Mutex::new(FeedServerState {
        etag: Some("\"no-cache-test\"".into()),
        cache_control: Some("no-cache".into()),
        ..Default::default()
    }));
    tc.init_feed_server_with_state(state.clone()).await?;

    let (feed_id, client, pool) = setup_feed_for_cache_test(&tc).await?;

    refresh_feed(&client, feed_id, pool.clone(), None).await?;

    let conn = tc.database_conn()?;
    let (stored_etag, stored_expires): (Option<String>, Option<i64>) = conn.query_row(
        "SELECT header_etag, header_expires FROM feeds WHERE id = ?1",
        [feed_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    assert_eq!(
        stored_etag.as_deref(),
        Some("\"no-cache-test\""),
        "ETag should be preserved with no-cache"
    );
    assert!(
        stored_expires.is_none(),
        "header_expires should be NULL with no-cache"
    );

    reset_last_checked(&conn, feed_id);

    // Second fetch: should make an HTTP request (no skip) and get 304
    refresh_feed(&client, feed_id, pool, None).await?;

    let s = state.lock().unwrap();
    assert_eq!(s.full_response_count, 1);
    assert_eq!(
        s.not_modified_count, 1,
        "conditional request should get 304"
    );

    Ok(())
}

/// Cache-Control: no-store clears all cache headers.
#[tokio::test]
async fn test_no_store_clears_all_cache_headers() -> Result<()> {
    let mut tc = TestBuilder::default().init_database().build()?;
    let state: SharedFeedServerState = Arc::new(Mutex::new(FeedServerState {
        etag: Some("\"no-store-test\"".into()),
        last_modified: Some("Sat, 01 Jan 2025 00:00:00 GMT".into()),
        cache_control: Some("no-store".into()),
        ..Default::default()
    }));
    tc.init_feed_server_with_state(state.clone()).await?;

    let (feed_id, client, pool) = setup_feed_for_cache_test(&tc).await?;

    refresh_feed(&client, feed_id, pool, None).await?;

    let conn = tc.database_conn()?;
    let (stored_etag, stored_lm, stored_expires): (Option<String>, Option<String>, Option<i64>) =
        conn.query_row(
            "SELECT header_etag, header_last_modified, header_expires FROM feeds WHERE id = ?1",
            [feed_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
    assert!(
        stored_etag.is_none(),
        "etag should be cleared with no-store"
    );
    assert!(
        stored_lm.is_none(),
        "last_modified should be cleared with no-store"
    );
    assert!(
        stored_expires.is_none(),
        "expires should be cleared with no-store"
    );

    Ok(())
}

/// After no-store clears headers, the second fetch sends no conditional
/// headers, resulting in two full 200 responses.
#[tokio::test]
async fn test_no_store_prevents_conditional_request() -> Result<()> {
    let mut tc = TestBuilder::default().init_database().build()?;
    let state: SharedFeedServerState = Arc::new(Mutex::new(FeedServerState {
        etag: Some("\"no-store-cond\"".into()),
        cache_control: Some("no-store".into()),
        ..Default::default()
    }));
    tc.init_feed_server_with_state(state.clone()).await?;

    let (feed_id, client, pool) = setup_feed_for_cache_test(&tc).await?;

    // First fetch: 200 (no-store clears stored etag)
    refresh_feed(&client, feed_id, pool.clone(), None).await?;

    let conn = tc.database_conn()?;
    reset_last_checked(&conn, feed_id);

    // Second fetch: no conditional headers sent, so another 200
    refresh_feed(&client, feed_id, pool, None).await?;

    let s = state.lock().unwrap();
    assert_eq!(
        s.full_response_count, 2,
        "both requests should get full 200 responses"
    );
    assert_eq!(
        s.not_modified_count, 0,
        "no 304 should occur since no-store cleared conditional headers"
    );

    Ok(())
}

/// A gzip-compressed response is transparently decompressed by reqwest,
/// and the feed entries are correctly parsed and inserted.
#[tokio::test]
async fn test_gzip_compressed_response() -> Result<()> {
    let mut tc = TestBuilder::default().init_database().build()?;
    let state: SharedFeedServerState = Arc::new(Mutex::new(FeedServerState {
        content_encoding: Some("gzip".into()),
        ..Default::default()
    }));
    tc.init_feed_server_with_state(state.clone()).await?;

    let (feed_id, client, pool) = setup_feed_for_cache_test(&tc).await?;

    refresh_feed(&client, feed_id, pool, None).await?;

    let conn = tc.database_conn()?;
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM entries WHERE feed_id = ?1",
        [feed_id],
        |row| row.get(0),
    )?;
    assert!(
        count > 0,
        "entries should be inserted from gzip-compressed response"
    );

    let s = state.lock().unwrap();
    assert_eq!(s.full_response_count, 1);

    Ok(())
}

/// A deflate-compressed response is transparently decompressed by reqwest,
/// and the feed entries are correctly parsed and inserted.
#[tokio::test]
async fn test_deflate_compressed_response() -> Result<()> {
    let mut tc = TestBuilder::default().init_database().build()?;
    let state: SharedFeedServerState = Arc::new(Mutex::new(FeedServerState {
        content_encoding: Some("deflate".into()),
        ..Default::default()
    }));
    tc.init_feed_server_with_state(state.clone()).await?;

    let (feed_id, client, pool) = setup_feed_for_cache_test(&tc).await?;

    refresh_feed(&client, feed_id, pool, None).await?;

    let conn = tc.database_conn()?;
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM entries WHERE feed_id = ?1",
        [feed_id],
        |row| row.get(0),
    )?;
    assert!(
        count > 0,
        "entries should be inserted from deflate-compressed response"
    );

    let s = state.lock().unwrap();
    assert_eq!(s.full_response_count, 1);

    Ok(())
}
