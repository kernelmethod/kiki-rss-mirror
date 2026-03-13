pub mod cleanup;
pub mod delete_entry;
pub mod get_entry;
pub mod list_entries;

use cleanup::cleanup;
use delete_entry::delete_entry;
use get_entry::get_entry;
#[allow(unused_imports)]
pub use list_entries::{list_entries, ListEntriesResponse, ListEntriesResponseEntry};

use crate::routes::v1::tags::{get_entry_tags, set_entry_tags};
use crate::server::AppState;
use axum::{
    routing::{get, post},
    Router,
};

pub fn create_router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_entries))
        .route("/cleanup", post(cleanup))
        .route("/id/{id}", get(get_entry).delete(delete_entry))
        .route("/id/{id}/tags", get(get_entry_tags).put(set_entry_tags))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod test {
    use super::*;
    use crate::test::{TestBuilder, TestConfig};
    use anyhow::Result;
    use axum::http::StatusCode;
    use chrono::{TimeZone, Utc};

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

        // Validate the publication dates that are returned
        assert_eq!(
            response.entries[0].published_at.as_deref(),
            Some("2026-05-15T00:00:00+00:00")
        );
        assert_eq!(
            response.entries[1].published_at.as_deref(),
            Some("2026-05-16T00:00:00+00:00")
        );

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
        assert_eq!(response.feed_id, 1);
        assert_eq!(response.syndication_format, "rss");
        assert_eq!(response.guid, "rss-guid-1");
        assert_eq!(response.title, "RSS Entry");
        assert_eq!(response.url, "http://example.com/rss-entry");
        assert_eq!(response.content.as_deref(), Some("RSS Content"));
        assert_eq!(response.status_read, 0);
        assert_eq!(response.status_favorite, 0);

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
}
