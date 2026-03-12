mod create_tag;
mod delete_tag;
pub mod entry_tags;
pub mod feed_tags;
mod get_tag;
pub mod list_tags;
mod tag_entries;
mod tag_feeds;
mod update_tag;

use create_tag::create_tag;
use delete_tag::delete_tag;
use get_tag::get_tag;
use list_tags::list_tags;
use tag_entries::tag_entries;
use tag_feeds::tag_feeds;
use update_tag::update_tag;

pub use entry_tags::{get_entry_tags, set_entry_tags};
pub use feed_tags::{get_feed_tags, set_feed_tags};

use crate::server::AppState;
use axum::{
    routing::{get, post},
    Router,
};

pub fn create_router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_tags))
        .route("/create", post(create_tag))
        .route("/id/{id}", get(get_tag).put(update_tag).delete(delete_tag))
        .route("/id/{id}/feeds", get(tag_feeds))
        .route("/id/{id}/entries", get(tag_entries))
}

#[cfg(test)]
mod test {
    use super::*;
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

    #[tokio::test]
    async fn test_list_tags_empty() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;

        let resp = client.get("http://kiki/v1/tags").send().await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = resp.json::<list_tags::ListTagsResponse>().await?;
        assert_eq!(json.tags.len(), 0);
        assert_eq!(json.count, 0);

        Ok(())
    }

    #[tokio::test]
    async fn test_create_and_list_tags() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;

        // Create a tag
        let resp = client
            .post("http://kiki/v1/tags/create")
            .json(&create_tag::CreateTagRequest {
                name: "news".to_string(),
            })
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let tag = resp.json::<create_tag::CreateTagResponse>().await?;
        assert_eq!(tag.name, "news");

        // List tags
        let resp = client.get("http://kiki/v1/tags").send().await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = resp.json::<list_tags::ListTagsResponse>().await?;
        assert_eq!(json.tags.len(), 1);
        assert_eq!(json.count, 1);
        assert_eq!(json.tags[0].name, "news");

        Ok(())
    }

    #[tokio::test]
    async fn test_create_duplicate_tag() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;

        let req = create_tag::CreateTagRequest {
            name: "news".to_string(),
        };

        let resp = client
            .post("http://kiki/v1/tags/create")
            .json(&req)
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::CREATED);

        // Try to create a duplicate
        let resp = client
            .post("http://kiki/v1/tags/create")
            .json(&req)
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::CONFLICT);

        Ok(())
    }

    #[tokio::test]
    async fn test_get_tag() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;

        populate_tags(&tc)?;

        let resp = client.get("http://kiki/v1/tags/id/1").send().await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let tag = resp.json::<get_tag::GetTagResponse>().await?;
        assert_eq!(tag.id, 1);
        assert_eq!(tag.name, "news");

        // Non-existent tag
        let resp = client.get("http://kiki/v1/tags/id/999").send().await?;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        Ok(())
    }

    #[tokio::test]
    async fn test_update_tag() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;

        populate_tags(&tc)?;

        // Rename tag
        let resp = client
            .put("http://kiki/v1/tags/id/1")
            .json(&update_tag::UpdateTagRequest {
                name: "breaking-news".to_string(),
            })
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let tag = resp.json::<update_tag::UpdateTagResponse>().await?;
        assert_eq!(tag.id, 1);
        assert_eq!(tag.name, "breaking-news");

        // Non-existent tag
        let resp = client
            .put("http://kiki/v1/tags/id/999")
            .json(&update_tag::UpdateTagRequest {
                name: "nope".to_string(),
            })
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        // Duplicate name conflict
        let resp = client
            .put("http://kiki/v1/tags/id/1")
            .json(&update_tag::UpdateTagRequest {
                name: "tech".to_string(),
            })
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::CONFLICT);

        Ok(())
    }

    #[tokio::test]
    async fn test_delete_tag() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;

        populate_tags(&tc)?;

        let resp = client.delete("http://kiki/v1/tags/id/1").send().await?;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        // Verify it's gone
        let resp = client.get("http://kiki/v1/tags/id/1").send().await?;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        // Delete non-existent
        let resp = client.delete("http://kiki/v1/tags/id/999").send().await?;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        Ok(())
    }

    #[tokio::test]
    async fn test_feed_tags() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;

        populate_tags(&tc)?;
        populate_feeds_and_entries(&tc)?;

        // Initially no tags on feed
        let resp = client.get("http://kiki/v1/feeds/id/1/tags").send().await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = resp.json::<feed_tags::GetFeedTagsResponse>().await?;
        assert_eq!(json.tags.len(), 0);

        // Set tags on feed
        let resp = client
            .put("http://kiki/v1/feeds/id/1/tags")
            .json(&feed_tags::SetFeedTagsRequest {
                tag_ids: vec![1, 2],
            })
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = resp.json::<feed_tags::GetFeedTagsResponse>().await?;
        assert_eq!(json.tags.len(), 2);

        // Verify via GET
        let resp = client.get("http://kiki/v1/feeds/id/1/tags").send().await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = resp.json::<feed_tags::GetFeedTagsResponse>().await?;
        assert_eq!(json.tags.len(), 2);

        // Replace tags
        let resp = client
            .put("http://kiki/v1/feeds/id/1/tags")
            .json(&feed_tags::SetFeedTagsRequest { tag_ids: vec![3] })
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = resp.json::<feed_tags::GetFeedTagsResponse>().await?;
        assert_eq!(json.tags.len(), 1);
        assert_eq!(json.tags[0].name, "science");

        // Non-existent feed
        let resp = client
            .get("http://kiki/v1/feeds/id/999/tags")
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        // Non-existent tag in set
        let resp = client
            .put("http://kiki/v1/feeds/id/1/tags")
            .json(&feed_tags::SetFeedTagsRequest { tag_ids: vec![999] })
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        Ok(())
    }

    #[tokio::test]
    async fn test_entry_tags() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;

        populate_tags(&tc)?;
        populate_feeds_and_entries(&tc)?;

        // Initially no tags on entry
        let resp = client
            .get("http://kiki/v1/entries/id/1/tags")
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = resp.json::<entry_tags::GetEntryTagsResponse>().await?;
        assert_eq!(json.tags.len(), 0);

        // Set tags on entry
        let resp = client
            .put("http://kiki/v1/entries/id/1/tags")
            .json(&entry_tags::SetEntryTagsRequest {
                tag_ids: vec![1, 3],
            })
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = resp.json::<entry_tags::GetEntryTagsResponse>().await?;
        assert_eq!(json.tags.len(), 2);

        // Non-existent entry
        let resp = client
            .get("http://kiki/v1/entries/id/999/tags")
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        Ok(())
    }

    #[tokio::test]
    async fn test_tag_feeds() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;

        populate_tags(&tc)?;
        populate_feeds_and_entries(&tc)?;

        // Associate feeds with tags
        {
            let conn = tc.database_conn()?;
            conn.execute(
                "INSERT INTO feed_tags (feed_id, tag_id) VALUES (?, ?)",
                [1, 1],
            )?;
            conn.execute(
                "INSERT INTO feed_tags (feed_id, tag_id) VALUES (?, ?)",
                [2, 1],
            )?;
        }

        let resp = client.get("http://kiki/v1/tags/id/1/feeds").send().await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = resp.json::<tag_feeds::TagFeedsResponse>().await?;
        assert_eq!(json.count, 2);
        assert_eq!(json.feeds.len(), 2);

        // Tag with no feeds
        let resp = client.get("http://kiki/v1/tags/id/3/feeds").send().await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = resp.json::<tag_feeds::TagFeedsResponse>().await?;
        assert_eq!(json.count, 0);

        // Non-existent tag
        let resp = client
            .get("http://kiki/v1/tags/id/999/feeds")
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        Ok(())
    }

    #[tokio::test]
    async fn test_tag_entries() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;

        populate_tags(&tc)?;
        populate_feeds_and_entries(&tc)?;

        // Associate entries with tags
        {
            let conn = tc.database_conn()?;
            conn.execute(
                "INSERT INTO entry_tags (entry_id, tag_id) VALUES (?, ?)",
                [1, 2],
            )?;
        }

        let resp = client
            .get("http://kiki/v1/tags/id/2/entries")
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = resp.json::<tag_entries::TagEntriesResponse>().await?;
        assert_eq!(json.count, 1);
        assert_eq!(json.entries.len(), 1);
        assert_eq!(json.entries[0].title, "Entry 1");

        Ok(())
    }
}
