pub mod create_tag;
pub mod delete_tag;
pub mod get_tag;
pub mod list_tags;
pub mod tag_entries;
pub mod tag_feeds;
pub mod update_tag;

use create_tag::create_tag;
use delete_tag::delete_tag;
use get_tag::get_tag;
use list_tags::list_tags;
use tag_entries::tag_entries;
use tag_feeds::tag_feeds;
use update_tag::update_tag;

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
#[allow(clippy::indexing_slicing)]
mod test {
    use super::*;
    use crate::db::tags::{SystemTag, TagKind};
    use crate::test::{TestBuilder, TestConfig};
    use anyhow::Result;
    use axum::http::StatusCode;

    /// Add user tags "news", "tech", and "science", with IDs 4, 5, and 6
    /// (after the system tags).
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

        // A new database only has the system tags
        let resp = client.get("http://localhost/v1/tags").send().await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = resp.json::<list_tags::ListTagsResponse>().await?;
        assert_eq!(json.count, SystemTag::ALL.len());
        let names: Vec<_> = json.tags.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, SystemTag::ALL.map(SystemTag::name));
        assert!(json.tags.iter().all(|t| t.kind == TagKind::System));

        let resp = client
            .get("http://localhost/v1/tags?kind=user")
            .send()
            .await?;
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
            .post("http://localhost/v1/tags/create")
            .json(&create_tag::CreateTagRequest {
                name: "news".to_string(),
            })
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let tag = resp.json::<create_tag::CreateTagResponse>().await?;
        assert_eq!(tag.name, "news");

        // List tags
        let resp = client
            .get("http://localhost/v1/tags?kind=user")
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = resp.json::<list_tags::ListTagsResponse>().await?;
        assert_eq!(json.tags.len(), 1);
        assert_eq!(json.count, 1);
        assert_eq!(json.tags[0].name, "news");
        assert_eq!(json.tags[0].kind, TagKind::User);

        let resp = client
            .get("http://localhost/v1/tags?kind=system")
            .send()
            .await?;
        let json = resp.json::<list_tags::ListTagsResponse>().await?;
        assert_eq!(json.count, SystemTag::ALL.len());

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
            .post("http://localhost/v1/tags/create")
            .json(&req)
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::CREATED);

        // Try to create a duplicate
        let resp = client
            .post("http://localhost/v1/tags/create")
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

        let resp = client.get("http://localhost/v1/tags/id/4").send().await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let tag = resp.json::<get_tag::GetTagResponse>().await?;
        assert_eq!(tag.id, 4);
        assert_eq!(tag.name, "news");
        assert_eq!(tag.kind, TagKind::User);

        // System tag
        let resp = client.get("http://localhost/v1/tags/id/1").send().await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let tag = resp.json::<get_tag::GetTagResponse>().await?;
        assert_eq!(tag.name, SystemTag::Read.name());
        assert_eq!(tag.kind, TagKind::System);

        // Non-existent tag
        let resp = client.get("http://localhost/v1/tags/id/999").send().await?;
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
            .put("http://localhost/v1/tags/id/4")
            .json(&update_tag::UpdateTagRequest {
                name: "breaking-news".to_string(),
            })
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let tag = resp.json::<update_tag::UpdateTagResponse>().await?;
        assert_eq!(tag.id, 4);
        assert_eq!(tag.name, "breaking-news");

        // Non-existent tag
        let resp = client
            .put("http://localhost/v1/tags/id/999")
            .json(&update_tag::UpdateTagRequest {
                name: "nope".to_string(),
            })
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        // Duplicate name conflict
        let resp = client
            .put("http://localhost/v1/tags/id/4")
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

        let resp = client
            .delete("http://localhost/v1/tags/id/4")
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        // Verify it's gone
        let resp = client.get("http://localhost/v1/tags/id/4").send().await?;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        // Delete non-existent
        let resp = client
            .delete("http://localhost/v1/tags/id/999")
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
                [1, 4],
            )?;
            conn.execute(
                "INSERT INTO feed_tags (feed_id, tag_id) VALUES (?, ?)",
                [2, 4],
            )?;
        }

        let resp = client
            .get("http://localhost/v1/tags/id/4/feeds")
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = resp.json::<tag_feeds::TagFeedsResponse>().await?;
        assert_eq!(json.count, 2);
        assert_eq!(json.feeds.len(), 2);

        // Tag with no feeds
        let resp = client
            .get("http://localhost/v1/tags/id/6/feeds")
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = resp.json::<tag_feeds::TagFeedsResponse>().await?;
        assert_eq!(json.count, 0);

        // Non-existent tag
        let resp = client
            .get("http://localhost/v1/tags/id/999/feeds")
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
                [1, 5],
            )?;
        }

        let resp = client
            .get("http://localhost/v1/tags/id/5/entries")
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = resp.json::<tag_entries::TagEntriesResponse>().await?;
        assert_eq!(json.count, 1);
        assert_eq!(json.entries.len(), 1);
        assert_eq!(json.entries[0].title, "Entry 1");

        Ok(())
    }

    /// Names with the reserved `system:` prefix cannot be used for user tags.
    #[tokio::test]
    async fn test_reserved_tag_names() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;

        populate_tags(&tc)?;

        for name in ["system:read", "system:starred", "SYSTEM:x"] {
            let resp = client
                .post("http://localhost/v1/tags/create")
                .json(&create_tag::CreateTagRequest {
                    name: name.to_string(),
                })
                .send()
                .await?;
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{name}");

            let resp = client
                .put("http://localhost/v1/tags/id/4")
                .json(&update_tag::UpdateTagRequest {
                    name: name.to_string(),
                })
                .send()
                .await?;
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{name}");
        }

        Ok(())
    }

    /// System tags cannot be renamed or deleted.
    #[tokio::test]
    async fn test_system_tags_are_immutable() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;

        let id = SystemTag::Read.id(&tc.database_conn()?)?;

        let resp = client
            .put(format!("http://localhost/v1/tags/id/{id}"))
            .json(&update_tag::UpdateTagRequest {
                name: "renamed".to_string(),
            })
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        let resp = client
            .delete(format!("http://localhost/v1/tags/id/{id}"))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        let resp = client
            .get(format!("http://localhost/v1/tags/id/{id}"))
            .send()
            .await?;
        let tag = resp.json::<get_tag::GetTagResponse>().await?;
        assert_eq!(tag.name, SystemTag::Read.name());

        Ok(())
    }
}
