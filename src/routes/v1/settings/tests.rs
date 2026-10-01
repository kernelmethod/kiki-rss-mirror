#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
//! HTTP-level tests for the settings routes.

use crate::test::TestBuilder;
use anyhow::Result;
use axum::http::StatusCode;

#[tokio::test]
async fn feed_fetch_settings_roundtrip() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    let client = tc.client()?;

    // Default state: the built-in default.
    let resp = client
        .get("http://localhost/v1/settings/feed-fetch")
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await?;
    assert_eq!(
        body["max_feed_bytes"].as_u64().unwrap(),
        crate::config::DEFAULT_MAX_FEED_BYTES
    );

    // Lower the cap.
    let resp = client
        .put("http://localhost/v1/settings/feed-fetch")
        .json(&serde_json::json!({"max_feed_bytes": 4096}))
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await?;
    assert_eq!(body["max_feed_bytes"], 4096);

    // The change is durable, not just echoed back.
    let resp = client
        .get("http://localhost/v1/settings/feed-fetch")
        .send()
        .await?;
    let body: serde_json::Value = resp.json().await?;
    assert_eq!(body["max_feed_bytes"], 4096);

    Ok(())
}

#[tokio::test]
async fn feed_fetch_settings_reject_a_zero_cap() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    let client = tc.client()?;

    let resp = client
        .put("http://localhost/v1/settings/feed-fetch")
        .json(&serde_json::json!({"max_feed_bytes": 0}))
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // And the stored value is untouched.
    let resp = client
        .get("http://localhost/v1/settings/feed-fetch")
        .send()
        .await?;
    let body: serde_json::Value = resp.json().await?;
    assert_eq!(
        body["max_feed_bytes"].as_u64().unwrap(),
        crate::config::DEFAULT_MAX_FEED_BYTES
    );

    Ok(())
}

#[tokio::test]
async fn feed_fetch_settings_omitted_field_is_a_no_op() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    let client = tc.client()?;

    client
        .put("http://localhost/v1/settings/feed-fetch")
        .json(&serde_json::json!({"max_feed_bytes": 4096}))
        .send()
        .await?;

    let resp = client
        .put("http://localhost/v1/settings/feed-fetch")
        .json(&serde_json::json!({"max_feed_bytes": null}))
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await?;
    assert_eq!(
        body["max_feed_bytes"], 4096,
        "null should leave the value alone"
    );

    Ok(())
}

#[tokio::test]
async fn default_fetch_interval_applies_to_new_feeds_only() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    let client = tc.client()?;

    let body = get_json(&client, "/v1/settings/feed-fetch").await?;
    assert_eq!(
        body["default_fetch_interval_seconds"].as_u64().unwrap(),
        crate::config::DEFAULT_FETCH_INTERVAL_SECONDS
    );

    let create = |url: &'static str| {
        client
            .post("http://localhost/v1/feeds/create")
            .json(&serde_json::json!({"title": "feed", "url": url}))
            .send()
    };
    let resp = create("https://example.com/before.xml").await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let before: serde_json::Value = resp.json().await?;

    let resp = client
        .put("http://localhost/v1/settings/feed-fetch")
        .json(&serde_json::json!({"default_fetch_interval_seconds": 900}))
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await?;
    assert_eq!(body["default_fetch_interval_seconds"], 900);

    let resp = create("https://example.com/after.xml").await?;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let after: serde_json::Value = resp.json().await?;

    let feed = get_json(&client, &format!("/v1/feeds/id/{}", after["id"])).await?;
    assert_eq!(feed["min_fetch_interval_seconds"], 900);
    // A feed added before the change keeps the interval it was given.
    let feed = get_json(&client, &format!("/v1/feeds/id/{}", before["id"])).await?;
    assert_eq!(
        feed["min_fetch_interval_seconds"].as_u64().unwrap(),
        crate::config::DEFAULT_FETCH_INTERVAL_SECONDS
    );

    // Zero is rejected.
    let resp = client
        .put("http://localhost/v1/settings/feed-fetch")
        .json(&serde_json::json!({"default_fetch_interval_seconds": 0}))
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    Ok(())
}

fn config_path(tc: &crate::test::TestConfig) -> std::path::PathBuf {
    tc.database_path()
        .with_file_name(crate::config::CONFIG_FILE_NAME)
}

async fn get_json(client: &reqwest::Client, path: &str) -> Result<serde_json::Value> {
    let resp = client.get(format!("http://localhost{path}")).send().await?;
    assert_eq!(resp.status(), StatusCode::OK);
    Ok(resp.json().await?)
}

#[tokio::test]
async fn updates_are_saved_to_the_config_file() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    let client = tc.client()?;
    assert!(!config_path(&tc).exists(), "no overrides, no file");

    let resp = client
        .put("http://localhost/v1/settings/feed-fetch")
        .json(&serde_json::json!({"timeout_seconds": 30, "max_feed_bytes": 4096}))
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = client
        .put("http://localhost/v1/settings/asset-cache")
        .json(&serde_json::json!({"enabled": false}))
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::OK);

    let text = std::fs::read_to_string(config_path(&tc))?;
    let overrides = crate::config::Overrides::parse(&text)?;
    let settings = overrides.resolve()?;
    assert_eq!(settings.feed_fetch.timeout_seconds, 30);
    assert_eq!(settings.feed_fetch.max_feed_bytes, 4096);
    assert!(!settings.asset_cache.enabled);
    // Untouched settings are left to their defaults, not written out.
    assert!(overrides.get("feed_fetch", "max_backoff_seconds").is_none());
    assert!(overrides.get("asset_cache", "max_bytes").is_none());

    Ok(())
}

#[tokio::test]
async fn retention_can_be_set_and_cleared() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    let client = tc.client()?;

    let resp = client
        .put("http://localhost/v1/settings/retention")
        .json(&serde_json::json!({"max_age_days": 30}))
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = get_json(&client, "/v1/settings/retention").await?;
    assert_eq!(body["max_age_days"], 30);

    let resp = client
        .put("http://localhost/v1/settings/retention")
        .json(&serde_json::json!({"max_age_days": null}))
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = get_json(&client, "/v1/settings/retention").await?;
    assert!(body["max_age_days"].is_null());

    let resp = client
        .put("http://localhost/v1/settings/retention")
        .json(&serde_json::json!({"max_age_days": 0}))
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    Ok(())
}

#[tokio::test]
async fn edits_to_the_config_file_reach_the_running_server() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    let client = tc.client()?;
    // Make sure the server (and so its watcher) is up before editing.
    get_json(&client, "/v1/settings/feed-fetch").await?;

    std::fs::write(config_path(&tc), "[feed_fetch]\nmax_feed_bytes = 1234\n")?;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let body = get_json(&client, "/v1/settings/feed-fetch").await?;
        if body["max_feed_bytes"] == 1234 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "edit was not picked up"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    Ok(())
}

#[tokio::test]
async fn an_invalid_file_on_disk_is_a_conflict() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    let client = tc.client()?;
    get_json(&client, "/v1/settings/asset-cache").await?;

    for bad in ["[retention]\nmax_age_days = 0\n", "[retention\n"] {
        std::fs::write(config_path(&tc), bad)?;
        let resp = client
            .put("http://localhost/v1/settings/asset-cache")
            .json(&serde_json::json!({"enabled": false}))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::CONFLICT, "{bad:?}");
        assert!(resp
            .text()
            .await?
            .contains("config file on disk is invalid"));
        assert_eq!(std::fs::read_to_string(config_path(&tc))?, bad);
    }

    Ok(())
}

#[tokio::test]
async fn lowering_max_backoff_brings_scheduled_fetches_forward() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    let client = tc.client()?;
    let now = chrono::Utc::now().timestamp();

    let conn = tc.database_conn()?;
    for (url, next_fetch_at) in [
        ("https://example.com/soon.xml", now + 600),
        ("https://example.com/far.xml", now + 86400),
    ] {
        conn.execute(
            "INSERT INTO feeds (title, url, next_fetch_at) VALUES ('feed', ?1, ?2)",
            rusqlite::params![url, next_fetch_at],
        )?;
    }

    let resp = client
        .put("http://localhost/v1/settings/feed-fetch")
        .json(&serde_json::json!({"max_backoff_seconds": 3600}))
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::OK);

    let next_fetch_at = |url: &str| -> Result<i64> {
        Ok(conn.query_row(
            "SELECT next_fetch_at FROM feeds WHERE url = ?1",
            [url],
            |row| row.get(0),
        )?)
    };
    // Already inside the new cap: unchanged.
    assert_eq!(next_fetch_at("https://example.com/soon.xml")?, now + 600);
    // Beyond it: brought forward to about an hour from now.
    let far = next_fetch_at("https://example.com/far.xml")?;
    assert!(
        (now + 3600..now + 3700).contains(&far),
        "expected ~{}, got {far}",
        now + 3600
    );

    Ok(())
}

/// `adaptive_fetch` is on by default, and turning it off brings in the
/// fetches it had put off for feeds without a setting of their own.
#[tokio::test]
async fn adaptive_fetch_setting_unwinds_feeds_that_follow_it() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    let client = tc.client()?;

    let resp = client
        .get("http://localhost/v1/settings/feed-fetch")
        .send()
        .await?;
    let body: serde_json::Value = resp.json().await?;
    assert_eq!(body["adaptive_fetch"], true);

    // Both are due in the future, so the scheduler leaves them alone.
    let last_checked = chrono::Utc::now().timestamp() - 10;
    let next = last_checked + 3000;
    let conn = tc.database_conn()?;
    conn.execute(
        "INSERT INTO feeds (id, title, url, adaptive_fetch, adaptive_fetch_level,
                            last_checked, next_fetch_at)
         VALUES (1, 'follows', 'http://a/', NULL, 5, ?1, ?2),
                (2, 'own setting', 'http://b/', 1, 5, ?1, ?2)",
        [last_checked, next],
    )?;

    let resp = client
        .put("http://localhost/v1/settings/feed-fetch")
        .json(&serde_json::json!({"adaptive_fetch": false}))
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await?;
    assert_eq!(body["adaptive_fetch"], false);

    let rows: Vec<(i64, i64, i64)> = conn
        .prepare("SELECT id, adaptive_fetch_level, next_fetch_at FROM feeds ORDER BY id")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<Result<_, _>>()?;
    assert_eq!(rows, [(1, 0, last_checked + 60), (2, 5, next)]);
    Ok(())
}
