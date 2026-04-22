#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
//! HTTP-level tests for the asset routes.

use crate::test::TestBuilder;
use anyhow::Result;
use axum::http::StatusCode;

/// Populate a single asset row with on-disk bytes so the route handlers
/// have something to serve. Returns the blake3 hex hash.
fn seed_asset(tc: &crate::test::TestConfig, bytes: &[u8], original_url: &str) -> Result<String> {
    let conn = tc.database_conn()?;
    let hash = blake3::hash(bytes).to_hex().to_string();
    let data_dir = tc.config_dir().to_path_buf();

    // Write the file where the route handler expects it.
    let path = crate::tasks::assets::asset_path(&data_dir, &hash);
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(&path, bytes)?;

    crate::db::assets::insert_asset(
        &conn,
        &hash,
        original_url,
        Some("image/png"),
        bytes.len() as i64,
        None,
        None,
    )?;

    Ok(hash)
}

#[tokio::test]
async fn get_asset_returns_bytes_and_etag() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    let client = tc.client()?;
    let payload = b"hello world".to_vec();
    let hash = seed_asset(&tc, &payload, "http://src.example/a.png")?;

    let resp = client
        .get(format!("http://localhost/v1/assets/{}", hash))
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap(),
        "image/png"
    );
    assert_eq!(
        resp.headers()
            .get("x-content-type-options")
            .unwrap()
            .to_str()
            .unwrap(),
        "nosniff"
    );
    assert_eq!(
        resp.headers()
            .get("content-disposition")
            .unwrap()
            .to_str()
            .unwrap(),
        "inline"
    );
    let etag = resp
        .headers()
        .get("etag")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(etag, format!("\"{}\"", hash));
    let body = resp.bytes().await?.to_vec();
    assert_eq!(body, payload);

    // If-None-Match returns 304.
    let resp = client
        .get(format!("http://localhost/v1/assets/{}", hash))
        .header("if-none-match", format!("\"{}\"", hash))
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);

    Ok(())
}

#[tokio::test]
async fn get_asset_invalid_hash_is_400() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    let client = tc.client()?;

    let resp = client.get("http://localhost/v1/assets/abc").send().await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    Ok(())
}

#[tokio::test]
async fn get_asset_unknown_hash_is_404() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    let client = tc.client()?;

    // 64 hex chars but not present in the database.
    let missing = "0".repeat(64);
    let resp = client
        .get(format!("http://localhost/v1/assets/{}", missing))
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    Ok(())
}

#[tokio::test]
async fn get_asset_by_url_redirects_when_cached() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    // Wait for the unix socket before building the client, mirroring
    // `TestConfig::client()` but with redirects disabled so the 302 is
    // observable.
    let socket_path = tc.socket_path();
    let start = std::time::Instant::now();
    while !socket_path.exists() && start.elapsed() < std::time::Duration::from_secs(5) {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let raw_client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .unix_socket(socket_path)
        .build()?;

    let hash = seed_asset(&tc, b"xyz", "http://src.example/q.png")?;

    let resp = raw_client
        .get("http://localhost/v1/assets/by-url?url=http%3A%2F%2Fsrc.example%2Fq.png")
        .send()
        .await?;
    // Axum `Redirect::to` returns 303 See Other; allow any 3xx with the
    // correct Location header rather than hardcoding a status code.
    assert!(
        resp.status().is_redirection(),
        "expected redirect, got {}",
        resp.status()
    );
    let loc = resp
        .headers()
        .get("location")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(loc, format!("/v1/assets/{}", hash));

    // Miss -> 404.
    let resp = raw_client
        .get("http://localhost/v1/assets/by-url?url=http%3A%2F%2Fsrc.example%2Fmissing.png")
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    Ok(())
}

#[tokio::test]
async fn delete_asset_removes_row_and_file() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    let client = tc.client()?;
    let hash = seed_asset(&tc, b"bye", "http://src.example/z.png")?;

    let resp = client
        .delete(format!("http://localhost/v1/assets/{}", hash))
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // Second delete yields 404.
    let resp = client
        .delete(format!("http://localhost/v1/assets/{}", hash))
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // File is gone.
    let path = crate::tasks::assets::asset_path(tc.config_dir(), &hash);
    assert!(!path.exists(), "asset file should be unlinked on delete");

    Ok(())
}

#[tokio::test]
async fn list_entry_assets_returns_mapping() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    let client = tc.client()?;

    // Seed a feed + entry + asset + link.
    let conn = tc.database_conn()?;
    conn.execute(
        "INSERT INTO feeds (title, url, syndication_format) VALUES (?, ?, ?)",
        ["F", "http://example.com", "rss"],
    )?;
    conn.execute(
        "INSERT INTO entries (feed_id, syndication_format, guid, published_at, title, url)
         VALUES (?, ?, ?, ?, ?, ?)",
        rusqlite::params![1i64, "rss", "g", 1i64, "t", "http://example.com/e"],
    )?;
    drop(conn);

    let hash = seed_asset(&tc, b"pixels", "http://src.example/l.png")?;
    let conn = tc.database_conn()?;
    let asset_id: i64 = conn.query_row(
        "SELECT id FROM feed_assets WHERE blake3 = ?1",
        [&hash],
        |r| r.get(0),
    )?;
    crate::db::assets::link_entry_asset(&conn, 1, asset_id, "inline_img")?;
    drop(conn);

    let resp = client
        .get("http://localhost/v1/entries/id/1/assets")
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await?;
    let arr = body["assets"].as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["original_url"], "http://src.example/l.png");
    assert_eq!(arr[0]["blake3"], hash);
    assert_eq!(arr[0]["url"], format!("/v1/assets/{}", hash));
    assert_eq!(arr[0]["kind"], "inline_img");

    // 404 for unknown entry.
    let resp = client
        .get("http://localhost/v1/entries/id/9999/assets")
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    Ok(())
}

#[tokio::test]
async fn asset_cache_settings_roundtrip() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    let client = tc.client()?;

    // Default state.
    let resp = client
        .get("http://localhost/v1/settings/asset-cache")
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await?;
    assert_eq!(body["enabled"], true);
    assert!(body["max_bytes"].as_i64().unwrap() > 0);
    assert_eq!(body["current_bytes"], 0);

    // Disable and shrink.
    let resp = client
        .put("http://localhost/v1/settings/asset-cache")
        .json(&serde_json::json!({"enabled": false, "max_bytes": 123}))
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await?;
    assert_eq!(body["enabled"], false);
    assert_eq!(body["max_bytes"], 123);

    // Negative max_bytes is rejected.
    let resp = client
        .put("http://localhost/v1/settings/asset-cache")
        .json(&serde_json::json!({"max_bytes": -1}))
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    Ok(())
}
