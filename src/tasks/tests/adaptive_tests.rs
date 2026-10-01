//! Adaptive fetching end to end: a feed with a short freshness hint that
//! keeps turning out unchanged is backed off from, between the minimum
//! polling cadence and the feed's own interval.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::super::*;
use crate::config::Settings;
use crate::test::{FeedServerState, SharedFeedServerState, TestBuilder, TestConfig};
use anyhow::Result;
use chrono::Utc;
use std::sync::{Arc, Mutex};

/// Slack for assertions on `next_fetch_at`, covering the time the test
/// itself takes.
const SLACK: i64 = 5;

/// Start a feed server in `state` and insert a feed pointing at it, with
/// the given per-feed `adaptive_fetch` column and fetch interval.
async fn setup(
    state: FeedServerState,
    adaptive_fetch: Option<bool>,
    interval: i64,
) -> Result<(
    TestConfig,
    SharedFeedServerState,
    i64,
    reqwest::Client,
    crate::db::Db,
)> {
    let mut tc = TestBuilder::default().init_database().build()?;
    let state: SharedFeedServerState = Arc::new(Mutex::new(state));
    tc.init_feed_server_with_state(state.clone()).await?;

    let conn = tc.database_conn()?;
    conn.execute(
        "INSERT INTO feeds (title, url, adaptive_fetch, min_fetch_interval_seconds)
         VALUES ('adaptive test', ?1, ?2, ?3)",
        rusqlite::params![tc.rss_feed_url(), adaptive_fetch, interval],
    )?;
    let feed_id = conn.last_insert_rowid();
    let client = reqwest::Client::builder()
        .user_agent(crate::http::USER_AGENT)
        .build()?;
    let pool = crate::db::Db::open(&tc.database_path(), Default::default())?;
    Ok((tc, state, feed_id, client, pool))
}

/// A server whose feed never changes and says it is never fresh.
fn max_age_zero() -> FeedServerState {
    FeedServerState {
        etag: Some("\"v1\"".into()),
        cache_control: Some("max-age=0".into()),
        ..Default::default()
    }
}

/// Make the feed due, refresh it under `settings`, and return how far out
/// its next fetch was scheduled and its adaptive level.
async fn refresh_once(
    tc: &TestConfig,
    client: &reqwest::Client,
    feed_id: i64,
    pool: &crate::db::Db,
    settings: &Settings,
) -> Result<(i64, i64)> {
    tc.database_conn()?.execute(
        "UPDATE feeds SET next_fetch_at = NULL WHERE id = ?1",
        [feed_id],
    )?;
    let now = Utc::now().timestamp();
    refresh_feed_with_settings(
        client,
        feed_id,
        pool.clone(),
        settings,
        None,
        &super::test_metrics(),
        &super::test_tx(),
    )
    .await?;
    let (next, level): (i64, i64) = tc.database_conn()?.query_row(
        "SELECT next_fetch_at, adaptive_fetch_level FROM feeds WHERE id = ?1",
        [feed_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok((next - now, level))
}

/// Refresh `n` times, returning each wait and level.
async fn refresh_n(
    tc: &TestConfig,
    client: &reqwest::Client,
    feed_id: i64,
    pool: &crate::db::Db,
    settings: &Settings,
    n: usize,
) -> Result<Vec<(i64, i64)>> {
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(refresh_once(tc, client, feed_id, pool, settings).await?);
    }
    Ok(out)
}

fn assert_waits(actual: &[(i64, i64)], expected: &[(i64, i64)]) {
    assert_eq!(actual.len(), expected.len());
    for (i, (&(wait, level), &(want_wait, want_level))) in actual.iter().zip(expected).enumerate() {
        assert!(
            (want_wait - SLACK..=want_wait + SLACK).contains(&wait) && level == want_level,
            "fetch {i}: expected ~{want_wait}s at level {want_level}, got {wait}s at level {level}; all: {actual:?}"
        );
    }
}

/// Each 304 doubles the wait from the 1m polling floor, and the wait
/// stops at the feed's own interval.
#[tokio::test]
async fn test_unchanged_feed_backs_off_up_to_its_interval() -> Result<()> {
    let (tc, state, feed_id, client, pool) = setup(max_age_zero(), None, 600).await?;
    let waits = refresh_n(&tc, &client, feed_id, &pool, &Settings::default(), 6).await?;
    // The first fetch has nothing to compare against, so it stays at the
    // floor; 1m * 2^4 is past the 10m interval, so the level holds at 4.
    assert_waits(
        &waits,
        &[(60, 0), (120, 1), (240, 2), (480, 3), (600, 4), (600, 4)],
    );
    assert_eq!(state.lock().unwrap().not_modified_count, 5);
    Ok(())
}

/// A fetch that finds new content halves the wait.
#[tokio::test]
async fn test_changed_feed_comes_back_down() -> Result<()> {
    let (tc, state, feed_id, client, pool) = setup(max_age_zero(), None, 86_400).await?;
    let settings = Settings::default();
    refresh_n(&tc, &client, feed_id, &pool, &settings, 4).await?;

    {
        let mut s = state.lock().unwrap();
        s.etag = Some("\"v2\"".into());
        s.body_override = Some(
            br#"<rss version="2.0"><channel><title>t</title><link>http://example.com/</link>
            <description>d</description><item><title>new</title><link>http://example.com/new</link><guid>new</guid></item>
            </channel></rss>"#
                .to_vec(),
        );
    }
    let waits = refresh_n(&tc, &client, feed_id, &pool, &settings, 2).await?;
    // From level 3: changed (2), then unchanged again (3).
    assert_waits(&waits, &[(240, 2), (480, 3)]);
    assert_eq!(state.lock().unwrap().full_response_count, 2);
    Ok(())
}

/// A server that ignores conditional requests is judged by its body: the
/// same body again counts as unchanged.
#[tokio::test]
async fn test_identical_200_counts_as_unchanged() -> Result<()> {
    let state = FeedServerState {
        cache_control: Some("max-age=0".into()),
        ..Default::default()
    };
    let (tc, state, feed_id, client, pool) = setup(state, None, 86_400).await?;
    let waits = refresh_n(&tc, &client, feed_id, &pool, &Settings::default(), 3).await?;
    assert_waits(&waits, &[(60, 0), (120, 1), (240, 2)]);
    assert_eq!(state.lock().unwrap().full_response_count, 3);
    Ok(())
}

/// With the server setting off, a feed that has no setting of its own
/// follows the hint every time; one turned on for itself still adapts.
#[tokio::test]
async fn test_server_setting_off() -> Result<()> {
    let mut settings = Settings::default();
    settings.feed_fetch.adaptive_fetch = false;

    let (tc, _state, feed_id, client, pool) = setup(max_age_zero(), None, 86_400).await?;
    let waits = refresh_n(&tc, &client, feed_id, &pool, &settings, 3).await?;
    assert_waits(&waits, &[(60, 0), (60, 0), (60, 0)]);

    let (tc, _state, feed_id, client, pool) = setup(max_age_zero(), Some(true), 86_400).await?;
    let waits = refresh_n(&tc, &client, feed_id, &pool, &settings, 3).await?;
    assert_waits(&waits, &[(60, 0), (120, 1), (240, 2)]);
    Ok(())
}

/// A feed turned off for itself does not adapt, and a level left over from
/// before is dropped.
#[tokio::test]
async fn test_feed_setting_off() -> Result<()> {
    let (tc, _state, feed_id, client, pool) = setup(max_age_zero(), Some(false), 86_400).await?;
    tc.database_conn()?.execute(
        "UPDATE feeds SET adaptive_fetch_level = 5 WHERE id = ?1",
        [feed_id],
    )?;
    let waits = refresh_n(&tc, &client, feed_id, &pool, &Settings::default(), 3).await?;
    assert_waits(&waits, &[(60, 0), (60, 0), (60, 0)]);
    Ok(())
}

/// A hint at least as long as the feed's interval leaves nothing to adapt:
/// the feed waits its interval and keeps level zero.
#[tokio::test]
async fn test_no_short_hint_is_left_alone() -> Result<()> {
    let state = FeedServerState {
        etag: Some("\"v1\"".into()),
        cache_control: Some("max-age=7200".into()),
        ..Default::default()
    };
    let (tc, _state, feed_id, client, pool) = setup(state, None, 3600).await?;
    let waits = refresh_n(&tc, &client, feed_id, &pool, &Settings::default(), 3).await?;
    assert_waits(&waits, &[(3600, 0), (3600, 0), (3600, 0)]);
    Ok(())
}

/// The minimum polling cadence is the starting point when it is above the
/// hint.
#[tokio::test]
async fn test_starts_from_the_polling_floor() -> Result<()> {
    let mut settings = Settings::default();
    settings.feed_fetch.min_polling_cadence_seconds = 300;
    let (tc, _state, feed_id, client, pool) = setup(max_age_zero(), None, 86_400).await?;
    let waits = refresh_n(&tc, &client, feed_id, &pool, &settings, 3).await?;
    assert_waits(&waits, &[(300, 0), (600, 1), (1200, 2)]);
    Ok(())
}
