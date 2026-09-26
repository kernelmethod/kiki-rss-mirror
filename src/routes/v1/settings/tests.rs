#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
//! HTTP-level tests for the settings routes.

use crate::test::TestBuilder;
use anyhow::Result;
use axum::http::StatusCode;

#[tokio::test]
async fn feed_fetch_settings_roundtrip() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    let client = tc.client()?;

    // Default state, seeded by init.sql.
    let resp = client
        .get("http://localhost/v1/settings/feed-fetch")
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await?;
    assert_eq!(
        body["max_feed_bytes"].as_u64().unwrap(),
        crate::db::settings::DEFAULT_MAX_FEED_BYTES
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
        crate::db::settings::DEFAULT_MAX_FEED_BYTES
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
