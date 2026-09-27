use crate::routes::v1::entries::ListEntriesResponseEntry;
use crate::server::AppState;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::task;
use tracing::{event, Level};

const DEFAULT_LIMIT: usize = 50;
/// Largest page size a client may request; larger values are clamped to this.
const MAX_LIMIT: usize = 200;

#[derive(Deserialize, utoipa::IntoParams)]
pub struct FeedEntriesQueryParams {
    /// Number of records to skip (default: 0).
    pub offset: Option<usize>,
    /// Maximum number of records to return (default: 50, max: 200). Larger
    /// values are clamped to 200; the response reports the limit that was applied.
    pub limit: Option<usize>,
}

#[derive(Deserialize, Serialize, utoipa::ToSchema)]
pub struct FeedEntriesResponse {
    pub entries: Vec<ListEntriesResponseEntry>,
    pub count: usize,
    pub offset: usize,
    pub limit: usize,
}

#[derive(Error, Debug)]
enum FeedEntriesTaskError {
    #[error("feed not found")]
    FeedNotFound,

    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
}

/// List entries
///
/// List all of the entries belonging to a specific feed, newest first. Entries
/// with the same publication time are ordered by descending ID.
#[utoipa::path(
    get,
    path = "/v1/feeds/id/{id}/entries",
    params(
        ("id" = i64, Path, description = "Feed ID"),
        FeedEntriesQueryParams,
    ),
    responses(
        (status = 200, description = "List of entries for the feed", body = FeedEntriesResponse),
        (status = 404, description = "Feed not found"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "feeds"
)]
#[axum::debug_handler]
pub async fn feed_entries(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(params): Query<FeedEntriesQueryParams>,
) -> Result<Response, Response> {
    let conn = state.conn_pool.get().map_err(|e| {
        event!(Level::ERROR, "failed to get database connection: {:?}", e);
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
    })?;
    let offset = params.offset.unwrap_or(0);
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);

    let result = task::spawn_blocking(move || {
        // Check if feed exists
        let exists: bool = conn
            .prepare("SELECT EXISTS(SELECT 1 FROM feeds WHERE id = ?1)")
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?
            .query_row([id], |row| row.get(0))?;

        if !exists {
            return Err(FeedEntriesTaskError::FeedNotFound);
        }

        let count: usize = conn
            .prepare("SELECT COUNT(*) FROM entries WHERE feed_id = ?1")
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?
            .query_row([id], |row| row.get(0))?;

        let entries = conn
            .prepare(
                "SELECT id, feed_id, source_id, syndication_format,
                        guid, published_at, title, url, content
                 FROM entries WHERE feed_id = ?1
                 ORDER BY published_at DESC, id DESC
                 LIMIT ?2 OFFSET ?3",
            )
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?
            .query_map(rusqlite::params![id, limit, offset], |row| {
                Ok(ListEntriesResponseEntry {
                    id: row.get(0)?,
                    feed_id: row.get(1)?,
                    source_id: row.get(2)?,
                    syndication_format: row.get(3)?,
                    guid: row.get(4)?,
                    published_at: chrono::DateTime::from_timestamp_secs(row.get(5)?)
                        .map(|d| d.to_rfc3339()),
                    title: row.get(6)?,
                    url: row.get(7)?,
                    content: row.get(8)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok::<FeedEntriesResponse, FeedEntriesTaskError>(FeedEntriesResponse {
            entries,
            count,
            offset,
            limit,
        })
    })
    .await
    .inspect_err(|e| {
        event!(Level::ERROR, "task error in feed_entries: {:?}", e);
    });

    match result {
        Ok(Ok(response)) => Ok(Json(response).into_response()),
        Ok(Err(FeedEntriesTaskError::FeedNotFound)) => {
            Err((StatusCode::NOT_FOUND, "Feed not found").into_response())
        }
        Ok(Err(_)) => {
            Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response())
        }
        Err(_) => Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod test {
    use super::*;
    use crate::test::{TestBuilder, TestConfig};
    use anyhow::Result;
    use std::collections::HashSet;

    fn insert_feed(tc: &TestConfig, title: &str) -> Result<i64> {
        let conn = tc.database_conn()?;
        conn.execute(
            "INSERT INTO feeds (title, url, syndication_format) VALUES (?1, ?2, 'rss')",
            [title, &format!("https://example.com/{title}.xml")],
        )?;
        Ok(conn.last_insert_rowid())
    }

    fn insert_entry(
        tc: &TestConfig,
        feed_id: Option<i64>,
        guid: &str,
        published_at: i64,
        content: Option<&str>,
    ) -> Result<i64> {
        let conn = tc.database_conn()?;
        conn.execute(
            "INSERT INTO entries
                (feed_id, syndication_format, guid, published_at, title, url, content)
             VALUES (?1, 'rss', ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                feed_id,
                guid,
                published_at,
                format!("title {guid}"),
                format!("https://example.com/{guid}"),
                content,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Insert `n` entries into a feed and return their IDs.
    fn insert_entries(tc: &TestConfig, feed_id: i64, n: usize) -> Result<Vec<i64>> {
        (0..n)
            .map(|i| {
                insert_entry(
                    tc,
                    Some(feed_id),
                    &format!("feed{feed_id}-guid-{i:03}"),
                    1_700_000_000 + i as i64,
                    None,
                )
            })
            .collect()
    }

    async fn get_entries(
        client: &reqwest::Client,
        feed_id: impl std::fmt::Display,
        query: &str,
    ) -> Result<reqwest::Response> {
        Ok(client
            .get(format!(
                "http://localhost/v1/feeds/id/{feed_id}/entries{query}"
            ))
            .send()
            .await?)
    }

    fn ids(resp: &FeedEntriesResponse) -> HashSet<i64> {
        resp.entries.iter().map(|e| e.id).collect()
    }

    #[tokio::test]
    async fn test_feed_entries_empty_feed() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        let feed_id = insert_feed(&tc, "empty")?;

        let resp = get_entries(&client, feed_id, "").await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<FeedEntriesResponse>().await?;
        assert!(body.entries.is_empty());
        assert_eq!(body.count, 0);
        assert_eq!(body.offset, 0);
        assert_eq!(body.limit, DEFAULT_LIMIT);

        Ok(())
    }

    #[tokio::test]
    async fn test_feed_entries_not_found() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;

        for query in ["", "?offset=0&limit=10"] {
            let resp = get_entries(&client, 12345, query).await?;
            assert_eq!(resp.status(), StatusCode::NOT_FOUND);
            assert_eq!(resp.text().await?, "Feed not found");
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_feed_entries_response_fields() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        let feed_id = insert_feed(&tc, "fields")?;
        let with_content = insert_entry(
            &tc,
            Some(feed_id),
            "with-content",
            1_700_000_000,
            Some("<p>hi</p>"),
        )?;
        let without_content = insert_entry(&tc, Some(feed_id), "no-content", 0, None)?;

        let resp = get_entries(&client, feed_id, "").await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<FeedEntriesResponse>().await?;
        assert_eq!(body.count, 2);
        assert_eq!(body.entries.len(), 2);

        let entry = body
            .entries
            .iter()
            .find(|e| e.id == with_content)
            .expect("entry with content should be listed");
        assert_eq!(entry.feed_id, Some(feed_id));
        assert_eq!(entry.source_id, None);
        assert_eq!(entry.syndication_format, "rss");
        assert_eq!(entry.guid, "with-content");
        assert_eq!(
            entry.published_at.as_deref(),
            Some("2023-11-14T22:13:20+00:00")
        );
        assert_eq!(entry.title, "title with-content");
        assert_eq!(entry.url, "https://example.com/with-content");
        assert_eq!(entry.content.as_deref(), Some("<p>hi</p>"));

        let entry = body
            .entries
            .iter()
            .find(|e| e.id == without_content)
            .expect("entry without content should be listed");
        assert_eq!(
            entry.published_at.as_deref(),
            Some("1970-01-01T00:00:00+00:00")
        );
        assert_eq!(entry.content, None);

        Ok(())
    }

    #[tokio::test]
    async fn test_feed_entries_only_returns_entries_for_feed() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        let feed_a = insert_feed(&tc, "a")?;
        let feed_b = insert_feed(&tc, "b")?;
        let empty = insert_feed(&tc, "empty")?;
        let a_ids: HashSet<i64> = insert_entries(&tc, feed_a, 3)?.into_iter().collect();
        let b_ids: HashSet<i64> = insert_entries(&tc, feed_b, 5)?.into_iter().collect();
        // Orphaned entries (e.g. from a feed deleted with delete_entries=false)
        // must not leak into any feed's listing.
        insert_entry(&tc, None, "orphan", 1_700_000_000, None)?;

        let resp = get_entries(&client, feed_a, "").await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<FeedEntriesResponse>().await?;
        assert_eq!(body.count, 3);
        assert_eq!(ids(&body), a_ids);
        assert!(body.entries.iter().all(|e| e.feed_id == Some(feed_a)));

        let resp = get_entries(&client, feed_b, "").await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<FeedEntriesResponse>().await?;
        assert_eq!(body.count, 5);
        assert_eq!(ids(&body), b_ids);

        let resp = get_entries(&client, empty, "").await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<FeedEntriesResponse>().await?;
        assert_eq!(body.count, 0);
        assert!(body.entries.is_empty());

        Ok(())
    }

    #[tokio::test]
    async fn test_feed_entries_ordering() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        let feed_id = insert_feed(&tc, "ordered")?;
        let other = insert_feed(&tc, "other")?;

        // Insert out of chronological order, with two entries sharing a
        // timestamp and an entry from another feed in between.
        let middle = insert_entry(&tc, Some(feed_id), "middle", 2_000, None)?;
        let oldest = insert_entry(&tc, Some(feed_id), "oldest", 1_000, None)?;
        let tie_first = insert_entry(&tc, Some(feed_id), "tie-first", 3_000, None)?;
        insert_entry(&tc, Some(other), "other", 2_500, None)?;
        let tie_second = insert_entry(&tc, Some(feed_id), "tie-second", 3_000, None)?;
        let newest = insert_entry(&tc, Some(feed_id), "newest", 4_000, None)?;

        // Newest first, with ties broken by descending ID.
        let expected = vec![newest, tie_second, tie_first, middle, oldest];

        let resp = get_entries(&client, feed_id, "").await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<FeedEntriesResponse>().await?;
        let ids: Vec<i64> = body.entries.iter().map(|e| e.id).collect();
        assert_eq!(ids, expected);

        // Paging through the feed yields the same order, including across the
        // tie at a page boundary.
        let mut paged = Vec::new();
        for offset in (0..expected.len()).step_by(2) {
            let resp = get_entries(&client, feed_id, &format!("?offset={offset}&limit=2")).await?;
            assert_eq!(resp.status(), StatusCode::OK);
            let body = resp.json::<FeedEntriesResponse>().await?;
            paged.extend(body.entries.iter().map(|e| e.id));
        }
        assert_eq!(paged, expected);

        Ok(())
    }

    #[tokio::test]
    async fn test_feed_entries_default_limit() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        let feed_id = insert_feed(&tc, "big")?;
        insert_entries(&tc, feed_id, DEFAULT_LIMIT + 5)?;

        let resp = get_entries(&client, feed_id, "").await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<FeedEntriesResponse>().await?;
        assert_eq!(body.entries.len(), DEFAULT_LIMIT);
        // `count` is the total for the feed, not the size of this page.
        assert_eq!(body.count, DEFAULT_LIMIT + 5);
        assert_eq!(body.offset, 0);
        assert_eq!(body.limit, DEFAULT_LIMIT);

        Ok(())
    }

    #[tokio::test]
    async fn test_feed_entries_pagination() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        let feed_id = insert_feed(&tc, "paged")?;
        let other = insert_feed(&tc, "other")?;
        let all_ids: HashSet<i64> = insert_entries(&tc, feed_id, 10)?.into_iter().collect();
        insert_entries(&tc, other, 4)?;

        // Walking the feed in pages of 3 visits every entry exactly once.
        let mut seen = HashSet::new();
        for (offset, expected_len) in [(0, 3), (3, 3), (6, 3), (9, 1)] {
            let resp = get_entries(&client, feed_id, &format!("?offset={offset}&limit=3")).await?;
            assert_eq!(resp.status(), StatusCode::OK);
            let body = resp.json::<FeedEntriesResponse>().await?;
            assert_eq!(body.entries.len(), expected_len, "offset {offset}");
            assert_eq!(body.count, 10);
            assert_eq!(body.offset, offset);
            assert_eq!(body.limit, 3);
            for id in ids(&body) {
                assert!(seen.insert(id), "entry {id} returned on more than one page");
            }
        }
        assert_eq!(seen, all_ids);

        // Offset alone uses the default limit.
        let resp = get_entries(&client, feed_id, "?offset=8").await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<FeedEntriesResponse>().await?;
        assert_eq!(body.entries.len(), 2);
        assert_eq!(body.offset, 8);
        assert_eq!(body.limit, DEFAULT_LIMIT);

        // Limit alone starts from the beginning.
        let resp = get_entries(&client, feed_id, "?limit=4").await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<FeedEntriesResponse>().await?;
        assert_eq!(body.entries.len(), 4);
        assert_eq!(body.offset, 0);
        assert_eq!(body.limit, 4);

        Ok(())
    }

    #[tokio::test]
    async fn test_feed_entries_pagination_edge_cases() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        let feed_id = insert_feed(&tc, "edges")?;
        let all_ids: HashSet<i64> = insert_entries(&tc, feed_id, 5)?.into_iter().collect();

        // limit=0 returns no entries but still reports the total.
        let resp = get_entries(&client, feed_id, "?limit=0").await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<FeedEntriesResponse>().await?;
        assert!(body.entries.is_empty());
        assert_eq!(body.count, 5);
        assert_eq!(body.limit, 0);

        // An offset at or past the end yields an empty page, not an error.
        for offset in [5, 6, 1000] {
            let resp = get_entries(&client, feed_id, &format!("?offset={offset}")).await?;
            assert_eq!(resp.status(), StatusCode::OK);
            let body = resp.json::<FeedEntriesResponse>().await?;
            assert!(body.entries.is_empty(), "offset {offset}");
            assert_eq!(body.count, 5);
            assert_eq!(body.offset, offset);
        }

        // A limit larger than the feed returns everything.
        let resp = get_entries(&client, feed_id, "?limit=100").await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<FeedEntriesResponse>().await?;
        assert_eq!(ids(&body), all_ids);
        assert_eq!(body.limit, 100);

        // A page that straddles the end is truncated.
        let resp = get_entries(&client, feed_id, "?offset=3&limit=10").await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<FeedEntriesResponse>().await?;
        assert_eq!(body.entries.len(), 2);

        Ok(())
    }

    #[tokio::test]
    async fn test_feed_entries_limit_is_capped() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        let feed_id = insert_feed(&tc, "capped")?;
        insert_entries(&tc, feed_id, MAX_LIMIT + 10)?;

        // The cap itself is honoured exactly.
        let resp = get_entries(&client, feed_id, &format!("?limit={MAX_LIMIT}")).await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<FeedEntriesResponse>().await?;
        assert_eq!(body.entries.len(), MAX_LIMIT);
        assert_eq!(body.limit, MAX_LIMIT);

        // Anything larger is clamped, including values that don't fit in an
        // SQLite integer.
        for limit in [MAX_LIMIT + 1, 1_000_000, usize::MAX] {
            let resp = get_entries(&client, feed_id, &format!("?limit={limit}")).await?;
            assert_eq!(resp.status(), StatusCode::OK, "limit {limit}");
            let body = resp.json::<FeedEntriesResponse>().await?;
            assert_eq!(body.entries.len(), MAX_LIMIT, "limit {limit}");
            assert_eq!(body.limit, MAX_LIMIT, "limit {limit}");
            assert_eq!(body.count, MAX_LIMIT + 10);
        }

        // The remainder is reachable by paging past the cap.
        let resp = get_entries(
            &client,
            feed_id,
            &format!("?offset={MAX_LIMIT}&limit={}", usize::MAX),
        )
        .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<FeedEntriesResponse>().await?;
        assert_eq!(body.entries.len(), 10);

        Ok(())
    }

    #[tokio::test]
    async fn test_feed_entries_malformed_requests() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        let feed_id = insert_feed(&tc, "malformed")?;
        insert_entries(&tc, feed_id, 2)?;

        let resp = get_entries(&client, "not-a-number", "").await?;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        for query in [
            "?offset=-1",
            "?limit=-1",
            "?offset=abc",
            "?limit=abc",
            "?limit=1.5",
            "?offset=",
        ] {
            let resp = get_entries(&client, feed_id, query).await?;
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "query {query}");
        }

        // Unknown query parameters are ignored.
        let resp = get_entries(&client, feed_id, "?foo=bar").await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<FeedEntriesResponse>().await?;
        assert_eq!(body.count, 2);

        Ok(())
    }

    #[tokio::test]
    async fn test_feed_entries_after_feed_deleted() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        let feed_id = insert_feed(&tc, "doomed")?;
        insert_entries(&tc, feed_id, 3)?;

        let resp = client
            .delete(format!(
                "http://localhost/v1/feeds/id/{feed_id}?delete_entries=false"
            ))
            .send()
            .await?;
        assert!(resp.status().is_success());

        // The entries survive, but the feed no longer exists.
        let resp = get_entries(&client, feed_id, "").await?;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        Ok(())
    }

    #[tokio::test]
    async fn test_feed_entries_from_fetched_feed() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;
        let feed_id = tc
            .add_feed_from_url("example", tc.example_feed_url())
            .await?;

        let resp = get_entries(&client, feed_id, "").await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<FeedEntriesResponse>().await?;
        assert!(body.count > 0, "fetched feed should have entries");
        assert_eq!(body.entries.len(), body.count.min(DEFAULT_LIMIT));
        assert!(body.entries.iter().all(|e| e.feed_id == Some(feed_id)));
        assert!(body.entries.iter().all(|e| e.published_at.is_some()));

        // The per-feed count agrees with the database.
        let conn = tc.database_conn()?;
        let db_count: usize = conn.query_row(
            "SELECT COUNT(*) FROM entries WHERE feed_id = ?1",
            [feed_id],
            |row| row.get(0),
        )?;
        assert_eq!(body.count, db_count);

        Ok(())
    }
}
