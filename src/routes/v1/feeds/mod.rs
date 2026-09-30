pub mod add_feed;
pub mod delete_feed;
pub mod export_opml;
pub mod feed_entries;
pub mod feed_favicon;
pub mod feed_tags;
pub mod fetch_all_feeds;
pub mod fetch_feed;
pub mod format_data;
pub mod get_feed;
pub mod import_opml;
pub mod list_feeds;
pub mod update_feed;

use add_feed::add_feed;
use delete_feed::delete_feed;
use export_opml::export_opml;
use feed_entries::feed_entries;
use feed_favicon::feed_favicon;
use feed_tags::{get_feed_tags, set_feed_tags};
use fetch_all_feeds::fetch_all_feeds;
use fetch_feed::fetch_feed;
use get_feed::get_feed;
use import_opml::import_opml;
use list_feeds::list_feeds;
use update_feed::update_feed;

use crate::server::AppState;
use axum::{
    routing::{get, post},
    Router,
};

pub fn create_router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_feeds))
        .route("/create", post(add_feed))
        .route(
            "/id/{id}",
            get(get_feed).delete(delete_feed).put(update_feed),
        )
        .route("/id/{id}/tags", get(get_feed_tags).put(set_feed_tags))
        .route("/id/{id}/entries", get(feed_entries))
        .route("/id/{id}/favicon", get(feed_favicon))
        .route("/refresh", post(fetch_all_feeds))
        .route("/refresh/{id}", post(fetch_feed))
        .route("/import", post(import_opml))
        .route("/export", get(export_opml))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::expect_used)]
mod test {
    use super::*;
    use crate::routes::v1::entries::ListEntriesResponse;
    use crate::routes::v1::feeds::feed_tags;
    use crate::test::{TestBuilder, TestConfig};
    use anyhow::Result;
    use axum::http::StatusCode;

    fn populate_tags(tc: &TestConfig) -> Result<()> {
        let conn = tc.database_conn()?;
        conn.execute("INSERT INTO tags (name) VALUES (?)", ["news"])?;
        conn.execute("INSERT INTO tags (name) VALUES (?)", ["tech"])?;
        conn.execute("INSERT INTO tags (name) VALUES (?)", ["science"])?;
        Ok(())
    }

    fn populate_feeds_and_entries(tc: &TestConfig) -> Result<()> {
        let conn = tc.database_conn()?;

        conn.execute(
            "INSERT INTO feeds (title, url, syndication_format) VALUES (?, ?, ?)",
            ["Feed A", "http://example.com/a", "rss"],
        )?;
        conn.execute(
            "INSERT INTO feeds (title, url, syndication_format) VALUES (?, ?, ?)",
            ["Feed B", "http://example.com/b", "atom"],
        )?;

        conn.execute(
            "INSERT INTO entries (feed_id, syndication_format, guid, published_at, title, url)
             VALUES (?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                1,
                "rss",
                "guid-1",
                1700000000i64,
                "Entry 1",
                "http://example.com/1"
            ],
        )?;
        conn.execute(
            "INSERT INTO entries (feed_id, syndication_format, guid, published_at, title, url)
             VALUES (?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                2,
                "atom",
                "guid-2",
                1700000001i64,
                "Entry 2",
                "http://example.com/2"
            ],
        )?;

        Ok(())
    }

    async fn add_example_feed(tc: &TestConfig) -> Result<i64> {
        tc.add_feed_from_url("my feed", tc.example_feed_url()).await
    }

    #[tokio::test]
    async fn test_add_feed() -> Result<()> {
        let tc = TestBuilder::all().init_server().build()?;
        let client = tc.client()?;

        // We should start off with zero feeds and zero entries
        let resp = client.get("http://localhost/v1/feeds").send().await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = resp.json::<list_feeds::ListFeedsResponse>().await?;
        assert_eq!(json.feeds.len(), 0);
        assert_eq!(json.count, 0);
        assert_eq!(json.offset, 0);
        let resp = client.get("http://localhost/v1/feeds/id/1").send().await?;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_eq!(&resp.text().await?, "Feed not found");

        let resp = client.get("http://localhost/v1/entries").send().await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = resp.json::<ListEntriesResponse>().await?;
        assert_eq!(json.count, 0);

        let feed_id = add_example_feed(&tc).await?;
        let resp = client
            .get(format!("http://localhost/v1/feeds/id/{:?}", feed_id))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = resp.json::<get_feed::GetFeedResponse>().await?;
        assert_eq!(json.id, feed_id);
        assert_eq!(json.title, "my feed");
        assert_eq!(json.url, tc.example_feed_url());
        assert_eq!(json.description, None);
        assert!(json.last_checked.is_some());

        // We should also see the feed in the list of feeds
        let resp = client.get("http://localhost/v1/feeds").send().await?;
        assert_eq!(resp.status(), StatusCode::OK);

        // Check that the entries from the feed were retrieved
        let resp = client.get("http://localhost/v1/entries").send().await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = resp.json::<ListEntriesResponse>().await?;
        assert_eq!(json.count, 6);

        Ok(())
    }

    #[tokio::test]
    async fn test_list_feeds() -> Result<()> {
        let tc = TestBuilder::all().init_server().build()?;
        let client = tc.client()?;

        // Test empty feeds list
        let resp = client.get("http://localhost/v1/feeds").send().await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = resp.json::<list_feeds::ListFeedsResponse>().await?;
        assert_eq!(json.feeds.len(), 0);
        assert_eq!(json.count, 0);

        // Add a feed and test that it appears in the list
        let resp = client
            .post("http://localhost/v1/feeds/create")
            .json(&add_feed::AddFeedRequest {
                title: "test feed".to_string(),
                url: "https://example.com/feed.xml".to_string(),
                ..Default::default()
            })
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let feed_id = resp.json::<add_feed::AddFeedResponse>().await?.id;

        // Check that the feed appears in the list
        let resp = client.get("http://localhost/v1/feeds").send().await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = resp.json::<list_feeds::ListFeedsResponse>().await?;
        assert_eq!(json.feeds.len(), 1);
        assert_eq!(json.count, 1);
        assert_eq!(json.feeds[0].id, feed_id);
        assert_eq!(json.feeds[0].title, "test feed");
        assert_eq!(json.feeds[0].url, "https://example.com/feed.xml");

        Ok(())
    }

    #[tokio::test]
    async fn test_delete_feed() -> Result<()> {
        let tc = TestBuilder::all().init_server().build()?;
        let client = tc.client()?;

        // Add a feed
        let resp = client
            .post("http://localhost/v1/feeds/create")
            .json(&add_feed::AddFeedRequest {
                title: "test feed".to_string(),
                url: "https://example.com/feed.xml".to_string(),
                ..Default::default()
            })
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let feed_id = resp.json::<add_feed::AddFeedResponse>().await?.id;

        // Verify the feed exists
        let resp = client.get("http://localhost/v1/feeds").send().await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = resp.json::<list_feeds::ListFeedsResponse>().await?;
        assert_eq!(json.feeds.len(), 1);
        assert_eq!(json.count, 1);
        assert_eq!(json.feeds[0].id, feed_id);
        assert_eq!(json.feeds[0].title, "test feed");
        assert_eq!(json.feeds[0].url, "https://example.com/feed.xml");

        // Delete the feed
        let resp = client
            .delete(format!("http://localhost/v1/feeds/id/{:?}", feed_id))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        // Verify the feed is gone
        let resp = client.get("http://localhost/v1/feeds").send().await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = resp.json::<list_feeds::ListFeedsResponse>().await?;
        assert_eq!(json.feeds.len(), 0);
        assert_eq!(json.count, 0);

        // Try to delete a non-existent feed
        let resp = client
            .delete("http://localhost/v1/feeds/id/999")
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let content = resp.text().await?;
        assert_eq!(&content, "Feed not found");

        Ok(())
    }

    #[tokio::test]
    async fn test_delete_feed_keep_entries() -> Result<()> {
        let tc = TestBuilder::all().init_server().build()?;
        let client = tc.client()?;

        // Add a feed (this fetches entries automatically)
        let feed_id = add_example_feed(&tc).await?;

        // Verify entries exist
        let resp = client.get("http://localhost/v1/entries").send().await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = resp.json::<ListEntriesResponse>().await?;
        let entry_count = json.count;
        assert!(entry_count > 0);

        // Delete the feed with delete_entries=false
        let resp = client
            .delete(format!(
                "http://localhost/v1/feeds/id/{:?}?delete_entries=false",
                feed_id
            ))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        // Verify the feed is gone
        let resp = client
            .get(format!("http://localhost/v1/feeds/id/{:?}", feed_id))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        // Verify entries are still present but no longer associated with a feed
        let resp = client.get("http://localhost/v1/entries").send().await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = resp.json::<ListEntriesResponse>().await?;
        assert_eq!(json.count, entry_count);
        for entry in &json.entries {
            assert_eq!(entry.feed_id, None);
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_delete_feed_with_entries() -> Result<()> {
        let tc = TestBuilder::all().init_server().build()?;
        let client = tc.client()?;

        // Add a feed (this fetches entries automatically)
        let feed_id = add_example_feed(&tc).await?;

        // Verify entries exist
        let resp = client.get("http://localhost/v1/entries").send().await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = resp.json::<ListEntriesResponse>().await?;
        assert!(json.count > 0);

        // Delete the feed with delete_entries=true (explicit default)
        let resp = client
            .delete(format!(
                "http://localhost/v1/feeds/id/{:?}?delete_entries=true",
                feed_id
            ))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        // Verify the feed is gone
        let resp = client
            .get(format!("http://localhost/v1/feeds/id/{:?}", feed_id))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        // Verify entries are also deleted
        let resp = client.get("http://localhost/v1/entries").send().await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = resp.json::<ListEntriesResponse>().await?;
        assert_eq!(json.count, 0);

        Ok(())
    }

    #[tokio::test]
    async fn test_update_feed() -> Result<()> {
        let tc = TestBuilder::all().init_server().build()?;
        let client = tc.client()?;

        // Add a feed
        let resp = client
            .post("http://localhost/v1/feeds/create")
            .json(&add_feed::AddFeedRequest {
                title: "original title".to_string(),
                url: "https://example.com/feed.xml".to_string(),
                ..Default::default()
            })
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let feed_id = resp.json::<add_feed::AddFeedResponse>().await?.id;

        // Verify the feed exists with original values
        let resp = client.get("http://localhost/v1/feeds").send().await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = resp.json::<list_feeds::ListFeedsResponse>().await?;
        assert_eq!(json.feeds.len(), 1);
        assert_eq!(json.feeds[0].id, feed_id);
        assert_eq!(json.feeds[0].title, "original title");
        assert_eq!(json.feeds[0].url, "https://example.com/feed.xml");

        // Update the feed
        let resp = client
            .put(format!("http://localhost/v1/feeds/id/{:?}", feed_id))
            .json(&update_feed::UpdateFeedRequest {
                title: Some("updated title".to_string()),
                url: None,
                description: Some("updated description".to_string()),
                min_fetch_interval_seconds: Some(7200),
                ..Default::default()
            })
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = resp.json::<update_feed::UpdateFeedResponse>().await?;
        assert_eq!(json.id, feed_id);
        assert_eq!(json.title, "updated title");
        assert_eq!(json.url, "https://example.com/feed.xml");
        assert_eq!(json.description, Some("updated description".to_string()));
        assert_eq!(json.min_fetch_interval_seconds, 7200);

        // Verify the updated min_fetch_interval_seconds is reflected in get_feed
        let resp = client
            .get(format!("http://localhost/v1/feeds/id/{:?}", feed_id))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = resp.json::<get_feed::GetFeedResponse>().await?;
        assert_eq!(json.min_fetch_interval_seconds, 7200);

        // Verify the feed was updated
        let resp = client.get("http://localhost/v1/feeds").send().await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = resp.json::<list_feeds::ListFeedsResponse>().await?;
        assert_eq!(json.feeds.len(), 1);
        assert_eq!(json.feeds[0].id, feed_id);
        assert_eq!(json.feeds[0].title, "updated title");
        assert_eq!(json.feeds[0].url, "https://example.com/feed.xml");
        assert_eq!(
            json.feeds[0].description,
            Some("updated description".to_string())
        );

        // Try to update a non-existent feed
        let resp = client
            .put("http://localhost/v1/feeds/id/999")
            .json(&update_feed::UpdateFeedRequest {
                title: Some("non-existent title".to_string()),
                url: None,
                description: None,
                min_fetch_interval_seconds: None,
                ..Default::default()
            })
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let content = resp.text().await?;
        assert_eq!(&content, "Feed not found");

        // Try to update with no fields provided
        let resp = client
            .put(format!("http://localhost/v1/feeds/id/{:?}", feed_id))
            .json(&update_feed::UpdateFeedRequest {
                title: None,
                url: None,
                description: None,
                min_fetch_interval_seconds: None,
                ..Default::default()
            })
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // Try to update with an invalid min_fetch_interval_seconds
        let resp = client
            .put(format!("http://localhost/v1/feeds/id/{:?}", feed_id))
            .json(&update_feed::UpdateFeedRequest {
                title: None,
                url: None,
                description: None,
                min_fetch_interval_seconds: Some(0),
                ..Default::default()
            })
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        Ok(())
    }

    #[tokio::test]
    async fn test_add_feed_with_basic_auth() -> Result<()> {
        use crate::http::FeedAuthType;

        let tc = TestBuilder::all().init_server().build()?;
        let client = tc.client()?;

        let resp = client
            .post("http://localhost/v1/feeds/create")
            .json(&add_feed::AddFeedRequest {
                title: "private".into(),
                url: "https://example.com/private.xml".into(),
                auth_type: Some(FeedAuthType::Basic),
                auth_username: Some("alice".into()),
                auth_password: Some("hunter2".into()),
                auth_bearer_token: None,
            })
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let feed_id = resp.json::<add_feed::AddFeedResponse>().await?.id;

        // Credentials should be stored exactly as provided.
        let conn = tc.database_conn()?;
        let (auth_type, username, password, bearer): (
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = conn.query_row(
            "SELECT auth_type, auth_username, auth_password, auth_bearer_token
             FROM feeds WHERE id = ?1",
            [feed_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        assert_eq!(auth_type.as_deref(), Some("basic"));
        assert_eq!(username.as_deref(), Some("alice"));
        assert_eq!(password.as_deref(), Some("hunter2"));
        assert_eq!(bearer, None);

        // GET should surface auth_type but not credentials.
        let resp = client
            .get(format!("http://localhost/v1/feeds/id/{}", feed_id))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body: serde_json::Value = resp.json().await?;
        assert_eq!(body["auth_type"], "basic");
        assert!(body.get("auth_username").is_none());
        assert!(body.get("auth_password").is_none());
        assert!(body.get("auth_bearer_token").is_none());

        Ok(())
    }

    #[tokio::test]
    async fn test_add_feed_rejects_invalid_auth() -> Result<()> {
        use crate::http::FeedAuthType;

        let tc = TestBuilder::all().init_server().build()?;
        let client = tc.client()?;

        // Basic auth without a username should 400.
        let resp = client
            .post("http://localhost/v1/feeds/create")
            .json(&add_feed::AddFeedRequest {
                title: "bad".into(),
                url: "https://example.com/x.xml".into(),
                auth_type: Some(FeedAuthType::Basic),
                auth_username: None,
                auth_password: Some("p".into()),
                auth_bearer_token: None,
            })
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // Bearer auth without a token should 400.
        let resp = client
            .post("http://localhost/v1/feeds/create")
            .json(&add_feed::AddFeedRequest {
                title: "bad".into(),
                url: "https://example.com/x.xml".into(),
                auth_type: Some(FeedAuthType::Bearer),
                auth_username: None,
                auth_password: None,
                auth_bearer_token: None,
            })
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        Ok(())
    }

    #[tokio::test]
    async fn test_update_feed_switches_auth_scheme() -> Result<()> {
        use crate::http::FeedAuthType;

        let tc = TestBuilder::all().init_server().build()?;
        let client = tc.client()?;

        // Create a feed using Basic auth.
        let resp = client
            .post("http://localhost/v1/feeds/create")
            .json(&add_feed::AddFeedRequest {
                title: "switch".into(),
                url: "https://example.com/s.xml".into(),
                auth_type: Some(FeedAuthType::Basic),
                auth_username: Some("u".into()),
                auth_password: Some("p".into()),
                auth_bearer_token: None,
            })
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let feed_id = resp.json::<add_feed::AddFeedResponse>().await?.id;

        // Switch to Bearer; basic credentials should be cleared.
        let resp = client
            .put(format!("http://localhost/v1/feeds/id/{}", feed_id))
            .json(&update_feed::UpdateFeedRequest {
                auth_type: Some(FeedAuthType::Bearer),
                auth_bearer_token: Some("tok".into()),
                ..Default::default()
            })
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<update_feed::UpdateFeedResponse>().await?;
        assert_eq!(body.auth_type, FeedAuthType::Bearer);

        let conn = tc.database_conn()?;
        let (auth_type, username, password, bearer): (
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = conn.query_row(
            "SELECT auth_type, auth_username, auth_password, auth_bearer_token
             FROM feeds WHERE id = ?1",
            [feed_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        assert_eq!(auth_type.as_deref(), Some("bearer"));
        assert_eq!(username, None, "previous basic username should be cleared");
        assert_eq!(password, None, "previous basic password should be cleared");
        assert_eq!(bearer.as_deref(), Some("tok"));

        // Clear auth entirely.
        let resp = client
            .put(format!("http://localhost/v1/feeds/id/{}", feed_id))
            .json(&update_feed::UpdateFeedRequest {
                auth_type: Some(FeedAuthType::None),
                ..Default::default()
            })
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let (auth_type, username, password, bearer): (
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = conn.query_row(
            "SELECT auth_type, auth_username, auth_password, auth_bearer_token
             FROM feeds WHERE id = ?1",
            [feed_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        assert_eq!(auth_type, None);
        assert_eq!(username, None);
        assert_eq!(password, None);
        assert_eq!(bearer, None);

        Ok(())
    }

    #[tokio::test]
    async fn test_fetch_feed() -> Result<()> {
        let tc = TestBuilder::all().init_server().build()?;
        let client = tc.client()?;

        // Add a feed
        let resp = client
            .post("http://localhost/v1/feeds/create")
            .json(&add_feed::AddFeedRequest {
                title: "test feed".to_string(),
                url: "https://example.com/feed.xml".to_string(),
                ..Default::default()
            })
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let feed_id = resp.json::<add_feed::AddFeedResponse>().await?.id;

        // Test the fetch endpoint - should return 202 Accepted
        let resp = client
            .post(format!("http://localhost/v1/feeds/refresh/{:?}", feed_id))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::ACCEPTED);

        Ok(())
    }

    #[tokio::test]
    async fn test_export_opml_empty() -> Result<()> {
        let tc = TestBuilder::all().init_server().build()?;
        let client = tc.client()?;

        let resp = client
            .get("http://localhost/v1/feeds/export")
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);

        let xml = resp.text().await?;
        assert!(xml.contains("<opml"));
        assert!(xml.contains(r#"version="2.0""#));
        assert!(xml.contains("<body"));
        // No feed outlines in an empty database
        assert!(!xml.contains("xmlUrl"));

        Ok(())
    }

    #[tokio::test]
    async fn test_export_opml_untagged_feeds() -> Result<()> {
        let tc = TestBuilder::all().init_server().build()?;
        let client = tc.client()?;

        // Add two untagged feeds directly to the database
        {
            let conn = tc.database_conn()?;
            conn.execute(
                "INSERT INTO feeds (title, url) VALUES (?, ?)",
                ["Alpha Feed", "https://example.com/alpha.xml"],
            )?;
            conn.execute(
                "INSERT INTO feeds (title, url) VALUES (?, ?)",
                ["Beta Feed", "https://example.com/beta.xml"],
            )?;
        }

        let resp = client
            .get("http://localhost/v1/feeds/export")
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);

        let xml = resp.text().await?;
        assert!(xml.contains("Alpha Feed"));
        assert!(xml.contains("https://example.com/alpha.xml"));
        assert!(xml.contains("Beta Feed"));
        assert!(xml.contains("https://example.com/beta.xml"));

        Ok(())
    }

    #[tokio::test]
    async fn test_export_opml_tagged_feeds() -> Result<()> {
        let tc = TestBuilder::all().init_server().build()?;
        let client = tc.client()?;

        // Set up a tagged feed and an untagged feed directly in the database
        {
            let conn = tc.database_conn()?;
            conn.execute(
                "INSERT INTO feeds (title, url) VALUES (?, ?)",
                ["Tech News", "https://example.com/tech.xml"],
            )?;
            conn.execute(
                "INSERT INTO feeds (title, url) VALUES (?, ?)",
                ["Untagged Feed", "https://example.com/untagged.xml"],
            )?;
            conn.execute("INSERT INTO tags (name) VALUES (?)", ["technology"])?;
            conn.execute(
                "INSERT INTO feed_tags (feed_id, tag_id)
                 SELECT 1, id FROM tags WHERE name = 'technology'",
                [],
            )?;
        }

        let resp = client
            .get("http://localhost/v1/feeds/export")
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);

        let xml = resp.text().await?;
        // Folder element for the tag
        assert!(xml.contains("technology"));
        // Tagged feed inside the folder
        assert!(xml.contains("Tech News"));
        assert!(xml.contains("https://example.com/tech.xml"));
        // Untagged feed at the top level
        assert!(xml.contains("Untagged Feed"));
        assert!(xml.contains("https://example.com/untagged.xml"));

        Ok(())
    }

    #[tokio::test]
    async fn test_import_opml_basic() -> Result<()> {
        let tc = TestBuilder::all().init_server().build()?;
        let client = tc.client()?;

        let opml = r#"<?xml version="1.0" encoding="UTF-8"?>
<opml version="2.0">
  <head><title>Test Feeds</title></head>
  <body>
    <outline type="rss" text="Feed One" xmlUrl="https://example.com/one.xml"/>
    <outline type="rss" text="Feed Two" xmlUrl="https://example.com/two.xml"/>
  </body>
</opml>"#;

        let resp = client
            .post("http://localhost/v1/feeds/import")
            .body(opml)
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let json = resp.json::<import_opml::ImportOpmlResponse>().await?;
        assert_eq!(json.imported, 2);

        // Verify feeds were created
        let resp = client.get("http://localhost/v1/feeds").send().await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let list = resp.json::<list_feeds::ListFeedsResponse>().await?;
        assert_eq!(list.count, 2);

        let titles: Vec<&str> = list.feeds.iter().map(|f| f.title.as_str()).collect();
        assert!(titles.contains(&"Feed One"));
        assert!(titles.contains(&"Feed Two"));

        Ok(())
    }

    #[tokio::test]
    async fn test_import_opml_with_folders() -> Result<()> {
        let tc = TestBuilder::all().init_server().build()?;
        let client = tc.client()?;

        let opml = r#"<?xml version="1.0" encoding="UTF-8"?>
<opml version="2.0">
  <head><title>Test Feeds</title></head>
  <body>
    <outline text="tech">
      <outline type="rss" text="Tech Blog" xmlUrl="https://example.com/tech.xml"/>
    </outline>
    <outline type="rss" text="Untagged Feed" xmlUrl="https://example.com/untagged.xml"/>
  </body>
</opml>"#;

        let resp = client
            .post("http://localhost/v1/feeds/import")
            .body(opml)
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let json = resp.json::<import_opml::ImportOpmlResponse>().await?;
        assert_eq!(json.imported, 2);

        // Verify both feeds were created
        let resp = client.get("http://localhost/v1/feeds").send().await?;
        let list = resp.json::<list_feeds::ListFeedsResponse>().await?;
        assert_eq!(list.count, 2);

        // The feed inside the folder should have the folder name as a tag
        let tech_feed = list.feeds.iter().find(|f| f.title == "Tech Blog").unwrap();
        let resp = client
            .get(format!(
                "http://localhost/v1/feeds/id/{}/tags",
                tech_feed.id
            ))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let tags_resp = resp.json::<feed_tags::GetFeedTagsResponse>().await?;
        assert_eq!(tags_resp.tags.len(), 1);
        assert_eq!(tags_resp.tags[0].name, "tech");

        // The top-level feed should have no tags
        let untagged = list
            .feeds
            .iter()
            .find(|f| f.title == "Untagged Feed")
            .unwrap();
        let resp = client
            .get(format!("http://localhost/v1/feeds/id/{}/tags", untagged.id))
            .send()
            .await?;
        let tags_resp = resp.json::<feed_tags::GetFeedTagsResponse>().await?;
        assert_eq!(tags_resp.tags.len(), 0);

        Ok(())
    }

    #[tokio::test]
    async fn test_import_opml_empty_body() -> Result<()> {
        let tc = TestBuilder::all().init_server().build()?;
        let client = tc.client()?;

        let opml = r#"<?xml version="1.0" encoding="UTF-8"?>
<opml version="2.0">
  <head><title>Empty</title></head>
  <body></body>
</opml>"#;

        let resp = client
            .post("http://localhost/v1/feeds/import")
            .body(opml)
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = resp.json::<import_opml::ImportOpmlResponse>().await?;
        assert_eq!(json.imported, 0);

        Ok(())
    }

    #[tokio::test]
    async fn test_import_opml_invalid_xml() -> Result<()> {
        let tc = TestBuilder::all().init_server().build()?;
        let client = tc.client()?;

        let resp = client
            .post("http://localhost/v1/feeds/import")
            .body("not xml at all <<<")
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        Ok(())
    }

    #[tokio::test]
    async fn test_export_import_roundtrip() -> Result<()> {
        let tc = TestBuilder::all().init_server().build()?;
        let client = tc.client()?;

        // Import an OPML file with feeds and folder-based tags
        let opml = r#"<?xml version="1.0" encoding="UTF-8"?>
<opml version="2.0">
  <head><title>My Feeds</title></head>
  <body>
    <outline text="news">
      <outline type="rss" text="News Site" xmlUrl="https://example.com/news.xml"/>
    </outline>
    <outline type="rss" text="Personal Blog" xmlUrl="https://example.com/blog.xml"/>
  </body>
</opml>"#;

        let resp = client
            .post("http://localhost/v1/feeds/import")
            .body(opml)
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let json = resp.json::<import_opml::ImportOpmlResponse>().await?;
        assert_eq!(json.imported, 2);

        // Export and verify the OPML contains the imported feeds and tags
        let resp = client
            .get("http://localhost/v1/feeds/export")
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let exported_xml = resp.text().await?;

        assert!(exported_xml.contains("News Site"));
        assert!(exported_xml.contains("https://example.com/news.xml"));
        assert!(exported_xml.contains("Personal Blog"));
        assert!(exported_xml.contains("https://example.com/blog.xml"));
        // The "news" folder should appear in the export
        assert!(exported_xml.contains("news"));

        Ok(())
    }

    #[tokio::test]
    async fn test_get_feed_includes_atom_data() -> Result<()> {
        let tc = TestBuilder::all().init_server().build()?;
        let client = tc.client()?;
        let feed_id = tc
            .add_feed_from_url("rich atom", tc.rich_atom_feed_url())
            .await?;

        let resp = client
            .get(format!("http://localhost/v1/feeds/id/{}", feed_id))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body: serde_json::Value = resp.json().await?;

        assert!(body.get("rss").is_some(), "rss key should be present");
        assert!(body["rss"].is_null(), "rss should be null for an atom feed");

        let atom = body["atom"].as_object().expect("atom object present");
        assert_eq!(atom["atom_language_tag"], "en-US");
        assert_eq!(atom["rights"], "(c) 2026 Example Corp");
        assert_eq!(atom["logo"], "http://example.com/logo.png");
        assert_eq!(atom["icon"], "http://example.com/icon.png");

        let gen_ = atom["generator"].as_object().expect("generator present");
        assert_eq!(gen_["value"], "Example Generator");
        assert_eq!(gen_["uri"], "https://example.com/gen");
        assert_eq!(gen_["version"], "1.2");

        assert_eq!(atom["authors"].as_array().unwrap().len(), 2);
        assert_eq!(atom["contributors"].as_array().unwrap().len(), 2);
        let cats = atom["categories"].as_array().unwrap();
        assert_eq!(cats.len(), 2);
        assert_eq!(cats[0]["term"], "t1");
        assert_eq!(cats[0]["label"], "Label One");

        Ok(())
    }

    #[tokio::test]
    async fn test_get_feed_rss_data_present_for_rss_feed() -> Result<()> {
        let tc = TestBuilder::all().init_server().build()?;
        let client = tc.client()?;
        let feed_id = tc
            .add_feed_from_url("rich rss", tc.rich_rss_feed_url())
            .await?;

        let resp = client
            .get(format!("http://localhost/v1/feeds/id/{}", feed_id))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body: serde_json::Value = resp.json().await?;

        assert!(
            body["rss"].is_object(),
            "rss sub-object should be present for an RSS feed, got: {:?}",
            body["rss"]
        );
        assert!(
            body["atom"].is_null(),
            "atom sub-object should be null for an RSS feed, got: {:?}",
            body["atom"]
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_list_feeds_does_not_include_format_data() -> Result<()> {
        let tc = TestBuilder::all().init_server().build()?;
        tc.add_feed_from_url("rich atom", tc.rich_atom_feed_url())
            .await?;
        let client = tc.client()?;

        let resp = client.get("http://localhost/v1/feeds").send().await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body: serde_json::Value = resp.json().await?;

        let feeds = body["feeds"].as_array().expect("feeds array");
        assert_eq!(feeds.len(), 1);
        assert!(
            feeds[0].get("rss").is_none(),
            "list endpoint leaked rss sub-object: {:?}",
            feeds[0]
        );
        assert!(
            feeds[0].get("atom").is_none(),
            "list endpoint leaked atom sub-object: {:?}",
            feeds[0]
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_get_feed_exposes_last_fetch_error() -> Result<()> {
        let tc = TestBuilder::all().init_server().build()?;
        let client = tc.client()?;

        // Insert a feed with a stored fetch error directly, simulating a
        // previously-failed refresh attempt.
        let feed_id = {
            let conn = tc.database_conn()?;
            conn.execute(
                "INSERT INTO feeds (title, url, last_fetch_error, last_fetch_error_at)
                 VALUES (?, ?, ?, ?)",
                rusqlite::params![
                    "broken feed",
                    "https://example.com/broken.xml",
                    r#"{"type":"http_status","url":"https://example.com/broken.xml","status":503}"#,
                    1700000000i64,
                ],
            )?;
            conn.last_insert_rowid()
        };

        let resp = client
            .get(format!("http://localhost/v1/feeds/id/{}", feed_id))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body: serde_json::Value = resp.json().await?;

        let err = body
            .get("last_fetch_error")
            .expect("last_fetch_error should be present");
        assert_eq!(err["type"], "http_status");
        assert_eq!(err["url"], "https://example.com/broken.xml");
        assert_eq!(err["status"], 503);
        assert!(
            body.get("last_fetch_error_at")
                .and_then(|v| v.as_str())
                .is_some(),
            "last_fetch_error_at should be a string: {:?}",
            body.get("last_fetch_error_at")
        );

        // A successful refresh clears the error, and the single-feed
        // response should then omit both error fields.
        {
            let conn = tc.database_conn()?;
            conn.execute(
                "UPDATE feeds SET last_fetch_error = NULL, last_fetch_error_at = NULL WHERE id = ?1",
                [feed_id],
            )?;
        }

        let resp = client
            .get(format!("http://localhost/v1/feeds/id/{}", feed_id))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body: serde_json::Value = resp.json().await?;
        assert!(
            body.get("last_fetch_error").is_none(),
            "last_fetch_error should be omitted when null: {:?}",
            body.get("last_fetch_error")
        );
        assert!(
            body.get("last_fetch_error_at").is_none(),
            "last_fetch_error_at should be omitted when null: {:?}",
            body.get("last_fetch_error_at")
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_list_feeds_does_not_expose_last_fetch_error() -> Result<()> {
        let tc = TestBuilder::all().init_server().build()?;
        let client = tc.client()?;

        // Insert a feed with a stored fetch error.
        {
            let conn = tc.database_conn()?;
            conn.execute(
                "INSERT INTO feeds (title, url, last_fetch_error, last_fetch_error_at)
                 VALUES (?, ?, ?, ?)",
                rusqlite::params![
                    "broken feed",
                    "https://example.com/broken.xml",
                    r#"{"type":"http_status","url":"https://example.com/broken.xml","status":503}"#,
                    1700000000i64,
                ],
            )?;
        }

        let resp = client.get("http://localhost/v1/feeds").send().await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body: serde_json::Value = resp.json().await?;

        let feeds = body["feeds"].as_array().expect("feeds array");
        assert_eq!(feeds.len(), 1);
        assert!(
            feeds[0].get("last_fetch_error").is_none(),
            "list endpoint leaked last_fetch_error: {:?}",
            feeds[0]
        );
        assert!(
            feeds[0].get("last_fetch_error_at").is_none(),
            "list endpoint leaked last_fetch_error_at: {:?}",
            feeds[0]
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_feed_tags() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;

        populate_tags(&tc)?;
        populate_feeds_and_entries(&tc)?;

        // Initially no tags on feed
        let resp = client
            .get("http://localhost/v1/feeds/id/1/tags")
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = resp.json::<feed_tags::GetFeedTagsResponse>().await?;
        assert_eq!(json.tags.len(), 0);

        // Set tags on feed
        let resp = client
            .put("http://localhost/v1/feeds/id/1/tags")
            .json(&feed_tags::SetFeedTagsRequest {
                tag_ids: vec![4, 5],
            })
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = resp.json::<feed_tags::GetFeedTagsResponse>().await?;
        assert_eq!(json.tags.len(), 2);

        // Verify via GET
        let resp = client
            .get("http://localhost/v1/feeds/id/1/tags")
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = resp.json::<feed_tags::GetFeedTagsResponse>().await?;
        assert_eq!(json.tags.len(), 2);

        // Replace tags
        let resp = client
            .put("http://localhost/v1/feeds/id/1/tags")
            .json(&feed_tags::SetFeedTagsRequest { tag_ids: vec![6] })
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = resp.json::<feed_tags::GetFeedTagsResponse>().await?;
        assert_eq!(json.tags.len(), 1);
        assert_eq!(json.tags[0].name, "science");

        // Non-existent feed
        let resp = client
            .get("http://localhost/v1/feeds/id/999/tags")
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        // Non-existent tag in set
        let resp = client
            .put("http://localhost/v1/feeds/id/1/tags")
            .json(&feed_tags::SetFeedTagsRequest { tag_ids: vec![999] })
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // System tags can't be applied to feeds
        let read_id = crate::db::tags::SystemTag::Read.id(&tc.database_conn()?)?;
        let resp = client
            .put("http://localhost/v1/feeds/id/1/tags")
            .json(&feed_tags::SetFeedTagsRequest {
                tag_ids: vec![read_id],
            })
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        Ok(())
    }

    /// A feed's schedule as `(next_fetch_at, consecutive_failures)`.
    fn feed_schedule(tc: &TestConfig, feed_id: i64) -> Result<(Option<i64>, i64)> {
        Ok(tc.database_conn()?.query_row(
            "SELECT next_fetch_at, consecutive_failures FROM feeds WHERE id = ?1",
            [feed_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?)
    }

    /// Insert a feed with URL `url`, last checked at `last_checked`, due at
    /// `next_fetch_at`, with a 3h interval and `failures` failures in a row.
    fn insert_scheduled_feed(
        tc: &TestConfig,
        url: &str,
        last_checked: i64,
        next_fetch_at: i64,
        failures: i64,
    ) -> Result<i64> {
        let conn = tc.database_conn()?;
        conn.execute(
            "INSERT INTO feeds (title, url, min_fetch_interval_seconds, last_checked,
                next_fetch_at, consecutive_failures)
             VALUES ('feed', ?1, 10800, ?2, ?3, ?4)",
            rusqlite::params![url, last_checked, next_fetch_at, failures],
        )?;
        Ok(conn.last_insert_rowid())
    }

    async fn put_feed(
        client: &reqwest::Client,
        feed_id: i64,
        body: serde_json::Value,
    ) -> Result<()> {
        let resp = client
            .put(format!("http://localhost/v1/feeds/id/{feed_id}"))
            .json(&body)
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        Ok(())
    }

    #[tokio::test]
    async fn test_update_feed_reschedules_on_new_url_or_credentials() -> Result<()> {
        let tc = TestBuilder::all().init_server().build()?;
        let client = tc.client()?;
        let now = chrono::Utc::now().timestamp();

        // Re-saving the same URL, or editing the title, leaves it alone.
        let feed_id =
            insert_scheduled_feed(&tc, "https://example.com/feed.xml", now, now + 3600, 2)?;
        put_feed(
            &client,
            feed_id,
            serde_json::json!({"title": "renamed", "url": "https://example.com/feed.xml"}),
        )
        .await?;
        assert_eq!(feed_schedule(&tc, feed_id)?, (Some(now + 3600), 2));

        // A new URL makes it due now and forgets the failure streak.
        put_feed(
            &client,
            feed_id,
            serde_json::json!({"url": "https://example.com/moved.xml"}),
        )
        .await?;
        assert_eq!(feed_schedule(&tc, feed_id)?, (None, 0));

        // So do new credentials.
        let feed_id =
            insert_scheduled_feed(&tc, "https://example.com/other.xml", now, now + 3600, 2)?;
        put_feed(
            &client,
            feed_id,
            serde_json::json!({"auth_type": "bearer", "auth_bearer_token": "secret"}),
        )
        .await?;
        assert_eq!(feed_schedule(&tc, feed_id)?, (None, 0));

        Ok(())
    }

    #[tokio::test]
    async fn test_update_feed_shorter_interval_brings_next_fetch_forward() -> Result<()> {
        let tc = TestBuilder::all().init_server().build()?;
        let client = tc.client()?;
        let now = chrono::Utc::now().timestamp();
        let last_checked = now - 1800;

        // A healthy feed is due one new interval after its last check.
        let feed_id = insert_scheduled_feed(
            &tc,
            "https://example.com/feed.xml",
            last_checked,
            last_checked + 10800,
            0,
        )?;
        put_feed(
            &client,
            feed_id,
            serde_json::json!({"min_fetch_interval_seconds": 3600}),
        )
        .await?;
        assert_eq!(feed_schedule(&tc, feed_id)?, (Some(last_checked + 3600), 0));

        // A longer interval waits for the next fetch to take effect.
        put_feed(
            &client,
            feed_id,
            serde_json::json!({"min_fetch_interval_seconds": 86400}),
        )
        .await?;
        assert_eq!(feed_schedule(&tc, feed_id)?, (Some(last_checked + 3600), 0));

        // A feed that is backing off keeps its backoff.
        let feed_id = insert_scheduled_feed(
            &tc,
            "https://example.com/other.xml",
            last_checked,
            now + 7200,
            3,
        )?;
        put_feed(
            &client,
            feed_id,
            serde_json::json!({"min_fetch_interval_seconds": 600}),
        )
        .await?;
        assert_eq!(feed_schedule(&tc, feed_id)?, (Some(now + 7200), 3));

        Ok(())
    }
}
