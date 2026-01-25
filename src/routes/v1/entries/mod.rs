mod list_entries;

use list_entries::list_entries;

use crate::server::AppState;
use axum::{routing::get, Router};

pub fn create_router() -> Router<AppState> {
    Router::new().route("/", get(list_entries))
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::test::TestBuilder;
    use anyhow::Result;

    #[tokio::test]
    async fn test_list_entries() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;

        // Insert some test entries
        let conn = tc.database_conn().unwrap();
        println!("tc.database_path = {:?}", tc.database_path());

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
            (feed_id, "rss", "rss-guid-1", "2023-01-01T00:00:00Z", "RSS Entry", "http://example.com/rss-entry", "RSS Content"),
        )?;

        // Insert Atom entry
        conn.execute(
            "INSERT INTO entries (feed_id, syndication_format, guid,
                published_at, title, url, content)
            VALUES (?, ?, ?, ?, ?, ?, ?)",
            (feed_id, "atom", "atom-guid-1", "2023-01-02T00:00:00Z", "Atom Entry", "http://example.com/atom-entry", "Atom Content"),
        )?;
        println!("here!");

        // Call the list_entries endpoint
        let response = client
            .get("http://kiki/v1/entries")
            .send()
            .await?
            .json::<list_entries::ListEntriesResponse>()
            .await?;

        // Verify the response
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

        Ok(())
    }

}
