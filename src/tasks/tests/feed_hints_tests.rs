//! Refresh hints declared inside the feed document (RSS `<ttl>`,
//! `<skipHours>`, `<skipDays>`, and the Syndication module) driving the
//! fetch schedule end to end.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::super::*;
use crate::test::{FeedServerState, SharedFeedServerState, TestBuilder, TestConfig};
use anyhow::Result;
use chrono::{DateTime, Timelike, Utc};
use std::sync::{Arc, Mutex};

/// Slack for assertions on `next_fetch_at`, covering the time the test
/// itself takes and `Date`-header rounding.
const SLACK: i64 = 10;

fn make_pool(path: &std::path::Path) -> Result<crate::db::Db> {
    crate::db::Db::open(path, Default::default())
}

/// An RSS body whose `<channel>` carries `channel_extra`.
fn rss_body(channel_extra: &str) -> Vec<u8> {
    format!(
        r#"<rss version="2.0" xmlns:sy="http://purl.org/rss/1.0/modules/syndication/">
        <channel><title>hints</title><link>http://example.com/</link>
        <description>d</description>{channel_extra}
        <item><title>one</title><link>http://example.com/1</link><guid>g1</guid></item></channel></rss>"#
    )
    .into_bytes()
}

/// Start a feed server in `state` and insert a feed pointing at it.
async fn setup(
    state: FeedServerState,
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
    // Adaptive fetching would stretch hints after a 304; these tests are
    // about the hints themselves (see adaptive_tests for the stretching).
    conn.execute(
        "INSERT INTO feeds (title, url, adaptive_fetch) VALUES ('feed hints test', ?1, 0)",
        [tc.rss_feed_url()],
    )?;
    let feed_id = conn.last_insert_rowid();
    let client = reqwest::Client::builder()
        .user_agent(crate::http::USER_AGENT)
        .build()?;
    let pool = make_pool(&tc.database_path())?;
    Ok((tc, state, feed_id, client, pool))
}

async fn refresh(client: &reqwest::Client, feed_id: i64, pool: &crate::db::Db) -> Result<()> {
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

fn next_fetch_at(tc: &TestConfig, feed_id: i64) -> i64 {
    tc.database_conn()
        .unwrap()
        .query_row(
            "SELECT next_fetch_at FROM feeds WHERE id = ?1",
            [feed_id],
            |row| row.get(0),
        )
        .unwrap()
}

fn make_eligible(tc: &TestConfig, feed_id: i64) {
    tc.database_conn()
        .unwrap()
        .execute(
            "UPDATE feeds SET next_fetch_at = NULL WHERE id = ?1",
            [feed_id],
        )
        .unwrap();
}

fn assert_near(actual: i64, expected: i64, what: &str) {
    assert!(
        (expected - SLACK..=expected + SLACK).contains(&actual),
        "{what}: expected ~{expected}, got {actual}"
    );
}

/// A `<skipHours>` mask allowing only the hour `hours_ahead` hours from now.
fn only_hour_allowed(hours_ahead: i64) -> (u32, String) {
    let allowed = (Utc::now() + chrono::Duration::hours(hours_ahead)).hour();
    let hours: String = (0..24)
        .filter(|h| *h != allowed)
        .map(|h| format!("<hour>{h}</hour>"))
        .collect();
    (allowed, format!("<skipHours>{hours}</skipHours>"))
}

fn assert_on_hour(ts: i64, hour: u32) {
    let dt = DateTime::<Utc>::from_timestamp(ts, 0).unwrap();
    assert_eq!(dt.hour(), hour, "deferred to the wrong hour: {dt}");
    assert_eq!(ts % 3600, 0, "deferred fetch should land on the hour: {dt}");
    assert!(ts > Utc::now().timestamp());
}

/// With no HTTP freshness headers, `<ttl>` schedules the next fetch, and
/// every hint is stored on the feed row.
#[tokio::test]
async fn test_ttl_schedules_next_fetch_without_http_hints() -> Result<()> {
    let (tc, _state, feed_id, client, pool) = setup(FeedServerState {
        body_override: Some(rss_body(
            "<ttl>30</ttl><sy:updatePeriod>hourly</sy:updatePeriod>\
             <sy:updateFrequency>4</sy:updateFrequency>\
             <skipHours><hour>3</hour></skipHours><skipDays><day>Sunday</day></skipDays>",
        )),
        ..Default::default()
    })
    .await?;

    let now = Utc::now().timestamp();
    refresh(&client, feed_id, &pool).await?;

    let (ttl, interval, skip_hours, skip_days): (Option<i64>, Option<i64>, i64, i64) =
        tc.database_conn()?.query_row(
            "SELECT feed_ttl_seconds, feed_update_interval_seconds,
                    feed_skip_hours, feed_skip_days
             FROM feeds WHERE id = ?1",
            [feed_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
    assert_eq!(ttl, Some(1800));
    assert_eq!(interval, Some(900));
    assert_eq!(skip_hours, 1 << 3);
    assert_eq!(skip_days, 1 << 6);

    // The longer of <ttl> (30m) and sy:* (15m), unless that lands in a
    // skipped hour or day.
    let expected = backoff::defer_past_skipped(now + 1800, 1 << 3, 1 << 6);
    assert_near(next_fetch_at(&tc, feed_id), expected, "next_fetch_at");
    Ok(())
}

/// An HTTP `max-age` takes precedence over `<ttl>`.
#[tokio::test]
async fn test_http_max_age_wins_over_ttl() -> Result<()> {
    let (tc, _state, feed_id, client, pool) = setup(FeedServerState {
        cache_control: Some("max-age=600".into()),
        body_override: Some(rss_body("<ttl>60</ttl>")),
        ..Default::default()
    })
    .await?;

    let now = Utc::now().timestamp();
    refresh(&client, feed_id, &pool).await?;
    assert_near(next_fetch_at(&tc, feed_id), now + 600, "next_fetch_at");
    Ok(())
}

/// `Cache-Control: no-cache` gives no freshness hint, so `<ttl>` applies.
#[tokio::test]
async fn test_ttl_applies_under_no_cache() -> Result<()> {
    let (tc, _state, feed_id, client, pool) = setup(FeedServerState {
        cache_control: Some("no-cache".into()),
        body_override: Some(rss_body("<ttl>20</ttl>")),
        ..Default::default()
    })
    .await?;

    let now = Utc::now().timestamp();
    refresh(&client, feed_id, &pool).await?;
    assert_near(next_fetch_at(&tc, feed_id), now + 1200, "next_fetch_at");
    Ok(())
}

/// A 304 has no body to read hints from, so the stored `<ttl>` from the
/// last 200 schedules the next fetch.
#[tokio::test]
async fn test_not_modified_uses_stored_ttl() -> Result<()> {
    let (tc, state, feed_id, client, pool) = setup(FeedServerState {
        etag: Some("\"v1\"".into()),
        body_override: Some(rss_body("<ttl>45</ttl>")),
        ..Default::default()
    })
    .await?;

    refresh(&client, feed_id, &pool).await?;
    make_eligible(&tc, feed_id);

    let now = Utc::now().timestamp();
    refresh(&client, feed_id, &pool).await?;
    assert_eq!(state.lock().unwrap().not_modified_count, 1);
    assert_near(next_fetch_at(&tc, feed_id), now + 2700, "next_fetch_at");
    Ok(())
}

/// A feed that drops its `<ttl>` stops being scheduled by it.
#[tokio::test]
async fn test_removed_ttl_is_cleared() -> Result<()> {
    let (tc, state, feed_id, client, pool) = setup(FeedServerState {
        body_override: Some(rss_body("<ttl>30</ttl>")),
        ..Default::default()
    })
    .await?;

    refresh(&client, feed_id, &pool).await?;
    state.lock().unwrap().body_override = Some(rss_body(""));
    make_eligible(&tc, feed_id);

    let now = Utc::now().timestamp();
    refresh(&client, feed_id, &pool).await?;
    let ttl: Option<i64> = tc.database_conn()?.query_row(
        "SELECT feed_ttl_seconds FROM feeds WHERE id = ?1",
        [feed_id],
        |row| row.get(0),
    )?;
    assert_eq!(ttl, None);
    // Back to the default 24-hour per-feed interval.
    assert_near(next_fetch_at(&tc, feed_id), now + 86_400, "next_fetch_at");
    Ok(())
}

/// A 200 whose body does not parse leaves the stored hints alone.
#[tokio::test]
async fn test_unparseable_body_keeps_stored_hints() -> Result<()> {
    let (tc, state, feed_id, client, pool) = setup(FeedServerState {
        body_override: Some(rss_body("<ttl>30</ttl>")),
        ..Default::default()
    })
    .await?;

    refresh(&client, feed_id, &pool).await?;
    state.lock().unwrap().body_override = Some(b"<html>not a feed</html>".to_vec());
    make_eligible(&tc, feed_id);
    refresh(&client, feed_id, &pool).await?;

    let ttl: Option<i64> = tc.database_conn()?.query_row(
        "SELECT feed_ttl_seconds FROM feeds WHERE id = ?1",
        [feed_id],
        |row| row.get(0),
    )?;
    assert_eq!(ttl, Some(1800));
    Ok(())
}

/// `<skipHours>` pushes the next fetch out of the skipped hours.
#[tokio::test]
async fn test_skip_hours_defers_next_fetch() -> Result<()> {
    let (allowed, skip) = only_hour_allowed(5);
    let (tc, _state, feed_id, client, pool) = setup(FeedServerState {
        body_override: Some(rss_body(&skip)),
        ..Default::default()
    })
    .await?;

    refresh(&client, feed_id, &pool).await?;
    assert_on_hour(next_fetch_at(&tc, feed_id), allowed);
    Ok(())
}

/// A 304 applies the stored `<skipHours>` too.
#[tokio::test]
async fn test_not_modified_applies_stored_skip_hours() -> Result<()> {
    let (allowed, skip) = only_hour_allowed(6);
    let (tc, state, feed_id, client, pool) = setup(FeedServerState {
        etag: Some("\"v1\"".into()),
        body_override: Some(rss_body(&skip)),
        ..Default::default()
    })
    .await?;

    refresh(&client, feed_id, &pool).await?;
    make_eligible(&tc, feed_id);
    refresh(&client, feed_id, &pool).await?;
    assert_eq!(state.lock().unwrap().not_modified_count, 1);
    assert_on_hour(next_fetch_at(&tc, feed_id), allowed);
    Ok(())
}

/// Retries after an error also stay out of the skipped hours.
#[tokio::test]
async fn test_error_retry_applies_stored_skip_hours() -> Result<()> {
    let (allowed, skip) = only_hour_allowed(4);
    let (tc, state, feed_id, client, pool) = setup(FeedServerState {
        body_override: Some(rss_body(&skip)),
        ..Default::default()
    })
    .await?;

    refresh(&client, feed_id, &pool).await?;
    {
        let mut s = state.lock().unwrap();
        s.fail_next = 1;
        s.fail_status = 500;
    }
    make_eligible(&tc, feed_id);
    refresh(&client, feed_id, &pool).await?;

    let failures: i64 = tc.database_conn()?.query_row(
        "SELECT consecutive_failures FROM feeds WHERE id = ?1",
        [feed_id],
        |row| row.get(0),
    )?;
    assert_eq!(failures, 1);
    assert_on_hour(next_fetch_at(&tc, feed_id), allowed);
    Ok(())
}
