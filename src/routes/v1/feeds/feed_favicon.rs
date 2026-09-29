//! `GET /v1/feeds/id/{id}/favicon` — redirect to the cached favicon of the
//! website a feed belongs to.
use crate::routes::v1::assets::asset_url;
use crate::server::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Redirect, Response},
};
use tokio::task;
use tracing::{event, Level};

/// Get a feed's favicon
///
/// Redirect to the cached favicon of the website the feed belongs to, so
/// that clients can use this URL directly as an image source. Favicons are
/// fetched in the background after the feed is refreshed, while the asset
/// cache is enabled.
#[utoipa::path(
    get,
    path = "/v1/feeds/id/{id}/favicon",
    params(("id" = i64, Path, description = "Feed ID")),
    responses(
        (status = 302, description = "Redirect to /v1/assets/{hash}"),
        (status = 404, description = "Feed not found, or it has no cached favicon"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "feeds"
)]
#[axum::debug_handler]
pub async fn feed_favicon(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, Response> {
    let pool = state.conn_pool.clone();
    let hash = task::spawn_blocking(move || -> anyhow::Result<Option<String>> {
        let conn = pool.get()?;
        crate::db::favicons::favicon_hash(&conn, id)
    })
    .await
    .map_err(|e| {
        event!(Level::ERROR, "task error in feed_favicon: {:?}", e);
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
    })?
    .map_err(|e| {
        event!(Level::ERROR, "db error in feed_favicon: {:?}", e);
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
    })?;

    match hash {
        Some(hash) => Ok(Redirect::to(&asset_url(&hash)).into_response()),
        None => Err((StatusCode::NOT_FOUND, "favicon not found").into_response()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::expect_used)]
mod tests {
    use crate::routes::v1::entries::get_entry::GetEntryResponse;
    use crate::routes::v1::entries::ListEntriesResponse;
    use crate::routes::v1::feeds::get_feed::GetFeedDetailResponse;
    use crate::routes::v1::feeds::list_feeds::ListFeedsResponse;
    use crate::test::{TestBuilder, TestConfig};
    use anyhow::Result;
    use axum::http::StatusCode;

    const ICON: &[u8] = b"icon-bytes";

    /// Add two feeds with an entry each, and cache a favicon for the first.
    /// Returns the favicon's hash.
    fn populate(tc: &TestConfig) -> Result<String> {
        let conn = tc.database_conn()?;
        for (feed, site) in [("A", Some("https://a.example/")), ("B", None)] {
            conn.execute(
                "INSERT INTO feeds (title, url, site_url, syndication_format)
                 VALUES (?1, ?2, ?3, 'rss')",
                rusqlite::params![feed, format!("https://{feed}.example/feed"), site],
            )?;
            conn.execute(
                "INSERT INTO entries (feed_id, syndication_format, guid, published_at, title, url)
                 VALUES (?1, 'rss', ?2, 1700000000, ?2, 'https://x/')",
                rusqlite::params![conn.last_insert_rowid(), feed],
            )?;
        }
        let hash = blake3::hash(ICON).to_hex().to_string();
        crate::tasks::assets::write_asset_file(tc.config_dir(), &hash, ICON)?;
        let asset = crate::db::assets::insert_asset(
            &conn,
            &hash,
            "https://a.example/favicon.ico",
            Some("image/x-icon"),
            ICON.len() as i64,
            None,
            None,
        )?;
        crate::db::favicons::record_check(&conn, 1, Some(asset))?;
        crate::db::favicons::record_check(&conn, 2, None)?;
        Ok(hash)
    }

    #[tokio::test]
    async fn favicons_are_served_with_entries_and_feeds() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let hash = populate(&tc)?;
        let client = tc.client()?;
        let expected = Some(format!("/v1/assets/{hash}"));

        let entries: ListEntriesResponse = client
            .get("http://localhost/v1/entries")
            .send()
            .await?
            .json()
            .await?;
        let favicon_of = |feed_id: i64| {
            entries
                .entries
                .iter()
                .find(|e| e.feed_id == Some(feed_id))
                .unwrap()
                .feed_favicon_url
                .clone()
        };
        assert_eq!(favicon_of(1), expected);
        assert_eq!(favicon_of(2), None);

        for (feed_id, want) in [(1, &expected), (2, &None)] {
            let entry: GetEntryResponse = client
                .get(format!("http://localhost/v1/entries/id/{feed_id}"))
                .send()
                .await?
                .json()
                .await?;
            assert_eq!(&entry.feed_favicon_url, want);

            let feed_entries: ListEntriesResponse = client
                .get(format!("http://localhost/v1/feeds/id/{feed_id}/entries"))
                .send()
                .await?
                .json()
                .await?;
            assert_eq!(&feed_entries.entries[0].feed_favicon_url, want);
        }

        let feed: GetFeedDetailResponse = client
            .get("http://localhost/v1/feeds/id/1")
            .send()
            .await?
            .json()
            .await?;
        assert_eq!(feed.feed.favicon_url, expected);
        assert_eq!(feed.feed.site_url.as_deref(), Some("https://a.example/"));

        let feeds: ListFeedsResponse = client
            .get("http://localhost/v1/feeds")
            .send()
            .await?
            .json()
            .await?;
        assert_eq!(feeds.feeds[0].favicon_url, expected);
        assert_eq!(feeds.feeds[1].favicon_url, None);
        assert_eq!(feeds.feeds[1].site_url, None);
        Ok(())
    }

    #[tokio::test]
    async fn favicon_route_redirects_to_the_asset() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let hash = populate(&tc)?;
        let client = tc
            .client_builder()?
            .redirect(reqwest::redirect::Policy::none())
            .build()?;

        let resp = client
            .get("http://localhost/v1/feeds/id/1/favicon")
            .send()
            .await?;
        assert!(resp.status().is_redirection(), "{}", resp.status());
        let location = resp.headers()[reqwest::header::LOCATION].to_str()?;
        assert_eq!(location, format!("/v1/assets/{hash}"));

        let resp = client
            .get(format!("http://localhost{location}"))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()[reqwest::header::CONTENT_TYPE].to_str()?,
            "image/x-icon"
        );
        assert_eq!(&resp.bytes().await?[..], ICON);

        for missing in ["2", "999"] {
            let resp = client
                .get(format!("http://localhost/v1/feeds/id/{missing}/favicon"))
                .send()
                .await?;
            assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        }
        Ok(())
    }
}
