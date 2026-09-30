pub mod batch_entries;
pub mod cleanup;
pub mod delete_entry;
pub mod entry_assets;
pub mod entry_tags;
pub mod format_data;
pub mod get_entry;
pub mod list_entries;
pub mod rows;
pub mod search_entries;

use batch_entries::batch_entries;
use cleanup::cleanup;
use delete_entry::delete_entry;
use entry_assets::list_entry_assets;
use entry_tags::{add_entry_system_tag, get_entry_tags, remove_entry_system_tag, set_entry_tags};
use get_entry::get_entry;
#[allow(unused_imports)]
pub use list_entries::{list_entries, ListEntriesResponse, ListEntriesResponseEntry};
use search_entries::{search_entries, search_entry_ids};

use crate::server::AppState;
use axum::{
    routing::{get, post, put},
    Router,
};

pub fn create_router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_entries))
        .route("/cleanup", post(cleanup))
        .route("/batch", post(batch_entries))
        .route("/search", post(search_entries))
        .route("/search/ids", post(search_entry_ids))
        .route("/id/{id}", get(get_entry).delete(delete_entry))
        .route("/id/{id}/tags", get(get_entry_tags).put(set_entry_tags))
        .route(
            "/id/{id}/system-tags/{name}",
            put(add_entry_system_tag).delete(remove_entry_system_tag),
        )
        .route("/id/{id}/assets", get(list_entry_assets))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::expect_used)]
mod test {
    use super::*;
    use crate::db::tags::{SystemTag, TagKind};
    use crate::test::{TestBuilder, TestConfig};
    use anyhow::Result;
    use axum::http::StatusCode;
    use chrono::{TimeZone, Utc};

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

    fn populate_entries(tc: &TestConfig) -> Result<()> {
        // Insert some test entries
        let conn = tc.database_conn().unwrap();

        // Insert a feed first
        conn.execute(
            "INSERT INTO feeds (title, url, syndication_format) VALUES (?, ?, ?)",
            ["Test Feed", "http://example.com/feed", "rss"],
        )?;

        let feed_id = conn.last_insert_rowid();

        // Insert RSS entry
        conn.execute(
            "INSERT INTO entries (feed_id, syndication_format, guid,
                published_at, title, url, content)
            VALUES (?, ?, ?, ?, ?, ?, ?)",
            (
                feed_id,
                "rss",
                "rss-guid-1",
                Utc.with_ymd_and_hms(2026, 5, 15, 0, 0, 0)
                    .unwrap()
                    .timestamp(),
                "RSS Entry",
                "http://example.com/rss-entry",
                "RSS Content",
            ),
        )?;

        // Insert Atom entry
        conn.execute(
            "INSERT INTO entries (feed_id, syndication_format, guid,
                published_at, title, url, content)
            VALUES (?, ?, ?, ?, ?, ?, ?)",
            (
                feed_id,
                "atom",
                "atom-guid-1",
                Utc.with_ymd_and_hms(2026, 5, 16, 0, 0, 0)
                    .unwrap()
                    .timestamp(),
                "Atom Entry",
                "http://example.com/atom-entry",
                "Atom Content",
            ),
        )?;

        Ok(())
    }

    #[tokio::test]
    async fn test_list_entries() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;

        populate_entries(&tc)?;

        // Call the list_entries endpoint
        let response = client.get("http://localhost/v1/entries").send().await?;

        // Verify the response
        assert_eq!(response.status(), StatusCode::OK);
        let response = response.json::<list_entries::ListEntriesResponse>().await?;
        assert_eq!(response.count, 2);
        assert_eq!(response.offset, 0);
        assert_eq!(response.limit, list_entries::DEFAULT_LIMIT);
        assert_eq!(response.entries.len(), 2);

        // Check that both RSS and Atom entries are returned
        let entry_formats: Vec<String> = response
            .entries
            .iter()
            .map(|entry| entry.syndication_format.clone())
            .collect();

        assert!(entry_formats.contains(&"rss".to_string()));
        assert!(entry_formats.contains(&"atom".to_string()));

        // Validate the publication dates that are returned (newest first)
        assert_eq!(
            response.entries[0].published_at.as_deref(),
            Some("2026-05-16T00:00:00+00:00")
        );
        assert_eq!(
            response.entries[1].published_at.as_deref(),
            Some("2026-05-15T00:00:00+00:00")
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_list_entries_leaves_out_hidden_entries() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        populate_feeds_and_entries(&tc)?;
        {
            let conn = tc.database_conn()?;
            let hidden = SystemTag::Hidden.id(&conn)?;
            conn.execute(
                "INSERT INTO entry_tags (entry_id, tag_id) VALUES (1, ?1)",
                [hidden],
            )?;
        }

        let titles = |r: &list_entries::ListEntriesResponse| -> Vec<String> {
            r.entries.iter().map(|e| e.title.clone()).collect()
        };

        let response = client
            .get("http://localhost/v1/entries")
            .send()
            .await?
            .json::<list_entries::ListEntriesResponse>()
            .await?;
        assert_eq!(response.count, 1);
        assert_eq!(titles(&response), ["Entry 2"]);

        let response = client
            .get("http://localhost/v1/entries?include_hidden=true")
            .send()
            .await?
            .json::<list_entries::ListEntriesResponse>()
            .await?;
        assert_eq!(response.count, 2);
        assert_eq!(titles(&response), ["Entry 2", "Entry 1"]);

        Ok(())
    }

    #[tokio::test]
    async fn test_list_entries_ordering() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        let conn = tc.database_conn()?;
        conn.execute(
            "INSERT INTO feeds (title, url, syndication_format) VALUES ('A', 'http://a', 'rss')",
            [],
        )?;
        let feed_a = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO feeds (title, url, syndication_format) VALUES ('B', 'http://b', 'rss')",
            [],
        )?;
        let feed_b = conn.last_insert_rowid();

        // Insert out of chronological order, across feeds, with two entries
        // sharing a timestamp, and one orphaned entry.
        let insert = |feed_id: Option<i64>, guid: &str, published_at: i64| -> Result<i64> {
            conn.execute(
                "INSERT INTO entries (feed_id, syndication_format, guid, published_at, title, url)
                 VALUES (?1, 'rss', ?2, ?3, ?2, 'http://example.com')",
                rusqlite::params![feed_id, guid, published_at],
            )?;
            Ok(conn.last_insert_rowid())
        };
        let middle = insert(Some(feed_a), "middle", 2_000)?;
        let oldest = insert(Some(feed_b), "oldest", 1_000)?;
        let tie_first = insert(Some(feed_b), "tie-first", 3_000)?;
        let orphan = insert(None, "orphan", 1_500)?;
        let tie_second = insert(Some(feed_a), "tie-second", 3_000)?;
        let expected = vec![tie_second, tie_first, middle, orphan, oldest];

        let response = client.get("http://localhost/v1/entries").send().await?;
        assert_eq!(response.status(), StatusCode::OK);
        let response = response.json::<list_entries::ListEntriesResponse>().await?;
        let ids: Vec<i64> = response.entries.iter().map(|e| e.id).collect();
        assert_eq!(ids, expected);

        // Paging through the list yields the same order.
        let mut paged = Vec::new();
        for offset in (0..expected.len()).step_by(2) {
            let response = client
                .get(format!(
                    "http://localhost/v1/entries?offset={offset}&limit=2"
                ))
                .send()
                .await?;
            assert_eq!(response.status(), StatusCode::OK);
            let response = response.json::<list_entries::ListEntriesResponse>().await?;
            paged.extend(response.entries.iter().map(|e| e.id));
        }
        assert_eq!(paged, expected);

        Ok(())
    }

    #[tokio::test]
    async fn test_get_entry() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;

        populate_entries(&tc)?;

        // Call the get_entry endpoint
        let response = client
            .get("http://localhost/v1/entries/id/1")
            .send()
            .await?;

        // Verify the response
        assert_eq!(response.status(), StatusCode::OK);
        let response = response.json::<get_entry::GetEntryResponse>().await?;

        assert_eq!(response.id, 1);
        assert_eq!(response.feed_id, Some(1));
        assert_eq!(response.syndication_format, "rss");
        assert_eq!(response.guid, "rss-guid-1");
        assert_eq!(response.title, "RSS Entry");
        assert_eq!(response.url, "http://example.com/rss-entry");
        assert_eq!(response.content.as_deref(), Some("RSS Content"));

        // Attempt to retrieve an entry that does not exist
        let response = client
            .get("http://localhost/v1/entries/id/1337")
            .send()
            .await?;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let content = response.text().await?;
        assert_eq!(&content, "Entry not found");

        Ok(())
    }

    #[tokio::test]
    async fn test_delete_entry() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;

        populate_entries(&tc)?;

        // Delete an existing entry
        let response = client
            .delete("http://localhost/v1/entries/id/1")
            .send()
            .await?;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        // Verify the entry was deleted by trying to retrieve it
        let response = client
            .get("http://localhost/v1/entries/id/1")
            .send()
            .await?;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(&response.text().await?, "Entry not found");

        // Try to delete a non-existent entry
        let response = client
            .delete("http://localhost/v1/entries/id/1337")
            .send()
            .await?;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(&response.text().await?, "Entry not found");

        Ok(())
    }

    #[tokio::test]
    async fn test_get_entry_includes_rss_data() -> Result<()> {
        let tc = TestBuilder::all().init_server().build()?;
        let client = tc.client()?;
        tc.add_feed_from_url("rich rss", tc.rich_rss_feed_url())
            .await?;

        // Locate the "item with everything" by guid.
        let conn = tc.database_conn()?;
        let entry_id: i64 = conn.query_row(
            "SELECT id FROM entries WHERE guid = ?1",
            ["http://example.com/items/1"],
            |row| row.get(0),
        )?;
        drop(conn);

        let resp = client
            .get(format!("http://localhost/v1/entries/id/{}", entry_id))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body: serde_json::Value = resp.json().await?;

        assert!(body["atom"].is_null(), "atom should be null for rss entry");
        let rss = body["rss"].as_object().expect("rss object present");
        assert_eq!(rss["enclosure_url"], "http://example.com/audio.mp3");
        assert_eq!(rss["enclosure_length"], 12345);
        assert_eq!(rss["enclosure_mime_type"], "audio/mpeg");
        assert_eq!(rss["author"], "alice@example.com (Alice)");
        assert_eq!(rss["comments"], "http://example.com/items/1/comments");
        assert_eq!(rss["description"], "A full-featured item.");

        let cats = rss["categories"].as_array().unwrap();
        assert_eq!(cats.len(), 2);

        Ok(())
    }

    #[tokio::test]
    async fn test_get_entry_includes_atom_data() -> Result<()> {
        let tc = TestBuilder::all().init_server().build()?;
        let client = tc.client()?;
        tc.add_feed_from_url("rich atom", tc.rich_atom_feed_url())
            .await?;

        let conn = tc.database_conn()?;
        let entry_id: i64 = conn.query_row(
            "SELECT id FROM entries WHERE guid = ?1",
            ["urn:uuid:1225c695-cfb8-4ebb-aaaa-80da344efa6a"],
            |row| row.get(0),
        )?;
        drop(conn);

        let resp = client
            .get(format!("http://localhost/v1/entries/id/{}", entry_id))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body: serde_json::Value = resp.json().await?;

        assert!(body["rss"].is_null(), "rss should be null for atom entry");
        let atom = body["atom"].as_object().expect("atom object present");
        assert_eq!(atom["rights"], "(c) 2026 Entry Author");
        assert_eq!(atom["authors"].as_array().unwrap().len(), 2);
        assert_eq!(atom["contributors"].as_array().unwrap().len(), 1);
        assert_eq!(atom["categories"].as_array().unwrap().len(), 1);
        assert_eq!(atom["categories"][0]["term"], "et1");

        Ok(())
    }

    #[tokio::test]
    async fn test_list_entries_does_not_include_format_data() -> Result<()> {
        let tc = TestBuilder::all().init_server().build()?;
        let client = tc.client()?;
        tc.add_feed_from_url("rich rss", tc.rich_rss_feed_url())
            .await?;

        let resp = client.get("http://localhost/v1/entries").send().await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body: serde_json::Value = resp.json().await?;

        let entries = body["entries"].as_array().expect("entries array");
        assert!(!entries.is_empty());
        for entry in entries {
            assert!(
                entry.get("rss").is_none(),
                "list endpoint leaked rss sub-object: {:?}",
                entry
            );
            assert!(
                entry.get("atom").is_none(),
                "list endpoint leaked atom sub-object: {:?}",
                entry
            );
        }

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
            .get("http://localhost/v1/entries/id/1/tags")
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = resp.json::<entry_tags::GetEntryTagsResponse>().await?;
        assert_eq!(json.tags.len(), 0);

        // Set tags on entry
        let resp = client
            .put("http://localhost/v1/entries/id/1/tags")
            .json(&entry_tags::SetEntryTagsRequest {
                tag_ids: vec![4, 6],
            })
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = resp.json::<entry_tags::GetEntryTagsResponse>().await?;
        assert_eq!(json.tags.len(), 2);

        // System tags can't be set through this endpoint
        let read_id = SystemTag::Read.id(&tc.database_conn()?)?;
        let resp = client
            .put("http://localhost/v1/entries/id/1/tags")
            .json(&entry_tags::SetEntryTagsRequest {
                tag_ids: vec![4, read_id],
            })
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // Non-existent entry
        let resp = client
            .get("http://localhost/v1/entries/id/999/tags")
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        Ok(())
    }

    /// System tags are managed through `/system-tags/{name}`, and survive
    /// replacing the entry's user tags.
    #[tokio::test]
    async fn test_entry_system_tags() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;

        populate_tags(&tc)?;
        populate_feeds_and_entries(&tc)?;

        let names = |json: &entry_tags::GetEntryTagsResponse| {
            json.tags.iter().map(|t| t.name.clone()).collect::<Vec<_>>()
        };

        // Mark as read, by short name, then saved, by full name
        let resp = client
            .put("http://localhost/v1/entries/id/1/system-tags/read")
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let resp = client
            .put("http://localhost/v1/entries/id/1/system-tags/system:saved")
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = resp.json::<entry_tags::GetEntryTagsResponse>().await?;
        assert_eq!(names(&json), ["system:read", "system:saved"]);
        assert!(json.tags.iter().all(|t| t.kind == TagKind::System));

        // Adding again is a no-op
        let resp = client
            .put("http://localhost/v1/entries/id/1/system-tags/read")
            .send()
            .await?;
        let json = resp.json::<entry_tags::GetEntryTagsResponse>().await?;
        assert_eq!(names(&json), ["system:read", "system:saved"]);

        // Setting user tags leaves the system tags alone
        let resp = client
            .put("http://localhost/v1/entries/id/1/tags")
            .json(&entry_tags::SetEntryTagsRequest { tag_ids: vec![5] })
            .send()
            .await?;
        let json = resp.json::<entry_tags::GetEntryTagsResponse>().await?;
        assert_eq!(names(&json), ["system:read", "system:saved", "tech"]);
        let resp = client
            .put("http://localhost/v1/entries/id/1/tags")
            .json(&entry_tags::SetEntryTagsRequest { tag_ids: vec![] })
            .send()
            .await?;
        let json = resp.json::<entry_tags::GetEntryTagsResponse>().await?;
        assert_eq!(names(&json), ["system:read", "system:saved"]);

        // Mark as unread
        let resp = client
            .delete("http://localhost/v1/entries/id/1/system-tags/read")
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = resp.json::<entry_tags::GetEntryTagsResponse>().await?;
        assert_eq!(names(&json), ["system:saved"]);

        // Unknown system tag, user tag name, and unknown entry
        for url in [
            "http://localhost/v1/entries/id/1/system-tags/starred",
            "http://localhost/v1/entries/id/1/system-tags/news",
            "http://localhost/v1/entries/id/999/system-tags/read",
        ] {
            let resp = client.put(url).send().await?;
            assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{url}");
        }

        // System tags can be used in search filters, e.g. to find unread
        // entries.
        let resp = client
            .post("http://localhost/v1/entries/search")
            .json(&serde_json::json!({"tags": {"not": "system:saved"}}))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<search_entries::SearchEntriesResponse>().await?;
        assert_eq!(body.count, 1);
        assert_eq!(body.entries[0].entry.title, "Entry 2");

        Ok(())
    }

    /// Helper to populate search test data: 4 entries with various tags, dates,
    /// titles, content, and URLs.
    fn populate_search_data(tc: &TestConfig) -> Result<()> {
        let conn = tc.database_conn()?;

        // Tags: news(4), tech(5), science(6), sports(7); IDs 1-3 are the
        // system tags.
        conn.execute("INSERT INTO tags (name) VALUES (?)", ["news"])?;
        conn.execute("INSERT INTO tags (name) VALUES (?)", ["tech"])?;
        conn.execute("INSERT INTO tags (name) VALUES (?)", ["science"])?;
        conn.execute("INSERT INTO tags (name) VALUES (?)", ["sports"])?;

        // Feed
        conn.execute(
            "INSERT INTO feeds (title, url, syndication_format) VALUES (?, ?, ?)",
            ["Test Feed", "http://example.com/feed", "rss"],
        )?;

        // Entry 1: "Breaking News Today" — news+tech, 2026-01-10
        conn.execute(
            "INSERT INTO entries (feed_id, syndication_format, guid, published_at, title, url, content)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                1, "rss", "g1",
                Utc.with_ymd_and_hms(2026, 1, 10, 12, 0, 0).unwrap().timestamp(),
                "Breaking News Today",
                "http://example.com/news-today",
                "Latest tech news coverage"
            ],
        )?;
        conn.execute(
            "INSERT INTO entry_tags (entry_id, tag_id) VALUES (1, 4)",
            [],
        )?;
        conn.execute(
            "INSERT INTO entry_tags (entry_id, tag_id) VALUES (1, 5)",
            [],
        )?;

        // Entry 2: "Science Discovery" — science, 2026-02-15
        conn.execute(
            "INSERT INTO entries (feed_id, syndication_format, guid, published_at, title, url, content)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                1, "rss", "g2",
                Utc.with_ymd_and_hms(2026, 2, 15, 8, 0, 0).unwrap().timestamp(),
                "Science Discovery",
                "http://example.com/science",
                "New scientific breakthrough in physics"
            ],
        )?;
        conn.execute(
            "INSERT INTO entry_tags (entry_id, tag_id) VALUES (2, 6)",
            [],
        )?;

        // Entry 3: "Tech Review" — tech+science, 2026-03-01
        conn.execute(
            "INSERT INTO entries (feed_id, syndication_format, guid, published_at, title, url, content)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                1, "rss", "g3",
                Utc.with_ymd_and_hms(2026, 3, 1, 10, 0, 0).unwrap().timestamp(),
                "Tech Review",
                "http://example.com/tech-review",
                "Reviewing the latest gadgets"
            ],
        )?;
        conn.execute(
            "INSERT INTO entry_tags (entry_id, tag_id) VALUES (3, 5)",
            [],
        )?;
        conn.execute(
            "INSERT INTO entry_tags (entry_id, tag_id) VALUES (3, 6)",
            [],
        )?;

        // Entry 4: "Sports Update" — sports, 2026-03-10 (no content)
        conn.execute(
            "INSERT INTO entries (feed_id, syndication_format, guid, published_at, title, url)
             VALUES (?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                1,
                "rss",
                "g4",
                Utc.with_ymd_and_hms(2026, 3, 10, 6, 0, 0)
                    .unwrap()
                    .timestamp(),
                "Sports Update",
                "http://example.com/sports"
            ],
        )?;
        conn.execute(
            "INSERT INTO entry_tags (entry_id, tag_id) VALUES (4, 7)",
            [],
        )?;

        Ok(())
    }

    #[tokio::test]
    async fn test_search_no_filters() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        populate_search_data(&tc)?;

        let resp = client
            .post("http://localhost/v1/entries/search")
            .json(&serde_json::json!({}))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<search_entries::SearchEntriesResponse>().await?;
        assert_eq!(body.count, 4);
        assert_eq!(body.entries.len(), 4);
        // Ordered by published_at DESC
        assert_eq!(body.entries[0].entry.title, "Sports Update");
        assert_eq!(body.entries[3].entry.title, "Breaking News Today");

        Ok(())
    }

    #[tokio::test]
    async fn test_search_single_tag() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        populate_search_data(&tc)?;

        let resp = client
            .post("http://localhost/v1/entries/search")
            .json(&serde_json::json!({"tags": "news"}))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<search_entries::SearchEntriesResponse>().await?;
        assert_eq!(body.count, 1);
        assert_eq!(body.entries[0].entry.title, "Breaking News Today");

        Ok(())
    }

    #[tokio::test]
    async fn test_search_tags_or() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        populate_search_data(&tc)?;

        let resp = client
            .post("http://localhost/v1/entries/search")
            .json(&serde_json::json!({"tags": {"or": ["news", "sports"]}}))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<search_entries::SearchEntriesResponse>().await?;
        assert_eq!(body.count, 2);
        let titles: Vec<&str> = body
            .entries
            .iter()
            .map(|e| e.entry.title.as_str())
            .collect();
        assert!(titles.contains(&"Breaking News Today"));
        assert!(titles.contains(&"Sports Update"));

        Ok(())
    }

    #[tokio::test]
    async fn test_search_tags_and() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        populate_search_data(&tc)?;

        // news AND tech — only entry 1 has both
        let resp = client
            .post("http://localhost/v1/entries/search")
            .json(&serde_json::json!({"tags": {"and": ["news", "tech"]}}))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<search_entries::SearchEntriesResponse>().await?;
        assert_eq!(body.count, 1);
        assert_eq!(body.entries[0].entry.title, "Breaking News Today");

        Ok(())
    }

    #[tokio::test]
    async fn test_search_tags_nested_and_or() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        populate_search_data(&tc)?;

        // (news OR science) AND (tech)
        // Entry 1 has news+tech ✓, Entry 3 has tech+science ✓
        let resp = client
            .post("http://localhost/v1/entries/search")
            .json(&serde_json::json!({
                "tags": {"and": [
                    {"or": ["news", "science"]},
                    "tech"
                ]}
            }))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<search_entries::SearchEntriesResponse>().await?;
        assert_eq!(body.count, 2);
        let titles: Vec<&str> = body
            .entries
            .iter()
            .map(|e| e.entry.title.as_str())
            .collect();
        assert!(titles.contains(&"Breaking News Today"));
        assert!(titles.contains(&"Tech Review"));

        Ok(())
    }

    #[tokio::test]
    async fn test_search_tags_not_single() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        populate_search_data(&tc)?;

        // NOT sports — entries 1 (news+tech), 2 (science), 3 (tech+science)
        let resp = client
            .post("http://localhost/v1/entries/search")
            .json(&serde_json::json!({"tags": {"not": "sports"}}))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<search_entries::SearchEntriesResponse>().await?;
        assert_eq!(body.count, 3);
        let titles: Vec<&str> = body
            .entries
            .iter()
            .map(|e| e.entry.title.as_str())
            .collect();
        assert!(titles.contains(&"Breaking News Today"));
        assert!(titles.contains(&"Science Discovery"));
        assert!(titles.contains(&"Tech Review"));
        assert!(!titles.contains(&"Sports Update"));

        Ok(())
    }

    #[tokio::test]
    async fn test_search_tags_and_with_not() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        populate_search_data(&tc)?;

        // tech AND NOT news — entry 1 has tech+news (excluded),
        // entry 3 has tech+science (included).
        let resp = client
            .post("http://localhost/v1/entries/search")
            .json(&serde_json::json!({
                "tags": {"and": ["tech", {"not": "news"}]}
            }))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<search_entries::SearchEntriesResponse>().await?;
        assert_eq!(body.count, 1);
        assert_eq!(body.entries[0].entry.title, "Tech Review");

        Ok(())
    }

    #[tokio::test]
    async fn test_search_tags_not_or() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        populate_search_data(&tc)?;

        // NOT (news OR science) — entry 4 only (sports)
        let resp = client
            .post("http://localhost/v1/entries/search")
            .json(&serde_json::json!({
                "tags": {"not": {"or": ["news", "science"]}}
            }))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<search_entries::SearchEntriesResponse>().await?;
        assert_eq!(body.count, 1);
        assert_eq!(body.entries[0].entry.title, "Sports Update");

        Ok(())
    }

    #[tokio::test]
    async fn test_search_tags_or_with_not() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        populate_search_data(&tc)?;

        // news OR NOT tech — entry 1 (news), entry 2 (science, no tech),
        // entry 4 (sports, no tech). Entry 3 has tech (excluded).
        let resp = client
            .post("http://localhost/v1/entries/search")
            .json(&serde_json::json!({
                "tags": {"or": ["news", {"not": "tech"}]}
            }))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<search_entries::SearchEntriesResponse>().await?;
        assert_eq!(body.count, 3);
        let titles: Vec<&str> = body
            .entries
            .iter()
            .map(|e| e.entry.title.as_str())
            .collect();
        assert!(titles.contains(&"Breaking News Today"));
        assert!(titles.contains(&"Science Discovery"));
        assert!(titles.contains(&"Sports Update"));
        assert!(!titles.contains(&"Tech Review"));

        Ok(())
    }

    #[tokio::test]
    async fn test_search_feed_id() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        populate_search_data(&tc)?;
        {
            let conn = tc.database_conn()?;
            conn.execute(
                "INSERT INTO feeds (id, title, url, syndication_format)
                 VALUES (2, 'Other Feed', 'http://example.com/other', 'rss')",
                [],
            )?;
            conn.execute("UPDATE entries SET feed_id = 2 WHERE id IN (2, 4)", [])?;
        }

        let resp = client
            .post("http://localhost/v1/entries/search")
            .json(&serde_json::json!({"feed_id": 2}))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<search_entries::SearchEntriesResponse>().await?;
        assert_eq!(body.count, 2);
        let titles: Vec<&str> = body
            .entries
            .iter()
            .map(|e| e.entry.title.as_str())
            .collect();
        assert_eq!(titles, ["Sports Update", "Science Discovery"]);

        // It combines with the other filters.
        let resp = client
            .post("http://localhost/v1/entries/search")
            .json(&serde_json::json!({"feed_id": 1, "tags": {"not": "news"}}))
            .send()
            .await?;
        let body = resp.json::<search_entries::SearchEntriesResponse>().await?;
        assert_eq!(body.count, 1);
        assert_eq!(body.entries[0].entry.title, "Tech Review");

        Ok(())
    }

    /// Entries published at the same time come out in descending ID order,
    /// so that paging through them neither skips nor repeats any.
    #[tokio::test]
    async fn test_search_orders_ties_by_id() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        populate_search_data(&tc)?;
        tc.database_conn()?
            .execute("UPDATE entries SET published_at = 1700000000", [])?;

        let resp = client
            .post("http://localhost/v1/entries/search")
            .json(&serde_json::json!({}))
            .send()
            .await?;
        let body = resp.json::<search_entries::SearchEntriesResponse>().await?;
        let ids: Vec<i64> = body.entries.iter().map(|e| e.entry.id).collect();
        assert_eq!(ids, [4, 3, 2, 1]);

        Ok(())
    }

    #[tokio::test]
    async fn test_search_date_range() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        populate_search_data(&tc)?;

        // Entries between Feb 1 and Mar 5
        let resp = client
            .post("http://localhost/v1/entries/search")
            .json(&serde_json::json!({
                "published_after": "2026-02-01T00:00:00Z",
                "published_before": "2026-03-05T00:00:00Z"
            }))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<search_entries::SearchEntriesResponse>().await?;
        assert_eq!(body.count, 2);
        let titles: Vec<&str> = body
            .entries
            .iter()
            .map(|e| e.entry.title.as_str())
            .collect();
        assert!(titles.contains(&"Science Discovery"));
        assert!(titles.contains(&"Tech Review"));

        Ok(())
    }

    #[tokio::test]
    async fn test_search_title_glob() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        populate_search_data(&tc)?;

        let resp = client
            .post("http://localhost/v1/entries/search")
            .json(&serde_json::json!({"title_glob": "*Review*"}))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<search_entries::SearchEntriesResponse>().await?;
        assert_eq!(body.count, 1);
        assert_eq!(body.entries[0].entry.title, "Tech Review");

        Ok(())
    }

    #[tokio::test]
    async fn test_search_content_glob() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        populate_search_data(&tc)?;

        let resp = client
            .post("http://localhost/v1/entries/search")
            .json(&serde_json::json!({"content_glob": "*physics*"}))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<search_entries::SearchEntriesResponse>().await?;
        assert_eq!(body.count, 1);
        assert_eq!(body.entries[0].entry.title, "Science Discovery");

        Ok(())
    }

    #[tokio::test]
    async fn test_search_url_glob() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        populate_search_data(&tc)?;

        let resp = client
            .post("http://localhost/v1/entries/search")
            .json(&serde_json::json!({"url_glob": "*tech*"}))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<search_entries::SearchEntriesResponse>().await?;
        assert_eq!(body.count, 1);
        assert_eq!(body.entries[0].entry.title, "Tech Review");

        Ok(())
    }

    #[tokio::test]
    async fn test_search_combined_filters() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        populate_search_data(&tc)?;

        // tech tag + after Feb + title contains "*ech*"
        let resp = client
            .post("http://localhost/v1/entries/search")
            .json(&serde_json::json!({
                "tags": "tech",
                "published_after": "2026-02-01T00:00:00Z",
                "title_glob": "*ech*"
            }))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<search_entries::SearchEntriesResponse>().await?;
        assert_eq!(body.count, 1);
        assert_eq!(body.entries[0].entry.title, "Tech Review");

        Ok(())
    }

    #[tokio::test]
    async fn test_search_invalid_date() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;

        let resp = client
            .post("http://localhost/v1/entries/search")
            .json(&serde_json::json!({"published_after": "not-a-date"}))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        Ok(())
    }

    #[tokio::test]
    async fn test_search_malformed_json() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;

        let resp = client
            .post("http://localhost/v1/entries/search")
            .header("content-type", "application/json")
            .body("{invalid json")
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        Ok(())
    }

    #[tokio::test]
    async fn test_search_pagination() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        populate_search_data(&tc)?;

        // Limit to 2 entries (page 1)
        let resp = client
            .post("http://localhost/v1/entries/search")
            .json(&serde_json::json!({"limit": 2, "offset": 0}))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let page1 = resp.json::<search_entries::SearchEntriesResponse>().await?;
        assert_eq!(page1.count, 4); // total count is still 4
        assert_eq!(page1.entries.len(), 2);
        assert_eq!(page1.limit, 2);
        assert_eq!(page1.offset, 0);

        // Get next page (page 2)
        let resp = client
            .post("http://localhost/v1/entries/search")
            .json(&serde_json::json!({"limit": 2, "offset": 2}))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let page2 = resp.json::<search_entries::SearchEntriesResponse>().await?;
        assert_eq!(page2.count, 4);
        assert_eq!(page2.entries.len(), 2);
        assert_eq!(page2.offset, 2);

        // Pages must contain different entries
        let page1_ids: Vec<i64> = page1.entries.iter().map(|e| e.entry.id).collect();
        let page2_ids: Vec<i64> = page2.entries.iter().map(|e| e.entry.id).collect();
        for id in &page2_ids {
            assert!(
                !page1_ids.contains(id),
                "entry {} appears on both pages",
                id
            );
        }

        Ok(())
    }

    // ── FTS5 + REGEXP tests (search feature only) ─────────────────────

    #[tokio::test]
    async fn test_search_fts5_query() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        populate_search_data(&tc)?;

        // "gadgets" appears in entry 3's content
        let resp = client
            .post("http://localhost/v1/entries/search")
            .json(&serde_json::json!({"query": "gadgets"}))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<search_entries::SearchEntriesResponse>().await?;
        assert_eq!(body.count, 1);
        assert_eq!(body.entries[0].entry.title, "Tech Review");
        assert!(body.entries[0].rank.is_some());

        Ok(())
    }

    #[tokio::test]
    async fn test_search_fts5_prefix_query() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        populate_search_data(&tc)?;

        // Prefix query: "break*" should match "Breaking News Today" and
        // "Science Discovery" (content contains "breakthrough")
        let resp = client
            .post("http://localhost/v1/entries/search")
            .json(&serde_json::json!({"query": "break*"}))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<search_entries::SearchEntriesResponse>().await?;
        assert_eq!(body.count, 2);
        let titles: Vec<&str> = body
            .entries
            .iter()
            .map(|e| e.entry.title.as_str())
            .collect();
        assert!(titles.contains(&"Breaking News Today"));
        assert!(titles.contains(&"Science Discovery"));

        Ok(())
    }

    #[tokio::test]
    async fn test_search_fts5_relevance_sort() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        populate_search_data(&tc)?;

        // "news" appears in entry 1 title+content and entry 2 is unrelated.
        // With sort=relevance we just check that the endpoint works and returns ranked results.
        let resp = client
            .post("http://localhost/v1/entries/search")
            .json(&serde_json::json!({"query": "news", "sort": "relevance"}))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<search_entries::SearchEntriesResponse>().await?;
        assert!(body.count >= 1);
        // All results should have a rank
        for entry in &body.entries {
            assert!(entry.rank.is_some());
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_search_title_regex() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        populate_search_data(&tc)?;

        // Regex: titles starting with "Break" or "Tech"
        let resp = client
            .post("http://localhost/v1/entries/search")
            .json(&serde_json::json!({"title_regex": "^(Break|Tech)"}))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<search_entries::SearchEntriesResponse>().await?;
        assert_eq!(body.count, 2);
        let titles: Vec<&str> = body
            .entries
            .iter()
            .map(|e| e.entry.title.as_str())
            .collect();
        assert!(titles.contains(&"Breaking News Today"));
        assert!(titles.contains(&"Tech Review"));

        Ok(())
    }

    #[tokio::test]
    async fn test_search_content_regex() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        populate_search_data(&tc)?;

        // Regex on content: "break.*physics"
        let resp = client
            .post("http://localhost/v1/entries/search")
            .json(&serde_json::json!({"content_regex": "break.*physics"}))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<search_entries::SearchEntriesResponse>().await?;
        assert_eq!(body.count, 1);
        assert_eq!(body.entries[0].entry.title, "Science Discovery");

        Ok(())
    }

    #[tokio::test]
    async fn test_search_url_regex() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        populate_search_data(&tc)?;

        // Regex on URL: ends with "science" or "sports"
        let resp = client
            .post("http://localhost/v1/entries/search")
            .json(&serde_json::json!({"url_regex": "(science|sports)$"}))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<search_entries::SearchEntriesResponse>().await?;
        assert_eq!(body.count, 2);
        let titles: Vec<&str> = body
            .entries
            .iter()
            .map(|e| e.entry.title.as_str())
            .collect();
        assert!(titles.contains(&"Science Discovery"));
        assert!(titles.contains(&"Sports Update"));

        Ok(())
    }

    #[tokio::test]
    async fn test_search_fts5_combined_with_regex() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        populate_search_data(&tc)?;

        // FTS5 narrows to entries with "news", regex further filters title
        let resp = client
            .post("http://localhost/v1/entries/search")
            .json(&serde_json::json!({
                "query": "news",
                "title_regex": "^Breaking"
            }))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<search_entries::SearchEntriesResponse>().await?;
        assert_eq!(body.count, 1);
        assert_eq!(body.entries[0].entry.title, "Breaking News Today");

        Ok(())
    }

    #[tokio::test]
    async fn test_search_invalid_regex() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;

        let resp = client
            .post("http://localhost/v1/entries/search")
            .json(&serde_json::json!({"title_regex": "[invalid"}))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        Ok(())
    }

    #[tokio::test]
    async fn test_search_invalid_sort() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;

        let resp = client
            .post("http://localhost/v1/entries/search")
            .json(&serde_json::json!({"sort": "bogus"}))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        Ok(())
    }

    #[tokio::test]
    async fn test_search_relevance_without_query() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;

        let resp = client
            .post("http://localhost/v1/entries/search")
            .json(&serde_json::json!({"sort": "relevance"}))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        Ok(())
    }

    #[tokio::test]
    async fn test_search_fts5_with_existing_filters() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        populate_search_data(&tc)?;

        // Combine FTS5 with tag, date, and GLOB filters
        let resp = client
            .post("http://localhost/v1/entries/search")
            .json(&serde_json::json!({
                "query": "tech",
                "tags": "tech",
                "published_after": "2026-02-01T00:00:00Z",
                "title_glob": "*Review*"
            }))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<search_entries::SearchEntriesResponse>().await?;
        assert_eq!(body.count, 1);
        assert_eq!(body.entries[0].entry.title, "Tech Review");

        Ok(())
    }

    /// Helper to populate sync test data: feed 1 with entries 1, 2 and 3,
    /// published in the order 2, 3, 1 and ingested in the order 1, 2, 3.
    /// Entry 2 is read and entry 3 has the user tag "news" (ID 4).
    fn populate_sync_entries(tc: &TestConfig) -> Result<()> {
        let conn = tc.database_conn()?;
        conn.execute(
            "INSERT INTO feeds (title, url, syndication_format)
             VALUES ('Feed', 'http://example.com/feed', 'rss')",
            [],
        )?;
        for (id, published_at) in [(1, 1700000300i64), (2, 1700000100), (3, 1700000200)] {
            conn.execute(
                "INSERT INTO entries
                    (id, feed_id, syndication_format, guid, published_at, title, url, ingested_at)
                 VALUES (?1, 1, 'rss', ?1, ?2, 'Entry ' || ?1, 'http://example.com/', ?3)",
                rusqlite::params![id, published_at, 1800000000 + id],
            )?;
        }
        conn.execute("INSERT INTO tags (name) VALUES ('news')", [])?;
        conn.execute(
            "INSERT INTO entry_tags (entry_id, tag_id) VALUES (2, ?1), (3, 4)",
            [SystemTag::Read.id(&conn)?],
        )?;
        Ok(())
    }

    fn ids(entries: &[ListEntriesResponseEntry]) -> Vec<i64> {
        entries.iter().map(|e| e.id).collect()
    }

    fn tag_names(entry: &ListEntriesResponseEntry) -> Vec<&str> {
        entry.tags.iter().map(|t| t.name.as_str()).collect()
    }

    /// Listed entries carry every tag applied to them, and when Kiki
    /// stored them.
    #[tokio::test]
    async fn test_list_entries_include_tags_and_ingested_at() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        populate_sync_entries(&tc)?;

        let resp = client.get("http://localhost/v1/entries").send().await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<ListEntriesResponse>().await?;
        assert_eq!(ids(&body.entries), [1, 3, 2]);
        assert_eq!(tag_names(&body.entries[0]), Vec::<&str>::new());
        assert_eq!(tag_names(&body.entries[1]), ["news"]);
        assert_eq!(body.entries[1].tags[0].kind, TagKind::User);
        assert_eq!(tag_names(&body.entries[2]), ["system:read"]);
        assert_eq!(body.entries[2].tags[0].kind, TagKind::System);
        assert_eq!(
            body.entries[0].ingested_at.as_deref(),
            Some("2027-01-15T08:00:01+00:00")
        );

        let resp = client
            .get("http://localhost/v1/entries/id/2")
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let entry = resp.json::<get_entry::GetEntryResponse>().await?;
        let names: Vec<&str> = entry.tags.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["system:read"]);
        assert_eq!(
            entry.ingested_at.as_deref(),
            Some("2027-01-15T08:00:02+00:00")
        );
        Ok(())
    }

    /// `sort`, `since_id` and `max_id` page through entries by ID, in
    /// either direction, on every entry listing.
    #[tokio::test]
    async fn test_list_entries_sync_by_id() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        populate_sync_entries(&tc)?;

        let list = |query: &'static str| {
            let client = client.clone();
            async move {
                let resp = client
                    .get(format!("http://localhost{query}"))
                    .send()
                    .await?;
                assert_eq!(resp.status(), StatusCode::OK, "{query}");
                let body = resp.json::<ListEntriesResponse>().await?;
                anyhow::Ok((ids(&body.entries), body.count))
            }
        };

        assert_eq!(list("/v1/entries?sort=id").await?, (vec![1, 2, 3], 3));
        assert_eq!(
            list("/v1/entries?sort=id&since_id=1&limit=1").await?,
            (vec![2], 2)
        );
        assert_eq!(
            list("/v1/entries?sort=id_desc&max_id=3").await?,
            (vec![2, 1], 2)
        );
        assert_eq!(list("/v1/entries?since_id=1&max_id=3").await?, (vec![2], 1));
        assert_eq!(
            list("/v1/feeds/id/1/entries?sort=id&since_id=1").await?,
            (vec![2, 3], 2)
        );
        assert_eq!(
            list("/v1/feeds/id/1/entries?sort=id_desc").await?,
            (vec![3, 2, 1], 3)
        );

        let resp = client
            .get("http://localhost/v1/entries?sort=newest")
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        Ok(())
    }

    /// `POST /batch` returns the entries asked for that exist, in the
    /// order asked for, each once, with their tags.
    #[tokio::test]
    async fn test_batch_entries() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        populate_sync_entries(&tc)?;

        let resp = client
            .post("http://localhost/v1/entries/batch")
            .json(&serde_json::json!({"ids": [3, 999, 2, 3]}))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<batch_entries::BatchEntriesResponse>().await?;
        assert_eq!(ids(&body.entries), [3, 2]);
        assert_eq!(tag_names(&body.entries[0]), ["news"]);
        assert_eq!(tag_names(&body.entries[1]), ["system:read"]);

        let resp = client
            .post("http://localhost/v1/entries/batch")
            .json(&serde_json::json!({"ids": []}))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<batch_entries::BatchEntriesResponse>().await?;
        assert!(body.entries.is_empty());

        let too_many: Vec<i64> = (1..=batch_entries::MAX_BATCH_IDS as i64 + 1).collect();
        let resp = client
            .post("http://localhost/v1/entries/batch")
            .json(&serde_json::json!({ "ids": too_many }))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        Ok(())
    }

    /// `POST /search/ids` takes the same filters as the search, including
    /// the ID and ingestion time ranges, and returns just the IDs.
    #[tokio::test]
    async fn test_search_entry_ids() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        populate_sync_entries(&tc)?;

        let search = |body: serde_json::Value| {
            let client = client.clone();
            async move {
                let resp = client
                    .post("http://localhost/v1/entries/search/ids")
                    .json(&body)
                    .send()
                    .await?;
                assert_eq!(resp.status(), StatusCode::OK, "{body}");
                anyhow::Ok(
                    resp.json::<search_entries::SearchEntryIdsResponse>()
                        .await?,
                )
            }
        };

        let unread = search(serde_json::json!({
            "tags": {"not": "system:read"},
            "sort": "id"
        }))
        .await?;
        assert_eq!(unread.ids, [1, 3]);
        assert_eq!(unread.count, 2);
        assert_eq!(unread.limit, search_entries::DEFAULT_ID_LIMIT);

        let body = search(serde_json::json!({"since_id": 1, "sort": "id"})).await?;
        assert_eq!(body.ids, [2, 3]);

        let body = search(serde_json::json!({
            "ingested_after": "2027-01-15T08:00:02Z",
            "ingested_before": "2027-01-15T08:00:04Z"
        }))
        .await?;
        assert_eq!(body.ids, [3, 2]);

        let body = search(serde_json::json!({"limit": 1_000_000, "offset": 1})).await?;
        assert_eq!(body.ids, [3, 2]);
        assert_eq!(body.count, 3);
        assert_eq!(body.limit, search_entries::MAX_ID_LIMIT);

        let resp = client
            .post("http://localhost/v1/entries/search/ids")
            .json(&serde_json::json!({"sort": "relevance"}))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        Ok(())
    }

    /// Full search results carry tags too, and sort by ID on request.
    #[tokio::test]
    async fn test_search_entries_sync_by_id() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        populate_sync_entries(&tc)?;

        let resp = client
            .post("http://localhost/v1/entries/search")
            .json(&serde_json::json!({"sort": "id_desc", "max_id": 3}))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<search_entries::SearchEntriesResponse>().await?;
        let found: Vec<i64> = body.entries.iter().map(|e| e.entry.id).collect();
        assert_eq!(found, [2, 1]);
        assert_eq!(tag_names(&body.entries[0].entry), ["system:read"]);
        Ok(())
    }
}
