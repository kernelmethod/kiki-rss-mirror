mod add_feed;
mod delete_feed;
mod fetch_feed;
mod get_feed;
mod list_feeds;
mod update_feed;

use add_feed::add_feed;
use delete_feed::delete_feed;
use fetch_feed::fetch_feed;
use get_feed::get_feed;
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
            "/id/{*id}",
            get(get_feed).delete(delete_feed).put(update_feed),
        )
        .route("/fetch/{*id}", post(fetch_feed))
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
}
