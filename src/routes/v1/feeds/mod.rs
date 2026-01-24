mod add_feed;
mod delete_feed;
mod get_feed;
mod list_feeds;

use add_feed::add_feed;
use delete_feed::delete_feed;
use get_feed::get_feed;
use list_feeds::list_feeds;

use crate::server::AppState;
use axum::{routing::get, Router};

pub fn create_router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_feeds).post(add_feed))
        .route("/{*id}", get(get_feed).delete(delete_feed))
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::test::TestBuilder;
    use anyhow::Result;
    use axum::http::StatusCode;

    #[tokio::test]
    async fn test_add_feed() -> Result<()> {
        let tc = TestBuilder::all().init_server().build()?;
        let client = tc.client()?;

        // We should start off with zero feeds
        let resp = client.get("http://kiki/v1/feeds").send().await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = resp.json::<list_feeds::ListFeedsResponse>().await?;
        assert_eq!(json.feeds.len(), 0);
        assert_eq!(json.count, 0);
        assert_eq!(json.offset, 0);
        let resp = client.get("http://kiki/v1/feeds/0").send().await?;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let json = resp.json::<get_feed::GetFeedError>().await?;
        assert_eq!(json.id, 0);
        assert_eq!(json.message, "not found");

        // Add a new feed to the database
        let resp = client
            .post("http://kiki/v1/feeds")
            .json(&add_feed::AddFeedRequest {
                title: "my feed".to_string(),
                url: "https://kernelmethod.org/rss.xml".to_string(),
            })
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let feed_id = resp.json::<add_feed::AddFeedResponse>().await?.id;

        // Now retrieve the feed from the database
        let resp = client
            .get(format!("http://kiki/v1/feeds/{:?}", feed_id))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = resp.json::<get_feed::GetFeedResponse>().await?;
        assert_eq!(json.id, feed_id);
        assert_eq!(json.title, "my feed");
        assert_eq!(json.url, "https://kernelmethod.org/rss.xml");
        assert_eq!(json.description, None);
        assert_eq!(json.last_checked, None);

        // We should also see the feed in the list of feeds
        let resp = client.get("http://kiki/v1/feeds").send().await?;
        assert_eq!(resp.status(), StatusCode::OK);

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
            .post("http://kiki/v1/feeds")
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
            .post("http://kiki/v1/feeds")
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
            .delete(format!("http://kiki/v1/feeds/{:?}", feed_id))
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
        let resp = client.delete("http://kiki/v1/feeds/999").send().await?;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        Ok(())
    }
}
