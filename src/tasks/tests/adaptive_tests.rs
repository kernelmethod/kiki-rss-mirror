//! Adaptive fetching end to end, through the bundled `adaptive-fetch`
//! plugin and the `fetch.schedule` event: a feed with a short freshness
//! hint that keeps turning out unchanged is backed off from, between the
//! minimum polling cadence and the feed's own interval.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::super::*;
use crate::config::Settings;
use crate::plugins::services::ServerServices;
use crate::scripting::lua::LuaScriptRunner;
use crate::scripting::{ScriptRunnerHandle, ScriptSource};
use crate::test::{FeedServerState, SharedFeedServerState, TestBuilder, TestConfig};
use anyhow::Result;
use chrono::Utc;
use rusqlite::OptionalExtension;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

const PLUGIN: &str = "adaptive-fetch";
const MAIN: &str = include_str!("../../../plugins/adaptive-fetch/main.lua");

/// Slack for assertions on `next_fetch_at`, covering the time the test
/// itself takes.
const SLACK: i64 = 5;

/// What a test refreshes a feed with.
struct Setup {
    tc: TestConfig,
    state: SharedFeedServerState,
    feed_id: i64,
    client: reqwest::Client,
    pool: crate::db::Db,
    /// The plugin, loaded with the config the test asked for, or `None` to
    /// refresh with no plugins at all.
    runner: Option<LuaScriptRunner>,
}

/// Start a feed server in `state`, insert a feed pointing at it with the
/// given fetch interval, and load the plugin with `config`, if given.
async fn setup(state: FeedServerState, config: Option<Value>, interval: i64) -> Result<Setup> {
    let mut tc = TestBuilder::default().init_database().build()?;
    let state: SharedFeedServerState = Arc::new(Mutex::new(state));
    tc.init_feed_server_with_state(state.clone()).await?;

    let conn = tc.database_conn()?;
    conn.execute(
        "INSERT INTO feeds (title, url, min_fetch_interval_seconds)
         VALUES ('adaptive test', ?1, ?2)",
        rusqlite::params![tc.rss_feed_url(), interval],
    )?;
    let feed_id = conn.last_insert_rowid();
    let client = reqwest::Client::builder()
        .user_agent(crate::http::USER_AGENT)
        .build()?;
    let pool = crate::db::Db::open(&tc.database_path(), Default::default())?;
    let runner = match config {
        Some(config) => {
            let services = ServerServices::new(
                pool.clone(),
                ScriptRunnerHandle::empty(),
                CancellationToken::new(),
            );
            services.set_loaded(HashSet::from([PLUGIN.to_string()]));
            let mut source = ScriptSource::new(MAIN);
            source.name = PLUGIN.to_string();
            source.config = config.to_string();
            Some(LuaScriptRunner::from_sources_with(
                &[source],
                Some(Arc::new(services)),
            )?)
        }
        None => None,
    };
    Ok(Setup {
        tc,
        state,
        feed_id,
        client,
        pool,
        runner,
    })
}

/// Set up with the plugin's default config.
async fn setup_default(state: FeedServerState, interval: i64) -> Result<Setup> {
    setup(state, Some(json!({})), interval).await
}

/// A server whose feed never changes and says it is never fresh.
fn max_age_zero() -> FeedServerState {
    FeedServerState {
        etag: Some("\"v1\"".into()),
        cache_control: Some("max-age=0".into()),
        ..Default::default()
    }
}

impl Setup {
    /// The feed's level in the plugin's store; zero when it has none.
    fn level(&self) -> Result<i64> {
        let stored: Option<String> = self
            .tc
            .database_conn()?
            .query_row(
                "SELECT value FROM plugin_store WHERE plugin = ?1 AND key = ?2",
                rusqlite::params![PLUGIN, format!("level:{}", self.feed_id)],
                |row| row.get(0),
            )
            .optional()?;
        Ok(stored.map_or(Ok(0), |v| v.parse())?)
    }

    /// Make the feed due, refresh it under `settings`, and return how far
    /// out its next fetch was scheduled and its level.
    async fn refresh_once(&self, settings: &Settings) -> Result<(i64, i64)> {
        self.tc.database_conn()?.execute(
            "UPDATE feeds SET next_fetch_at = NULL WHERE id = ?1",
            [self.feed_id],
        )?;
        let now = Utc::now().timestamp();
        refresh_feed_with_settings(
            &self.client,
            self.feed_id,
            self.pool.clone(),
            settings,
            self.runner
                .as_ref()
                .map(|r| r as &dyn crate::scripting::ScriptRunner),
            &super::test_metrics(),
            &super::test_tx(),
        )
        .await?;
        let next: i64 = self.tc.database_conn()?.query_row(
            "SELECT next_fetch_at FROM feeds WHERE id = ?1",
            [self.feed_id],
            |row| row.get(0),
        )?;
        Ok((next - now, self.level()?))
    }

    /// Refresh `n` times, returning each wait and level.
    async fn refresh_n(&self, settings: &Settings, n: usize) -> Result<Vec<(i64, i64)>> {
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            out.push(self.refresh_once(settings).await?);
        }
        Ok(out)
    }
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
    let s = setup_default(max_age_zero(), 600).await?;
    let waits = s.refresh_n(&Settings::default(), 6).await?;
    // The first fetch has nothing to compare against, so it stays at the
    // floor; 1m * 2^4 is past the 10m interval, so the level holds at 4.
    assert_waits(
        &waits,
        &[(60, 0), (120, 1), (240, 2), (480, 3), (600, 4), (600, 4)],
    );
    assert_eq!(s.state.lock().unwrap().not_modified_count, 5);
    Ok(())
}

/// A fetch that finds new content halves the wait.
#[tokio::test]
async fn test_changed_feed_comes_back_down() -> Result<()> {
    let s = setup_default(max_age_zero(), 86_400).await?;
    let settings = Settings::default();
    s.refresh_n(&settings, 4).await?;

    {
        let mut state = s.state.lock().unwrap();
        state.etag = Some("\"v2\"".into());
        state.body_override = Some(
            br#"<rss version="2.0"><channel><title>t</title><link>http://example.com/</link>
            <description>d</description><item><title>new</title><link>http://example.com/new</link><guid>new</guid></item>
            </channel></rss>"#
                .to_vec(),
        );
    }
    let waits = s.refresh_n(&settings, 2).await?;
    // From level 3: changed (2), then unchanged again (3).
    assert_waits(&waits, &[(240, 2), (480, 3)]);
    assert_eq!(s.state.lock().unwrap().full_response_count, 2);
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
    let s = setup_default(state, 86_400).await?;
    let waits = s.refresh_n(&Settings::default(), 3).await?;
    assert_waits(&waits, &[(60, 0), (120, 1), (240, 2)]);
    assert_eq!(s.state.lock().unwrap().full_response_count, 3);
    Ok(())
}

/// Without the plugin, a feed follows the hint every time.
#[tokio::test]
async fn test_without_the_plugin() -> Result<()> {
    let s = setup(max_age_zero(), None, 86_400).await?;
    let waits = s.refresh_n(&Settings::default(), 3).await?;
    assert_waits(&waits, &[(60, 0), (60, 0), (60, 0)]);
    Ok(())
}

/// A feed the plugin is told to leave alone does not adapt, and a level
/// left over from before is dropped.
#[tokio::test]
async fn test_excluded_feed() -> Result<()> {
    let s = setup(max_age_zero(), Some(json!({ "exclude": [1] })), 86_400).await?;
    assert_eq!(s.feed_id, 1);
    s.tc.database_conn()?.execute(
        "INSERT INTO plugin_store (plugin, key, value) VALUES (?1, 'level:1', '5')",
        [PLUGIN],
    )?;
    let waits = s.refresh_n(&Settings::default(), 3).await?;
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
    let s = setup_default(state, 3600).await?;
    let waits = s.refresh_n(&Settings::default(), 3).await?;
    assert_waits(&waits, &[(3600, 0), (3600, 0), (3600, 0)]);
    Ok(())
}

/// The minimum polling cadence is the starting point when it is above the
/// hint.
#[tokio::test]
async fn test_starts_from_the_polling_floor() -> Result<()> {
    let mut settings = Settings::default();
    settings.feed_fetch.min_polling_cadence_seconds = 300;
    let s = setup_default(max_age_zero(), 86_400).await?;
    let waits = s.refresh_n(&settings, 3).await?;
    assert_waits(&waits, &[(300, 0), (600, 1), (1200, 2)]);
    Ok(())
}

/// The wait a plugin asks for is held between the one planned and the
/// feed's interval: a plugin can have a feed fetched less often than its
/// server asks, never more often, and never less often than its interval.
#[tokio::test]
async fn test_plugin_waits_are_bounded() -> Result<()> {
    let s = setup(max_age_zero(), None, 3600).await?;
    let settings = Settings::default();
    for (asked, expected) in [(5, 60), (900, 900), (100_000, 3600)] {
        let mut source = ScriptSource::new(format!(
            "kiki.on('fetch.schedule', function() return {asked} end)"
        ));
        source.name = "fixed".to_string();
        let runner = LuaScriptRunner::from_sources(&[source])?;
        s.tc.database_conn()?.execute(
            "UPDATE feeds SET next_fetch_at = NULL WHERE id = ?1",
            [s.feed_id],
        )?;
        let now = Utc::now().timestamp();
        refresh_feed_with_settings(
            &s.client,
            s.feed_id,
            s.pool.clone(),
            &settings,
            Some(&runner),
            &super::test_metrics(),
            &super::test_tx(),
        )
        .await?;
        let next: i64 = s.tc.database_conn()?.query_row(
            "SELECT next_fetch_at FROM feeds WHERE id = ?1",
            [s.feed_id],
            |row| row.get(0),
        )?;
        assert_waits(&[(next - now, 0)], &[(expected, 0)]);
    }
    Ok(())
}

/// A plugin that fails leaves the planned schedule in force.
#[tokio::test]
async fn test_failing_plugin_keeps_the_planned_wait() -> Result<()> {
    let s = setup(max_age_zero(), None, 3600).await?;
    let mut source = ScriptSource::new("kiki.on('fetch.schedule', function() error('boom') end)");
    source.name = "broken".to_string();
    let runner = LuaScriptRunner::from_sources(&[source])?;
    let now = Utc::now().timestamp();
    refresh_feed_with_settings(
        &s.client,
        s.feed_id,
        s.pool.clone(),
        &Settings::default(),
        Some(&runner),
        &super::test_metrics(),
        &super::test_tx(),
    )
    .await?;
    let next: i64 = s.tc.database_conn()?.query_row(
        "SELECT next_fetch_at FROM feeds WHERE id = ?1",
        [s.feed_id],
        |row| row.get(0),
    )?;
    assert_waits(&[(next - now, 0)], &[(60, 0)]);
    Ok(())
}
