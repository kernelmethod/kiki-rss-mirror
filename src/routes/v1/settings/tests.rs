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
