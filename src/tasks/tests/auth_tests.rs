#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::super::*;
use crate::http::{FeedAuth, FeedAuthType};
use crate::test::{FeedServerState, SharedFeedServerState, TestBuilder};
use anyhow::Result;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::OpenFlags;
use std::sync::{Arc, Mutex};

fn make_pool(path: &std::path::Path) -> Result<r2d2::Pool<SqliteConnectionManager>> {
    let manager = SqliteConnectionManager::file(path)
        .with_flags(OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .with_init(|c| c.execute_batch("PRAGMA foreign_keys=ON;"));
    Ok(r2d2::Pool::new(manager)?)
}

/// Insert a feed pointing at the test feed server with the given auth
/// configuration, returning its id plus a client and pool for use with
/// [`refresh_feed`].
async fn setup_feed_with_auth(
    tc: &crate::test::TestConfig,
    auth: &FeedAuth,
) -> Result<(i64, reqwest::Client, r2d2::Pool<SqliteConnectionManager>)> {
    let conn = tc.database_conn()?;
    conn.execute(
        "INSERT INTO feeds
            (title, url, auth_type, auth_username, auth_password, auth_bearer_token)
         VALUES ('auth test feed', ?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            tc.rss_feed_url(),
            auth.auth_type.as_db(),
            auth.username,
            auth.password,
            auth.bearer_token,
        ],
    )?;
    let feed_id = conn.last_insert_rowid();

    let client = reqwest::Client::builder()
        .user_agent(crate::http::USER_AGENT)
        .build()?;
    let pool = make_pool(&tc.database_path())?;
    Ok((feed_id, client, pool))
}

#[tokio::test]
async fn test_basic_auth_header_sent() -> Result<()> {
    let mut tc = TestBuilder::default().init_database().build()?;

    // "alice:hunter2" -> base64 -> "YWxpY2U6aHVudGVyMg=="
    let state: SharedFeedServerState = Arc::new(Mutex::new(FeedServerState {
        require_authorization: Some("Basic YWxpY2U6aHVudGVyMg==".into()),
        ..Default::default()
    }));
    tc.init_feed_server_with_state(state.clone()).await?;

    let auth = FeedAuth {
        auth_type: FeedAuthType::Basic,
        username: Some("alice".into()),
        password: Some("hunter2".into()),
        bearer_token: None,
    };
    let (feed_id, client, pool) = setup_feed_with_auth(&tc, &auth).await?;

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
        s.unauthorized_count, 0,
        "server returned 401 unexpectedly; last Authorization was {:?}",
        s.last_authorization
    );
    assert_eq!(
        s.last_authorization.as_deref(),
        Some("Basic YWxpY2U6aHVudGVyMg==")
    );
    assert_eq!(s.full_response_count, 1);
    Ok(())
}

#[tokio::test]
async fn test_bearer_auth_header_sent() -> Result<()> {
    let mut tc = TestBuilder::default().init_database().build()?;

    let state: SharedFeedServerState = Arc::new(Mutex::new(FeedServerState {
        require_authorization: Some("Bearer s3cret-token".into()),
        ..Default::default()
    }));
    tc.init_feed_server_with_state(state.clone()).await?;

    let auth = FeedAuth {
        auth_type: FeedAuthType::Bearer,
        username: None,
        password: None,
        bearer_token: Some("s3cret-token".into()),
    };
    let (feed_id, client, pool) = setup_feed_with_auth(&tc, &auth).await?;

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
        s.unauthorized_count, 0,
        "server returned 401 unexpectedly; last Authorization was {:?}",
        s.last_authorization
    );
    assert_eq!(s.last_authorization.as_deref(), Some("Bearer s3cret-token"));
    assert_eq!(s.full_response_count, 1);
    Ok(())
}

#[tokio::test]
async fn test_missing_auth_records_fetch_error() -> Result<()> {
    let mut tc = TestBuilder::default().init_database().build()?;

    // Server requires auth, but we configure the feed without any.
    let state: SharedFeedServerState = Arc::new(Mutex::new(FeedServerState {
        require_authorization: Some("Basic dXNlcjpwYXNz".into()),
        ..Default::default()
    }));
    tc.init_feed_server_with_state(state.clone()).await?;

    let auth = FeedAuth::default();
    let (feed_id, client, pool) = setup_feed_with_auth(&tc, &auth).await?;

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
    assert_eq!(s.unauthorized_count, 1);
    assert_eq!(s.last_authorization, None);
    drop(s);

    // The fetch error should have been recorded on the feed row.
    let conn = tc.database_conn()?;
    let last_error: Option<String> = conn.query_row(
        "SELECT last_fetch_error FROM feeds WHERE id = ?1",
        [feed_id],
        |row| row.get(0),
    )?;
    let last_error = last_error.expect("fetch error should be persisted on 401");
    assert!(
        last_error.contains("401"),
        "expected error to mention 401, got {last_error}"
    );

    Ok(())
}

#[tokio::test]
async fn test_no_auth_sends_no_authorization_header() -> Result<()> {
    let mut tc = TestBuilder::default().init_database().build()?;

    let state: SharedFeedServerState = Arc::new(Mutex::new(FeedServerState::default()));
    tc.init_feed_server_with_state(state.clone()).await?;

    let auth = FeedAuth::default();
    let (feed_id, client, pool) = setup_feed_with_auth(&tc, &auth).await?;

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
        s.last_authorization, None,
        "unauthenticated feed should not send an Authorization header"
    );
    Ok(())
}
