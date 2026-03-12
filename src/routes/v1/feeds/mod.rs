mod add_feed;
mod delete_feed;
mod export_opml;
mod feed_entries;
mod fetch_all_feeds;
mod fetch_feed;
pub mod get_feed;
mod import_opml;
mod list_feeds;
mod update_feed;

use add_feed::add_feed;
use delete_feed::delete_feed;
use export_opml::export_opml;
use feed_entries::feed_entries;
use fetch_all_feeds::fetch_all_feeds;
use fetch_feed::fetch_feed;
use get_feed::get_feed;
use import_opml::import_opml;
use list_feeds::list_feeds;
use update_feed::update_feed;

use crate::routes::v1::tags::{get_feed_tags, set_feed_tags};
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
        .route("/fetch", post(fetch_all_feeds))
        .route("/fetch/{id}", post(fetch_feed))
        .route("/import", post(import_opml))
        .route("/export", get(export_opml))
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::routes::v1::entries::ListEntriesResponse;
    use crate::test::{TestBuilder, TestConfig};
    use anyhow::Result;
    use axum::http::StatusCode;
    use std::time::Duration;

    async fn add_example_feed(tc: &TestConfig) -> Result<i64> {
        let client = tc.client()?;

        // Add a new feed via the API
        let url = tc.example_feed_url();
        let resp = client
            .post("http://kiki/v1/feeds/create")
            .json(&add_feed::AddFeedRequest {
                title: "my feed".to_string(),
                url: url.clone(),
            })
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let feed_id = resp.json::<add_feed::AddFeedResponse>().await?.id;

        // Wait a short period of time for the feed to get fetched
        std::thread::sleep(Duration::from_millis(250));

        Ok(feed_id)
    }

    #[tokio::test]
    async fn test_add_feed() -> Result<()> {
        let tc = TestBuilder::all().init_server().build()?;
        let client = tc.client()?;

        // We should start off with zero feeds and zero entries
        let resp = client.get("http://kiki/v1/feeds").send().await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = resp.json::<list_feeds::ListFeedsResponse>().await?;
        assert_eq!(json.feeds.len(), 0);
        assert_eq!(json.count, 0);
        assert_eq!(json.offset, 0);
        let resp = client.get("http://kiki/v1/feeds/id/1").send().await?;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_eq!(&resp.text().await?, "Feed not found");

        let resp = client.get("http://kiki/v1/entries").send().await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = resp.json::<ListEntriesResponse>().await?;
        assert_eq!(json.count, 0);

        let feed_id = add_example_feed(&tc).await?;
        let resp = client
            .get(format!("http://kiki/v1/feeds/id/{:?}", feed_id))
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
        let resp = client.get("http://kiki/v1/feeds").send().await?;
        assert_eq!(resp.status(), StatusCode::OK);

        // Check that the entries from the feed were retrieved
        let resp = client.get("http://kiki/v1/entries").send().await?;
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
        let resp = client.get("http://kiki/v1/feeds").send().await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = resp.json::<list_feeds::ListFeedsResponse>().await?;
        assert_eq!(json.feeds.len(), 0);
        assert_eq!(json.count, 0);

        // Add a feed and test that it appears in the list
        let resp = client
            .post("http://kiki/v1/feeds/create")
            .json(&add_feed::AddFeedRequest {
                title: "test feed".to_string(),
                url: "https://example.com/feed.xml".to_string(),
            })
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let feed_id = resp.json::<add_feed::AddFeedResponse>().await?.id;

        // Check that the feed appears in the list
        let resp = client.get("http://kiki/v1/feeds").send().await?;
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
            .post("http://kiki/v1/feeds/create")
            .json(&add_feed::AddFeedRequest {
                title: "test feed".to_string(),
                url: "https://example.com/feed.xml".to_string(),
            })
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let feed_id = resp.json::<add_feed::AddFeedResponse>().await?.id;

        // Verify the feed exists
        let resp = client.get("http://kiki/v1/feeds").send().await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = resp.json::<list_feeds::ListFeedsResponse>().await?;
        assert_eq!(json.feeds.len(), 1);
        assert_eq!(json.count, 1);
        assert_eq!(json.feeds[0].id, feed_id);
        assert_eq!(json.feeds[0].title, "test feed");
        assert_eq!(json.feeds[0].url, "https://example.com/feed.xml");

        // Delete the feed
        let resp = client
            .delete(format!("http://kiki/v1/feeds/id/{:?}", feed_id))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        // Verify the feed is gone
        let resp = client.get("http://kiki/v1/feeds").send().await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = resp.json::<list_feeds::ListFeedsResponse>().await?;
        assert_eq!(json.feeds.len(), 0);
        assert_eq!(json.count, 0);

        // Try to delete a non-existent feed
        let resp = client.delete("http://kiki/v1/feeds/id/999").send().await?;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let content = resp.text().await?;
        assert_eq!(&content, "Feed not found");

        Ok(())
    }

    #[tokio::test]
    async fn test_update_feed() -> Result<()> {
        let tc = TestBuilder::all().init_server().build()?;
        let client = tc.client()?;

        // Add a feed
        let resp = client
            .post("http://kiki/v1/feeds/create")
            .json(&add_feed::AddFeedRequest {
                title: "original title".to_string(),
                url: "https://example.com/feed.xml".to_string(),
            })
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let feed_id = resp.json::<add_feed::AddFeedResponse>().await?.id;

        // Verify the feed exists with original values
        let resp = client.get("http://kiki/v1/feeds").send().await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = resp.json::<list_feeds::ListFeedsResponse>().await?;
        assert_eq!(json.feeds.len(), 1);
        assert_eq!(json.feeds[0].id, feed_id);
        assert_eq!(json.feeds[0].title, "original title");
        assert_eq!(json.feeds[0].url, "https://example.com/feed.xml");

        // Update the feed
        let resp = client
            .put(format!("http://kiki/v1/feeds/id/{:?}", feed_id))
            .json(&update_feed::UpdateFeedRequest {
                title: Some("updated title".to_string()),
                url: None,
                description: Some("updated description".to_string()),
            })
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = resp.json::<update_feed::UpdateFeedResponse>().await?;
        assert_eq!(json.id, feed_id);
        assert_eq!(json.title, "updated title");
        assert_eq!(json.url, "https://example.com/feed.xml");
        assert_eq!(json.description, Some("updated description".to_string()));

        // Verify the feed was updated
        let resp = client.get("http://kiki/v1/feeds").send().await?;
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
            .put("http://kiki/v1/feeds/id/999")
            .json(&update_feed::UpdateFeedRequest {
                title: Some("non-existent title".to_string()),
                url: None,
                description: None,
            })
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let content = resp.text().await?;
        assert_eq!(&content, "Feed not found");

        // Try to update with no fields provided
        let resp = client
            .put(format!("http://kiki/v1/feeds/id/{:?}", feed_id))
            .json(&update_feed::UpdateFeedRequest {
                title: None,
                url: None,
                description: None,
            })
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        Ok(())
    }

    #[tokio::test]
    async fn test_fetch_feed() -> Result<()> {
        let tc = TestBuilder::all().init_server().build()?;
        let client = tc.client()?;

        // Add a feed
        let resp = client
            .post("http://kiki/v1/feeds/create")
            .json(&add_feed::AddFeedRequest {
                title: "test feed".to_string(),
                url: "https://example.com/feed.xml".to_string(),
            })
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let feed_id = resp.json::<add_feed::AddFeedResponse>().await?.id;

        // Test the fetch endpoint - should return 202 Accepted
        let resp = client
            .post(format!("http://kiki/v1/feeds/fetch/{:?}", feed_id))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::ACCEPTED);

        Ok(())
    }

    #[tokio::test]
    async fn test_export_opml_empty() -> Result<()> {
        let tc = TestBuilder::all().init_server().build()?;
        let client = tc.client()?;

        let resp = client.get("http://kiki/v1/feeds/export").send().await?;
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

        let resp = client.get("http://kiki/v1/feeds/export").send().await?;
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
                "INSERT INTO feed_tags (feed_id, tag_id) VALUES (?, ?)",
                [1i64, 1i64],
            )?;
        }

        let resp = client.get("http://kiki/v1/feeds/export").send().await?;
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
            .post("http://kiki/v1/feeds/import")
            .body(opml)
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let json = resp.json::<import_opml::ImportOpmlResponse>().await?;
        assert_eq!(json.imported, 2);

        // Verify feeds were created
        let resp = client.get("http://kiki/v1/feeds").send().await?;
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
            .post("http://kiki/v1/feeds/import")
            .body(opml)
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let json = resp.json::<import_opml::ImportOpmlResponse>().await?;
        assert_eq!(json.imported, 2);

        // Verify both feeds were created
        let resp = client.get("http://kiki/v1/feeds").send().await?;
        let list = resp.json::<list_feeds::ListFeedsResponse>().await?;
        assert_eq!(list.count, 2);

        // The feed inside the folder should have the folder name as a tag
        let tech_feed = list.feeds.iter().find(|f| f.title == "Tech Blog").unwrap();
        let resp = client
            .get(format!("http://kiki/v1/feeds/id/{}/tags", tech_feed.id))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let tags_resp = resp
            .json::<crate::routes::v1::tags::feed_tags::GetFeedTagsResponse>()
            .await?;
        assert_eq!(tags_resp.tags.len(), 1);
        assert_eq!(tags_resp.tags[0].name, "tech");

        // The top-level feed should have no tags
        let untagged = list
            .feeds
            .iter()
            .find(|f| f.title == "Untagged Feed")
            .unwrap();
        let resp = client
            .get(format!("http://kiki/v1/feeds/id/{}/tags", untagged.id))
            .send()
            .await?;
        let tags_resp = resp
            .json::<crate::routes::v1::tags::feed_tags::GetFeedTagsResponse>()
            .await?;
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
            .post("http://kiki/v1/feeds/import")
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
            .post("http://kiki/v1/feeds/import")
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
            .post("http://kiki/v1/feeds/import")
            .body(opml)
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let json = resp.json::<import_opml::ImportOpmlResponse>().await?;
        assert_eq!(json.imported, 2);

        // Export and verify the OPML contains the imported feeds and tags
        let resp = client.get("http://kiki/v1/feeds/export").send().await?;
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
}
