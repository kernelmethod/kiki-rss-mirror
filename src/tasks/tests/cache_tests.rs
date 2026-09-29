#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::super::*;
use crate::test::{FeedServerState, SharedFeedServerState, TestBuilder};
use anyhow::Result;
use chrono::Utc;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::OpenFlags;
use std::sync::{Arc, Mutex};

fn make_pool(path: &std::path::Path) -> Result<crate::db::Pool> {
    let manager = SqliteConnectionManager::file(path)
        .with_flags(OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .with_init(|c| c.execute_batch("PRAGMA foreign_keys=ON;"));
    Ok(r2d2::Pool::new(manager.into())?)
}

/// Insert a feed pointing at the test feed server's RSS URL and return
/// the feed id, an HTTP client, and a connection pool.
async fn setup_feed_for_cache_test(
    tc: &crate::test::TestConfig,
) -> Result<(i64, reqwest::Client, crate::db::Pool)> {
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

/// Reset the scheduler so the next call to `refresh_feed` is immediately
/// eligible: clears `next_fetch_at` and backdates `last_checked` past any
/// interval that might still matter.
fn reset_last_checked(conn: &rusqlite::Connection, feed_id: i64) {
    conn.execute(
        "UPDATE feeds SET last_checked = ?1, next_fetch_at = NULL WHERE id = ?2",
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
    refresh_feed(
        &client,
        feed_id,
        pool.clone(),
        None,
        &super::test_metrics(),
        &super::test_tx(),
    )
    .await?;

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
    refresh_feed(
        &client,
        feed_id,
        pool,
        None,
        &super::test_metrics(),
        &super::test_tx(),
    )
    .await?;

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
    refresh_feed(
        &client,
        feed_id,
        pool.clone(),
        None,
        &super::test_metrics(),
        &super::test_tx(),
    )
    .await?;

    let conn = tc.database_conn()?;
    let stored_lm: Option<String> = conn.query_row(
        "SELECT header_last_modified FROM feeds WHERE id = ?1",
        [feed_id],
        |row| row.get(0),
    )?;
    assert_eq!(stored_lm.as_deref(), Some("Sat, 01 Jan 2025 00:00:00 GMT"));

    reset_last_checked(&conn, feed_id);

    // Second fetch: should send If-Modified-Since and get 304
    refresh_feed(
        &client,
        feed_id,
        pool,
        None,
        &super::test_metrics(),
        &super::test_tx(),
    )
    .await?;

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
    refresh_feed(
        &client,
        feed_id,
        pool.clone(),
        None,
        &super::test_metrics(),
        &super::test_tx(),
    )
    .await?;

    let conn = tc.database_conn()?;
    let stored_expires: Option<i64> = conn.query_row(
        "SELECT header_expires FROM feeds WHERE id = ?1",
        [feed_id],
        |row| row.get(0),
    )?;
    assert!(stored_expires.is_some(), "expires should be stored in DB");

    // Second call: should skip entirely because `next_fetch_at` was set
    // from the Expires hint. Intentionally no `reset_last_checked` — the
    // scheduler gate must persist a server's freshness guarantee.
    refresh_feed(
        &client,
        feed_id,
        pool,
        None,
        &super::test_metrics(),
        &super::test_tx(),
    )
    .await?;

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
    refresh_feed(
        &client,
        feed_id,
        pool.clone(),
        None,
        &super::test_metrics(),
        &super::test_tx(),
    )
    .await?;

    let conn = tc.database_conn()?;
    reset_last_checked(&conn, feed_id);

    // Second call: expires is in the past, so a new HTTP request should be made
    refresh_feed(
        &client,
        feed_id,
        pool,
        None,
        &super::test_metrics(),
        &super::test_tx(),
    )
    .await?;

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
    refresh_feed(
        &client,
        feed_id,
        pool.clone(),
        None,
        &super::test_metrics(),
        &super::test_tx(),
    )
    .await?;

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
    refresh_feed(
        &client,
        feed_id,
        pool,
        None,
        &super::test_metrics(),
        &super::test_tx(),
    )
    .await?;

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
    refresh_feed(
        &client,
        feed_id,
        pool.clone(),
        None,
        &super::test_metrics(),
        &super::test_tx(),
    )
    .await?;

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
    refresh_feed(
        &client,
        feed_id,
        pool,
        None,
        &super::test_metrics(),
        &super::test_tx(),
    )
    .await?;

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

    refresh_feed(
        &client,
        feed_id,
        pool.clone(),
        None,
        &super::test_metrics(),
        &super::test_tx(),
    )
    .await?;

    let conn = tc.database_conn()?;
    let stored_expires: Option<i64> = conn.query_row(
        "SELECT header_expires FROM feeds WHERE id = ?1",
        [feed_id],
        |row| row.get(0),
    )?;
    let expires = stored_expires.expect("header_expires should be set");
    // Allow a small slack below the lower bound: corrected_max_age (RFC 9111
    // §4.2.3) may subtract up to one second based on the response's auto-
    // generated Date header rounding to the previous second.
    assert!(
        expires >= before + 3595 && expires <= before + 3600 + 5,
        "header_expires should be approximately now + 3600, got offset {}",
        expires - before
    );

    // Second fetch: should be skipped because max-age sets `next_fetch_at`
    // into the future. Intentionally no `reset_last_checked`.
    refresh_feed(
        &client,
        feed_id,
        pool,
        None,
        &super::test_metrics(),
        &super::test_tx(),
    )
    .await?;

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

    refresh_feed(
        &client,
        feed_id,
        pool.clone(),
        None,
        &super::test_metrics(),
        &super::test_tx(),
    )
    .await?;

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

    refresh_feed(
        &client,
        feed_id,
        pool.clone(),
        None,
        &super::test_metrics(),
        &super::test_tx(),
    )
    .await?;

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
    refresh_feed(
        &client,
        feed_id,
        pool,
        None,
        &super::test_metrics(),
        &super::test_tx(),
    )
    .await?;

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

    refresh_feed(
        &client,
        feed_id,
        pool,
        None,
        &super::test_metrics(),
        &super::test_tx(),
    )
    .await?;

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
    refresh_feed(
        &client,
        feed_id,
        pool.clone(),
        None,
        &super::test_metrics(),
        &super::test_tx(),
    )
    .await?;

    let conn = tc.database_conn()?;
    reset_last_checked(&conn, feed_id);

    // Second fetch: no conditional headers sent, so another 200
    refresh_feed(
        &client,
        feed_id,
        pool,
        None,
        &super::test_metrics(),
        &super::test_tx(),
    )
    .await?;

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

    refresh_feed(
        &client,
        feed_id,
        pool,
        None,
        &super::test_metrics(),
        &super::test_tx(),
    )
    .await?;

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

    refresh_feed(
        &client,
        feed_id,
        pool,
        None,
        &super::test_metrics(),
        &super::test_tx(),
    )
    .await?;

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

/// A `next_fetch_at` value in the future skips the HTTP request entirely
/// and records the `next_fetch_at` cache-hit reason.
#[tokio::test]
async fn test_next_fetch_at_gate_skips_fetch() -> Result<()> {
    let mut tc = TestBuilder::default().init_database().build()?;
    let state: SharedFeedServerState = Arc::new(Mutex::new(FeedServerState::default()));
    tc.init_feed_server_with_state(state.clone()).await?;

    let (feed_id, client, pool) = setup_feed_for_cache_test(&tc).await?;

    // Seed next_fetch_at 10 minutes into the future.
    let conn = tc.database_conn()?;
    conn.execute(
        "UPDATE feeds SET next_fetch_at = ?1 WHERE id = ?2",
        rusqlite::params![Utc::now().timestamp() + 600, feed_id],
    )?;

    refresh_feed(
        &client,
        feed_id,
        pool,
        None,
        &super::test_metrics(),
        &super::test_tx(),
    )
    .await?;

    let s = state.lock().unwrap();
    assert_eq!(
        s.request_count, 0,
        "fetch should be skipped when next_fetch_at is in the future"
    );
    Ok(())
}

/// `max-age=5` is below the 60s floor and must not schedule a
/// next-fetch sooner than `min_polling_cadence_seconds`.
#[tokio::test]
async fn test_max_age_below_min_cadence_is_floored() -> Result<()> {
    let mut tc = TestBuilder::default().init_database().build()?;
    let state: SharedFeedServerState = Arc::new(Mutex::new(FeedServerState {
        cache_control: Some("max-age=5".into()),
        ..Default::default()
    }));
    tc.init_feed_server_with_state(state.clone()).await?;

    let (feed_id, client, pool) = setup_feed_for_cache_test(&tc).await?;

    let before = Utc::now().timestamp();
    refresh_feed(
        &client,
        feed_id,
        pool,
        None,
        &super::test_metrics(),
        &super::test_tx(),
    )
    .await?;
    let after = Utc::now().timestamp();

    let conn = tc.database_conn()?;
    let next_fetch_at: Option<i64> = conn.query_row(
        "SELECT next_fetch_at FROM feeds WHERE id = ?1",
        [feed_id],
        |row| row.get(0),
    )?;
    let nf = next_fetch_at.expect("next_fetch_at should be set");
    assert!(
        nf >= before + 60 && nf <= after + 60,
        "max-age=5 should be floored to now+60s, got offset {}",
        nf - before
    );
    Ok(())
}

/// A generous `max-age` must be capped by the per-feed
/// `min_fetch_interval_seconds` — we never wait longer than the user asked.
#[tokio::test]
async fn test_max_age_above_min_fetch_interval_is_capped() -> Result<()> {
    let mut tc = TestBuilder::default().init_database().build()?;
    let state: SharedFeedServerState = Arc::new(Mutex::new(FeedServerState {
        cache_control: Some("max-age=86400".into()),
        ..Default::default()
    }));
    tc.init_feed_server_with_state(state.clone()).await?;

    let (feed_id, client, pool) = setup_feed_for_cache_test(&tc).await?;

    // Lower the per-feed interval to 1 hour so we can assert the cap.
    {
        let conn = tc.database_conn()?;
        conn.execute(
            "UPDATE feeds SET min_fetch_interval_seconds = 3600 WHERE id = ?1",
            [feed_id],
        )?;
    }

    let before = Utc::now().timestamp();
    refresh_feed(
        &client,
        feed_id,
        pool,
        None,
        &super::test_metrics(),
        &super::test_tx(),
    )
    .await?;
    let after = Utc::now().timestamp();

    let conn = tc.database_conn()?;
    let next_fetch_at: Option<i64> = conn.query_row(
        "SELECT next_fetch_at FROM feeds WHERE id = ?1",
        [feed_id],
        |row| row.get(0),
    )?;
    let nf = next_fetch_at.expect("next_fetch_at should be set");
    assert!(
        nf >= before + 3600 && nf <= after + 3600,
        "max-age should be capped at per-feed interval (3600s), got offset {}",
        nf - before
    );
    Ok(())
}

/// A 200 OK after a failure streak resets `consecutive_failures` to 0.
#[tokio::test]
async fn test_success_resets_consecutive_failures() -> Result<()> {
    let mut tc = TestBuilder::default().init_database().build()?;
    let state: SharedFeedServerState = Arc::new(Mutex::new(FeedServerState::default()));
    tc.init_feed_server_with_state(state.clone()).await?;

    let (feed_id, client, pool) = setup_feed_for_cache_test(&tc).await?;

    // Pre-populate a failure streak.
    {
        let conn = tc.database_conn()?;
        conn.execute(
            "UPDATE feeds SET consecutive_failures = 3 WHERE id = ?1",
            [feed_id],
        )?;
    }

    refresh_feed(
        &client,
        feed_id,
        pool,
        None,
        &super::test_metrics(),
        &super::test_tx(),
    )
    .await?;

    let conn = tc.database_conn()?;
    let failures: i64 = conn.query_row(
        "SELECT consecutive_failures FROM feeds WHERE id = ?1",
        [feed_id],
        |row| row.get(0),
    )?;
    assert_eq!(failures, 0);
    Ok(())
}

/// A 304 Not Modified also resets the failure streak and reschedules
/// from the max-age hint on the 304 response.
#[tokio::test]
async fn test_304_resets_consecutive_failures_and_reschedules() -> Result<()> {
    let mut tc = TestBuilder::default().init_database().build()?;
    let state: SharedFeedServerState = Arc::new(Mutex::new(FeedServerState {
        etag: Some("\"304-reset\"".into()),
        cache_control: Some("max-age=600".into()),
        ..Default::default()
    }));
    tc.init_feed_server_with_state(state.clone()).await?;

    let (feed_id, client, pool) = setup_feed_for_cache_test(&tc).await?;

    // First fetch stores ETag and succeeds.
    refresh_feed(
        &client,
        feed_id,
        pool.clone(),
        None,
        &super::test_metrics(),
        &super::test_tx(),
    )
    .await?;

    // Inject a failure streak plus clear the gate so the next refresh runs.
    {
        let conn = tc.database_conn()?;
        conn.execute(
            "UPDATE feeds SET consecutive_failures = 5, next_fetch_at = NULL WHERE id = ?1",
            [feed_id],
        )?;
    }

    let before = Utc::now().timestamp();
    refresh_feed(
        &client,
        feed_id,
        pool,
        None,
        &super::test_metrics(),
        &super::test_tx(),
    )
    .await?;
    let after = Utc::now().timestamp();

    let conn = tc.database_conn()?;
    let (failures, next_fetch_at): (i64, Option<i64>) = conn.query_row(
        "SELECT consecutive_failures, next_fetch_at FROM feeds WHERE id = ?1",
        [feed_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    assert_eq!(failures, 0, "304 should reset consecutive_failures");
    let nf = next_fetch_at.expect("next_fetch_at should be set");
    // Allow a small slack below the lower bound: the server's auto-injected
    // Date header can round to the previous second, which `corrected_max_age`
    // then subtracts from the declared max-age (RFC 9111 §4.2.3).
    assert!(
        nf >= before + 595 && nf <= after + 600,
        "304 with max-age=600 should schedule ~600s out, got offset {}",
        nf - before
    );

    let s = state.lock().unwrap();
    assert_eq!(s.not_modified_count, 1);
    Ok(())
}

/// Multiple `Cache-Control` header fields are combined per RFC 9110 §5.3:
/// `max-age` from one header and `no-store` from the other should both
/// apply, and `no-store` must win (clearing any stored freshness).
#[tokio::test]
async fn test_multiple_cache_control_headers_combine() -> Result<()> {
    let mut tc = TestBuilder::default().init_database().build()?;
    let state: SharedFeedServerState = Arc::new(Mutex::new(FeedServerState {
        cache_control: Some("max-age=3600".into()),
        cache_control_extra: vec!["no-store".into()],
        etag: Some("\"combine\"".into()),
        ..Default::default()
    }));
    tc.init_feed_server_with_state(state.clone()).await?;

    let (feed_id, client, pool) = setup_feed_for_cache_test(&tc).await?;

    refresh_feed(
        &client,
        feed_id,
        pool,
        None,
        &super::test_metrics(),
        &super::test_tx(),
    )
    .await?;

    let conn = tc.database_conn()?;
    let (stored_etag, stored_expires): (Option<String>, Option<i64>) = conn.query_row(
        "SELECT header_etag, header_expires FROM feeds WHERE id = ?1",
        [feed_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    assert!(
        stored_etag.is_none(),
        "no-store from combined Cache-Control should clear etag"
    );
    assert!(
        stored_expires.is_none(),
        "no-store from combined Cache-Control should clear expires"
    );
    Ok(())
}

/// `Age: 300` with `max-age=600` should subtract the upstream age
/// from the freshness lifetime (RFC 9111 §4.2.3).
#[tokio::test]
async fn test_age_header_reduces_max_age() -> Result<()> {
    let mut tc = TestBuilder::default().init_database().build()?;
    let state: SharedFeedServerState = Arc::new(Mutex::new(FeedServerState {
        cache_control: Some("max-age=600".into()),
        age: Some(300),
        ..Default::default()
    }));
    tc.init_feed_server_with_state(state.clone()).await?;

    let (feed_id, client, pool) = setup_feed_for_cache_test(&tc).await?;
    let before = Utc::now().timestamp();

    refresh_feed(
        &client,
        feed_id,
        pool,
        None,
        &super::test_metrics(),
        &super::test_tx(),
    )
    .await?;

    let conn = tc.database_conn()?;
    let stored_expires: Option<i64> = conn.query_row(
        "SELECT header_expires FROM feeds WHERE id = ?1",
        [feed_id],
        |row| row.get(0),
    )?;
    let expires = stored_expires.expect("header_expires should be set");
    // 600 - 300 = 300 seconds of remaining freshness.
    assert!(
        expires >= before + 290 && expires <= before + 310,
        "Age header should subtract from max-age; got offset {}",
        expires - before
    );
    Ok(())
}

/// A `Date` 60 seconds in the past should subtract 60s from `max-age`
/// even without an explicit `Age` header (RFC 9111 §4.2.3).
#[tokio::test]
async fn test_date_skew_reduces_max_age() -> Result<()> {
    let past_date = (Utc::now() - chrono::Duration::seconds(60))
        .format("%a, %d %b %Y %H:%M:%S GMT")
        .to_string();
    let mut tc = TestBuilder::default().init_database().build()?;
    let state: SharedFeedServerState = Arc::new(Mutex::new(FeedServerState {
        cache_control: Some("max-age=600".into()),
        date: Some(past_date),
        ..Default::default()
    }));
    tc.init_feed_server_with_state(state.clone()).await?;

    let (feed_id, client, pool) = setup_feed_for_cache_test(&tc).await?;
    let before = Utc::now().timestamp();

    refresh_feed(
        &client,
        feed_id,
        pool,
        None,
        &super::test_metrics(),
        &super::test_tx(),
    )
    .await?;

    let conn = tc.database_conn()?;
    let stored_expires: Option<i64> = conn.query_row(
        "SELECT header_expires FROM feeds WHERE id = ?1",
        [feed_id],
        |row| row.get(0),
    )?;
    let expires = stored_expires.expect("header_expires should be set");
    // 600 - 60 = 540 seconds of remaining freshness (±10s slack for test jitter).
    assert!(
        expires >= before + 530 && expires <= before + 550,
        "Date in the past should subtract from max-age; got offset {}",
        expires - before
    );
    Ok(())
}

/// `Pragma: no-cache` without any `Cache-Control` header should clear
/// the freshness hint (RFC 9111 §5.4).
#[tokio::test]
async fn test_pragma_no_cache_without_cache_control() -> Result<()> {
    let future_expires = (Utc::now() + chrono::Duration::hours(1))
        .format("%a, %d %b %Y %H:%M:%S GMT")
        .to_string();
    let mut tc = TestBuilder::default().init_database().build()?;
    let state: SharedFeedServerState = Arc::new(Mutex::new(FeedServerState {
        expires: Some(future_expires),
        pragma: Some("no-cache".into()),
        ..Default::default()
    }));
    tc.init_feed_server_with_state(state.clone()).await?;

    let (feed_id, client, pool) = setup_feed_for_cache_test(&tc).await?;
    let before = Utc::now().timestamp();

    refresh_feed(
        &client,
        feed_id,
        pool,
        None,
        &super::test_metrics(),
        &super::test_tx(),
    )
    .await?;

    let conn = tc.database_conn()?;
    let next_fetch_at: Option<i64> = conn.query_row(
        "SELECT next_fetch_at FROM feeds WHERE id = ?1",
        [feed_id],
        |row| row.get(0),
    )?;
    let nf = next_fetch_at.expect("next_fetch_at should be set");
    // Pragma: no-cache should suppress the Expires hint, so the next fetch
    // should fall back to min_fetch_interval (default 10800s) rather than
    // being gated by the Expires header an hour out.
    assert!(
        nf - before > 3600,
        "pragma no-cache should fall back to min_fetch_interval; got offset {}",
        nf - before
    );
    Ok(())
}

/// `Pragma: no-cache` is ignored when `Cache-Control` is present
/// (RFC 9111 §5.4 restricts the fallback to requests/responses with no
/// Cache-Control).
#[tokio::test]
async fn test_pragma_ignored_when_cache_control_present() -> Result<()> {
    let mut tc = TestBuilder::default().init_database().build()?;
    let state: SharedFeedServerState = Arc::new(Mutex::new(FeedServerState {
        cache_control: Some("max-age=3600".into()),
        pragma: Some("no-cache".into()),
        ..Default::default()
    }));
    tc.init_feed_server_with_state(state.clone()).await?;

    let (feed_id, client, pool) = setup_feed_for_cache_test(&tc).await?;
    let before = Utc::now().timestamp();

    refresh_feed(
        &client,
        feed_id,
        pool,
        None,
        &super::test_metrics(),
        &super::test_tx(),
    )
    .await?;

    let conn = tc.database_conn()?;
    let stored_expires: Option<i64> = conn.query_row(
        "SELECT header_expires FROM feeds WHERE id = ?1",
        [feed_id],
        |row| row.get(0),
    )?;
    let expires = stored_expires.expect("header_expires should be set from max-age");
    assert!(
        expires >= before + 3500 && expires <= before + 3700,
        "Cache-Control max-age should win over Pragma; got offset {}",
        expires - before
    );
    Ok(())
}

/// `Cache-Control: immutable, max-age=3600` stores a future
/// `header_immutable_until`, and the next refresh omits conditional
/// request headers (RFC 8246).
#[tokio::test]
async fn test_immutable_skips_conditional_headers() -> Result<()> {
    let mut tc = TestBuilder::default().init_database().build()?;
    let state: SharedFeedServerState = Arc::new(Mutex::new(FeedServerState {
        etag: Some("\"imm\"".into()),
        last_modified: Some("Sat, 01 Jan 2025 00:00:00 GMT".into()),
        cache_control: Some("immutable, max-age=3600".into()),
        ..Default::default()
    }));
    tc.init_feed_server_with_state(state.clone()).await?;

    let (feed_id, client, pool) = setup_feed_for_cache_test(&tc).await?;
    let before = Utc::now().timestamp();

    refresh_feed(
        &client,
        feed_id,
        pool.clone(),
        None,
        &super::test_metrics(),
        &super::test_tx(),
    )
    .await?;

    let conn = tc.database_conn()?;
    let stored_until: Option<i64> = conn.query_row(
        "SELECT header_immutable_until FROM feeds WHERE id = ?1",
        [feed_id],
        |row| row.get(0),
    )?;
    let until = stored_until.expect("header_immutable_until should be set");
    assert!(
        until >= before + 3500 && until <= before + 3700,
        "header_immutable_until should be ~now+3600"
    );

    // Force a second fetch (the scheduler would otherwise gate us).
    reset_last_checked(&conn, feed_id);

    refresh_feed(
        &client,
        feed_id,
        pool,
        None,
        &super::test_metrics(),
        &super::test_tx(),
    )
    .await?;

    let s = state.lock().unwrap();
    assert_eq!(
        s.if_none_match_count, 0,
        "immutable response should suppress If-None-Match on the follow-up"
    );
    assert_eq!(
        s.if_modified_since_count, 0,
        "immutable response should suppress If-Modified-Since on the follow-up"
    );
    Ok(())
}

/// A 503 that carries `Cache-Control: stale-if-error=3600` must cap the
/// backoff schedule at 3600 seconds so we retry before the grace window
/// elapses (RFC 5861 §4).
#[tokio::test]
async fn test_stale_if_error_caps_backoff() -> Result<()> {
    let mut tc = TestBuilder::default().init_database().build()?;
    let state: SharedFeedServerState = Arc::new(Mutex::new(FeedServerState {
        fail_next: 1,
        fail_status: 503,
        cache_control: Some("stale-if-error=3600".into()),
        ..Default::default()
    }));
    tc.init_feed_server_with_state(state.clone()).await?;

    let (feed_id, client, pool) = setup_feed_for_cache_test(&tc).await?;

    // Pre-populate a long failure streak so the exponential backoff would
    // otherwise push next_fetch_at well past one hour.
    {
        let conn = tc.database_conn()?;
        conn.execute(
            "UPDATE feeds SET consecutive_failures = 10 WHERE id = ?1",
            [feed_id],
        )?;
    }

    let before = Utc::now().timestamp();
    refresh_feed(
        &client,
        feed_id,
        pool,
        None,
        &super::test_metrics(),
        &super::test_tx(),
    )
    .await?;

    let conn = tc.database_conn()?;
    let nf: i64 = conn
        .query_row(
            "SELECT next_fetch_at FROM feeds WHERE id = ?1",
            [feed_id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        nf <= before + 3600 + 10,
        "stale-if-error=3600 must cap the schedule; got offset {}",
        nf - before
    );
    Ok(())
}

/// The RFC 850 date format is still accepted for the `Expires` header
/// (RFC 9110 §5.6.7).
#[tokio::test]
async fn test_expires_rfc850_format_is_parsed() -> Result<()> {
    let future = Utc::now() + chrono::Duration::minutes(30);
    let rfc850 = future.format("%A, %d-%b-%y %H:%M:%S GMT").to_string();

    let mut tc = TestBuilder::default().init_database().build()?;
    let state: SharedFeedServerState = Arc::new(Mutex::new(FeedServerState {
        expires: Some(rfc850),
        ..Default::default()
    }));
    tc.init_feed_server_with_state(state.clone()).await?;

    let (feed_id, client, pool) = setup_feed_for_cache_test(&tc).await?;
    let before = Utc::now().timestamp();

    refresh_feed(
        &client,
        feed_id,
        pool,
        None,
        &super::test_metrics(),
        &super::test_tx(),
    )
    .await?;

    let conn = tc.database_conn()?;
    let stored: Option<i64> = conn.query_row(
        "SELECT header_expires FROM feeds WHERE id = ?1",
        [feed_id],
        |row| row.get(0),
    )?;
    let stored = stored.expect("header_expires should parse from RFC 850 format");
    assert!(
        stored >= before + 25 * 60 && stored <= before + 35 * 60,
        "RFC 850 Expires should parse into a ~30min-future timestamp"
    );
    Ok(())
}

/// Settings with `force_refresh_after_seconds` set to `secs`, so a forced
/// refresh can be made to fire immediately.
fn with_force_refresh_after_secs(secs: u64) -> crate::config::Settings {
    let mut settings = crate::config::Settings::default();
    settings.feed_fetch.force_refresh_after_seconds = secs;
    settings
}

/// A server that keeps returning the same `ETag` while silently changing
/// the body should be flagged once the forced-refresh cadence elapses.
#[tokio::test]
async fn test_forced_refresh_detects_validator_lie() -> Result<()> {
    let mut tc = TestBuilder::default().init_database().build()?;
    let body_a = b"<rss version=\"2.0\"><channel><title>A</title></channel></rss>".to_vec();
    let body_b = b"<rss version=\"2.0\"><channel><title>B</title></channel></rss>".to_vec();
    let state: SharedFeedServerState = Arc::new(Mutex::new(FeedServerState {
        etag: Some("\"v1\"".into()),
        body_override: Some(body_a.clone()),
        ..Default::default()
    }));
    tc.init_feed_server_with_state(state.clone()).await?;

    let (feed_id, client, pool) = setup_feed_for_cache_test(&tc).await?;

    // Force refresh should fire on the very next eligible fetch.
    let settings = with_force_refresh_after_secs(0);

    let metrics = super::test_metrics();

    // First fetch: normal 200, records baseline body hash + last_full_refresh_at.
    refresh_feed_with_settings(
        &client,
        feed_id,
        pool.clone(),
        &settings,
        None,
        &metrics,
        &super::test_tx(),
    )
    .await?;

    let expected_hash_a = blake3::hash(&body_a).to_hex().to_string();
    let conn = tc.database_conn()?;
    let (stored_hash, stored_refresh_at): (Option<String>, Option<i64>) = conn.query_row(
        "SELECT header_body_hash, last_full_refresh_at FROM feeds WHERE id = ?1",
        [feed_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    assert_eq!(stored_hash.as_deref(), Some(expected_hash_a.as_str()));
    assert!(stored_refresh_at.is_some());

    // Now flip the body but keep the server's ETag identical — exactly the
    // pathological case the forced refresh is supposed to catch.
    {
        let mut s = state.lock().unwrap();
        s.body_override = Some(body_b.clone());
    }
    reset_last_checked(&conn, feed_id);

    // Second fetch: force_refresh_after_secs=0 skips conditionals, so the
    // server serves the new body under the old ETag.
    refresh_feed_with_settings(
        &client,
        feed_id,
        pool,
        &settings,
        None,
        &metrics,
        &super::test_tx(),
    )
    .await?;

    let expected_hash_b = blake3::hash(&body_b).to_hex().to_string();
    let stored_hash_after: Option<String> = conn.query_row(
        "SELECT header_body_hash FROM feeds WHERE id = ?1",
        [feed_id],
        |row| row.get(0),
    )?;
    assert_eq!(
        stored_hash_after.as_deref(),
        Some(expected_hash_b.as_str()),
        "body hash should be updated to the newly-fetched body"
    );

    let s = state.lock().unwrap();
    assert_eq!(
        s.full_response_count, 2,
        "both fetches should receive a full body (second one forced)"
    );
    assert_eq!(
        s.if_none_match_count, 0,
        "forced refresh must not send If-None-Match"
    );
    drop(s);

    let rendered = metrics.render();
    assert!(
        rendered.contains("kiki_feed_validator_lie_total 1"),
        "validator-lie counter should have been incremented exactly once; got:\n{rendered}"
    );
    assert!(
        rendered.contains("kiki_feed_forced_refresh_total{outcome=\"mismatch\"} 1"),
        "forced-refresh counter should record the mismatch; got:\n{rendered}"
    );

    Ok(())
}

/// When a forced refresh confirms the server's body actually matches the
/// stored hash, the validator-lie counter must stay silent.
#[tokio::test]
async fn test_forced_refresh_match_does_not_fire_lie() -> Result<()> {
    let mut tc = TestBuilder::default().init_database().build()?;
    let body = b"<rss version=\"2.0\"><channel><title>stable</title></channel></rss>".to_vec();
    let state: SharedFeedServerState = Arc::new(Mutex::new(FeedServerState {
        etag: Some("\"v1\"".into()),
        body_override: Some(body.clone()),
        ..Default::default()
    }));
    tc.init_feed_server_with_state(state.clone()).await?;

    let (feed_id, client, pool) = setup_feed_for_cache_test(&tc).await?;
    let settings = with_force_refresh_after_secs(0);

    let metrics = super::test_metrics();

    refresh_feed_with_settings(
        &client,
        feed_id,
        pool.clone(),
        &settings,
        None,
        &metrics,
        &super::test_tx(),
    )
    .await?;

    let conn = tc.database_conn()?;
    reset_last_checked(&conn, feed_id);

    refresh_feed_with_settings(
        &client,
        feed_id,
        pool,
        &settings,
        None,
        &metrics,
        &super::test_tx(),
    )
    .await?;

    let rendered = metrics.render();
    assert!(
        !rendered.contains("kiki_feed_validator_lie_total 1"),
        "validator-lie counter should stay at zero when body matches; got:\n{rendered}"
    );
    assert!(
        rendered.contains("kiki_feed_forced_refresh_total{outcome=\"match\"} 1"),
        "forced-refresh counter should record the match; got:\n{rendered}"
    );

    Ok(())
}

/// Read the stored `(header_etag, header_last_modified)` for a feed.
fn stored_validators(
    conn: &rusqlite::Connection,
    feed_id: i64,
) -> (Option<String>, Option<String>) {
    conn.query_row(
        "SELECT header_etag, header_last_modified FROM feeds WHERE id = ?1",
        [feed_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .unwrap()
}

/// Fetch once to store validators, then make the feed eligible again.
async fn fetch_and_reset(
    tc: &crate::test::TestConfig,
    client: &reqwest::Client,
    feed_id: i64,
    pool: &crate::db::Pool,
) -> Result<()> {
    refresh_feed(
        client,
        feed_id,
        pool.clone(),
        None,
        &super::test_metrics(),
        &super::test_tx(),
    )
    .await?;
    reset_last_checked(&tc.database_conn()?, feed_id);
    Ok(())
}

/// A 304 that carries a new `ETag` replaces the stored one (RFC 9111
/// §4.3.4), and the new value is what the next request revalidates with.
#[tokio::test]
async fn test_304_rotated_etag_is_stored_and_sent() -> Result<()> {
    let mut tc = TestBuilder::default().init_database().build()?;
    let state: SharedFeedServerState = Arc::new(Mutex::new(FeedServerState {
        etag: Some("\"v1\"".into()),
        ..Default::default()
    }));
    tc.init_feed_server_with_state(state.clone()).await?;
    let (feed_id, client, pool) = setup_feed_for_cache_test(&tc).await?;

    fetch_and_reset(&tc, &client, feed_id, &pool).await?;

    // The server revalidates "v1" but announces "v2" on the 304.
    state.lock().unwrap().not_modified_etag = Some("\"v2\"".into());
    fetch_and_reset(&tc, &client, feed_id, &pool).await?;
    assert_eq!(state.lock().unwrap().not_modified_count, 1);
    assert_eq!(
        stored_validators(&tc.database_conn()?, feed_id)
            .0
            .as_deref(),
        Some("\"v2\"")
    );

    // From now on the server only recognizes "v2".
    {
        let mut s = state.lock().unwrap();
        s.etag = Some("\"v2\"".into());
        s.not_modified_etag = None;
    }
    fetch_and_reset(&tc, &client, feed_id, &pool).await?;
    let s = state.lock().unwrap();
    assert_eq!(s.last_if_none_match.as_deref(), Some("\"v2\""));
    assert_eq!(
        s.not_modified_count, 2,
        "the rotated ETag should revalidate"
    );
    assert_eq!(s.full_response_count, 1);
    Ok(())
}

/// A 304 that carries a new `Last-Modified` replaces the stored one.
#[tokio::test]
async fn test_304_rotated_last_modified_is_stored_and_sent() -> Result<()> {
    let lm1 = "Mon, 01 Jan 2024 00:00:00 GMT";
    let lm2 = "Tue, 02 Jan 2024 00:00:00 GMT";
    let mut tc = TestBuilder::default().init_database().build()?;
    let state: SharedFeedServerState = Arc::new(Mutex::new(FeedServerState {
        last_modified: Some(lm1.into()),
        ..Default::default()
    }));
    tc.init_feed_server_with_state(state.clone()).await?;
    let (feed_id, client, pool) = setup_feed_for_cache_test(&tc).await?;

    fetch_and_reset(&tc, &client, feed_id, &pool).await?;
    state.lock().unwrap().not_modified_last_modified = Some(lm2.into());
    fetch_and_reset(&tc, &client, feed_id, &pool).await?;
    assert_eq!(
        stored_validators(&tc.database_conn()?, feed_id)
            .1
            .as_deref(),
        Some(lm2)
    );

    {
        let mut s = state.lock().unwrap();
        s.last_modified = Some(lm2.into());
        s.not_modified_last_modified = None;
    }
    fetch_and_reset(&tc, &client, feed_id, &pool).await?;
    let s = state.lock().unwrap();
    assert_eq!(s.last_if_modified_since.as_deref(), Some(lm2));
    assert_eq!(s.not_modified_count, 2);
    Ok(())
}

/// A 304 that omits the validators keeps the stored ones.
#[tokio::test]
async fn test_304_without_validators_keeps_stored_ones() -> Result<()> {
    let lm = "Mon, 01 Jan 2024 00:00:00 GMT";
    let mut tc = TestBuilder::default().init_database().build()?;
    let state: SharedFeedServerState = Arc::new(Mutex::new(FeedServerState {
        etag: Some("\"v1\"".into()),
        last_modified: Some(lm.into()),
        ..Default::default()
    }));
    tc.init_feed_server_with_state(state.clone()).await?;
    let (feed_id, client, pool) = setup_feed_for_cache_test(&tc).await?;

    fetch_and_reset(&tc, &client, feed_id, &pool).await?;
    fetch_and_reset(&tc, &client, feed_id, &pool).await?;
    assert_eq!(state.lock().unwrap().not_modified_count, 1);
    assert_eq!(
        stored_validators(&tc.database_conn()?, feed_id),
        (Some("\"v1\"".into()), Some(lm.into()))
    );
    Ok(())
}

/// `Cache-Control: no-store` on a 304 clears the stored validators, as it
/// does on a 200.
#[tokio::test]
async fn test_304_no_store_clears_validators() -> Result<()> {
    let mut tc = TestBuilder::default().init_database().build()?;
    let state: SharedFeedServerState = Arc::new(Mutex::new(FeedServerState {
        etag: Some("\"v1\"".into()),
        ..Default::default()
    }));
    tc.init_feed_server_with_state(state.clone()).await?;
    let (feed_id, client, pool) = setup_feed_for_cache_test(&tc).await?;

    fetch_and_reset(&tc, &client, feed_id, &pool).await?;
    // Only after the first 200, so it stores the validators.
    state.lock().unwrap().cache_control = Some("no-store".into());
    fetch_and_reset(&tc, &client, feed_id, &pool).await?;
    assert_eq!(state.lock().unwrap().not_modified_count, 1);
    assert_eq!(
        stored_validators(&tc.database_conn()?, feed_id),
        (None, None)
    );
    Ok(())
}

/// A 304 carrying `immutable` with a `max-age` opens a new immutable
/// window, so the next refresh sends no conditional headers (RFC 8246).
#[tokio::test]
async fn test_304_immutable_opens_window() -> Result<()> {
    let mut tc = TestBuilder::default().init_database().build()?;
    let state: SharedFeedServerState = Arc::new(Mutex::new(FeedServerState {
        etag: Some("\"v1\"".into()),
        ..Default::default()
    }));
    tc.init_feed_server_with_state(state.clone()).await?;
    let (feed_id, client, pool) = setup_feed_for_cache_test(&tc).await?;

    fetch_and_reset(&tc, &client, feed_id, &pool).await?;
    state.lock().unwrap().cache_control = Some("immutable, max-age=3600".into());
    let now = Utc::now().timestamp();
    fetch_and_reset(&tc, &client, feed_id, &pool).await?;
    assert_eq!(state.lock().unwrap().not_modified_count, 1);

    let (immutable_until, expires): (Option<i64>, Option<i64>) = tc.database_conn()?.query_row(
        "SELECT header_immutable_until, header_expires FROM feeds WHERE id = ?1",
        [feed_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let until = immutable_until.expect("the 304 should open an immutable window");
    assert!((now + 3590..=now + 3610).contains(&until), "got {until}");
    assert!(expires.is_some_and(|e| (now + 3590..=now + 3610).contains(&e)));

    // Inside the window the next refresh must not revalidate.
    let before = state.lock().unwrap().if_none_match_count;
    fetch_and_reset(&tc, &client, feed_id, &pool).await?;
    let s = state.lock().unwrap();
    assert_eq!(s.if_none_match_count, before);
    assert_eq!(s.full_response_count, 2);
    Ok(())
}

/// A 304 with no freshness headers leaves the stored expiry alone.
#[tokio::test]
async fn test_304_without_freshness_keeps_stored_expires() -> Result<()> {
    let mut tc = TestBuilder::default().init_database().build()?;
    let state: SharedFeedServerState = Arc::new(Mutex::new(FeedServerState {
        etag: Some("\"v1\"".into()),
        cache_control: Some("max-age=600".into()),
        ..Default::default()
    }));
    tc.init_feed_server_with_state(state.clone()).await?;
    let (feed_id, client, pool) = setup_feed_for_cache_test(&tc).await?;

    fetch_and_reset(&tc, &client, feed_id, &pool).await?;
    let read_expires = || -> Option<i64> {
        tc.database_conn()
            .unwrap()
            .query_row(
                "SELECT header_expires FROM feeds WHERE id = ?1",
                [feed_id],
                |row| row.get(0),
            )
            .unwrap()
    };
    let stored = read_expires();
    assert!(stored.is_some());

    state.lock().unwrap().cache_control = None;
    fetch_and_reset(&tc, &client, feed_id, &pool).await?;
    assert_eq!(state.lock().unwrap().not_modified_count, 1);
    assert_eq!(read_expires(), stored);
    Ok(())
}

/// Run one `refresh_feed` with the default test settings.
async fn refresh_once(
    client: &reqwest::Client,
    feed_id: i64,
    pool: &crate::db::Pool,
) -> Result<()> {
    refresh_feed(
        client,
        feed_id,
        pool.clone(),
        None,
        &super::test_metrics(),
        &super::test_tx(),
    )
    .await
}

/// Read one nullable integer column from a feed row.
fn feed_column(tc: &crate::test::TestConfig, feed_id: i64, column: &str) -> Option<i64> {
    tc.database_conn()
        .unwrap()
        .query_row(
            &format!("SELECT {column} FROM feeds WHERE id = ?1"),
            [feed_id],
            |row| row.get(0),
        )
        .unwrap()
}

/// `immutable` without a `max-age` has no freshness to cover: no immutable
/// window is stored and the next refresh still revalidates.
#[tokio::test]
async fn test_immutable_without_max_age_still_revalidates() -> Result<()> {
    let mut tc = TestBuilder::default().init_database().build()?;
    let state: SharedFeedServerState = Arc::new(Mutex::new(FeedServerState {
        etag: Some("\"v1\"".into()),
        cache_control: Some("immutable".into()),
        ..Default::default()
    }));
    tc.init_feed_server_with_state(state.clone()).await?;
    let (feed_id, client, pool) = setup_feed_for_cache_test(&tc).await?;

    refresh_once(&client, feed_id, &pool).await?;
    assert_eq!(feed_column(&tc, feed_id, "header_immutable_until"), None);

    reset_last_checked(&tc.database_conn()?, feed_id);
    refresh_once(&client, feed_id, &pool).await?;
    let s = state.lock().unwrap();
    assert_eq!(
        s.if_none_match_count, 1,
        "should revalidate with If-None-Match"
    );
    assert_eq!(s.not_modified_count, 1);
    Ok(())
}

/// Once an immutable window has passed, conditional requests resume.
#[tokio::test]
async fn test_expired_immutable_window_resumes_revalidation() -> Result<()> {
    let mut tc = TestBuilder::default().init_database().build()?;
    let state: SharedFeedServerState = Arc::new(Mutex::new(FeedServerState {
        etag: Some("\"v1\"".into()),
        cache_control: Some("immutable, max-age=3600".into()),
        ..Default::default()
    }));
    tc.init_feed_server_with_state(state.clone()).await?;
    let (feed_id, client, pool) = setup_feed_for_cache_test(&tc).await?;

    refresh_once(&client, feed_id, &pool).await?;
    assert!(feed_column(&tc, feed_id, "header_immutable_until").is_some());

    // Move the window into the past and make the feed eligible.
    let conn = tc.database_conn()?;
    conn.execute(
        "UPDATE feeds SET header_immutable_until = ?1 WHERE id = ?2",
        rusqlite::params![Utc::now().timestamp() - 1, feed_id],
    )?;
    reset_last_checked(&conn, feed_id);

    refresh_once(&client, feed_id, &pool).await?;
    let s = state.lock().unwrap();
    assert_eq!(s.if_none_match_count, 1, "expired window should revalidate");
    assert_eq!(s.not_modified_count, 1);
    assert_eq!(s.full_response_count, 1);
    Ok(())
}

/// A `Date` ahead of our clock must not extend freshness past `max-age`.
#[tokio::test]
async fn test_future_date_does_not_extend_max_age() -> Result<()> {
    let future_date = (Utc::now() + chrono::Duration::minutes(30))
        .format("%a, %d %b %Y %H:%M:%S GMT")
        .to_string();
    let mut tc = TestBuilder::default().init_database().build()?;
    let state: SharedFeedServerState = Arc::new(Mutex::new(FeedServerState {
        cache_control: Some("max-age=600".into()),
        date: Some(future_date),
        ..Default::default()
    }));
    tc.init_feed_server_with_state(state.clone()).await?;
    let (feed_id, client, pool) = setup_feed_for_cache_test(&tc).await?;

    let before = Utc::now().timestamp();
    refresh_once(&client, feed_id, &pool).await?;

    let expires = feed_column(&tc, feed_id, "header_expires").expect("header_expires set");
    let next = feed_column(&tc, feed_id, "next_fetch_at").expect("next_fetch_at set");
    for (what, ts) in [("header_expires", expires), ("next_fetch_at", next)] {
        assert!(
            (before + 595..=before + 610).contains(&ts),
            "{what} should be ~now+600 despite the future Date; got offset {}",
            ts - before
        );
    }
    Ok(())
}

/// `Expires` in asctime format is parsed (RFC 9110 §5.6.7).
#[tokio::test]
async fn test_expires_asctime_format_is_parsed() -> Result<()> {
    let future = Utc::now() + chrono::Duration::minutes(30);
    let asctime = future.format("%a %b %e %H:%M:%S %Y").to_string();

    let mut tc = TestBuilder::default().init_database().build()?;
    let state: SharedFeedServerState = Arc::new(Mutex::new(FeedServerState {
        expires: Some(asctime),
        ..Default::default()
    }));
    tc.init_feed_server_with_state(state.clone()).await?;
    let (feed_id, client, pool) = setup_feed_for_cache_test(&tc).await?;

    let before = Utc::now().timestamp();
    refresh_once(&client, feed_id, &pool).await?;

    let stored = feed_column(&tc, feed_id, "header_expires")
        .expect("header_expires should parse from asctime format");
    assert!(
        (before + 25 * 60..=before + 35 * 60).contains(&stored),
        "asctime Expires should parse into a ~30min-future timestamp"
    );
    Ok(())
}

/// Refresh once against a server sending `Expires: expires`, and return
/// the scheduled `next_fetch_at` offset from just before the refresh.
async fn next_fetch_offset_with_expires(expires: String) -> Result<(i64, Option<i64>)> {
    let mut tc = TestBuilder::default().init_database().build()?;
    let state: SharedFeedServerState = Arc::new(Mutex::new(FeedServerState {
        expires: Some(expires),
        ..Default::default()
    }));
    tc.init_feed_server_with_state(state.clone()).await?;
    let (feed_id, client, pool) = setup_feed_for_cache_test(&tc).await?;

    let before = Utc::now().timestamp();
    refresh_once(&client, feed_id, &pool).await?;
    let next = feed_column(&tc, feed_id, "next_fetch_at").expect("next_fetch_at set");
    Ok((next - before, feed_column(&tc, feed_id, "header_expires")))
}

/// An `Expires` already in the past gives no freshness, so the feed stays
/// on its per-feed interval instead of being polled at the min-cadence
/// floor.
#[tokio::test]
async fn test_past_expires_uses_per_feed_interval() -> Result<()> {
    let past = (Utc::now() - chrono::Duration::hours(1))
        .format("%a, %d %b %Y %H:%M:%S GMT")
        .to_string();
    let (offset, stored) = next_fetch_offset_with_expires(past).await?;
    assert!(stored.is_some(), "the server's Expires is still recorded");
    assert!(
        (10_800..=10_810).contains(&offset),
        "should use the default 3h per-feed interval; got offset {offset}"
    );
    Ok(())
}

/// `Expires: 0` is invalid and means "already expired" (RFC 9111 §5.3):
/// nothing is stored and, like a past `Expires`, the feed stays on its
/// per-feed interval.
#[tokio::test]
async fn test_invalid_expires_gives_no_freshness() -> Result<()> {
    let (offset, stored) = next_fetch_offset_with_expires("0".into()).await?;
    assert_eq!(stored, None);
    assert!(
        (10_800..=10_810).contains(&offset),
        "should use the default 3h per-feed interval; got offset {offset}"
    );
    Ok(())
}

/// Spin up a server that fails once with 503, `Retry-After: retry_after`
/// and `Cache-Control: cache_control`, and return the scheduled
/// `next_fetch_at` offset from just before the refresh.
async fn retry_offset_with_stale_if_error(retry_after: &str, cache_control: &str) -> Result<i64> {
    let mut tc = TestBuilder::default().init_database().build()?;
    let state: SharedFeedServerState = Arc::new(Mutex::new(FeedServerState {
        fail_next: 1,
        fail_status: 503,
        fail_retry_after: Some(retry_after.into()),
        cache_control: Some(cache_control.into()),
        ..Default::default()
    }));
    tc.init_feed_server_with_state(state.clone()).await?;
    let (feed_id, client, pool) = setup_feed_for_cache_test(&tc).await?;

    let before = Utc::now().timestamp();
    refresh_once(&client, feed_id, &pool).await?;
    Ok(feed_column(&tc, feed_id, "next_fetch_at").expect("next_fetch_at set") - before)
}

/// A `Retry-After` that outlasts `stale-if-error` is cut short so we retry
/// before the grace window closes (RFC 5861 §4).
#[tokio::test]
async fn test_stale_if_error_caps_retry_after() -> Result<()> {
    let offset = retry_offset_with_stale_if_error("7200", "stale-if-error=1800").await?;
    assert!(
        (1795..=1810).contains(&offset),
        "stale-if-error=1800 should cap Retry-After: 7200; got offset {offset}"
    );
    Ok(())
}

/// A `Retry-After` inside the `stale-if-error` window is honored as is.
#[tokio::test]
async fn test_retry_after_within_stale_if_error_is_honored() -> Result<()> {
    let offset = retry_offset_with_stale_if_error("600", "stale-if-error=3600").await?;
    assert!(
        (595..=610).contains(&offset),
        "Retry-After: 600 should be honored; got offset {offset}"
    );
    Ok(())
}
