#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use crate::test::TestBuilder;
use anyhow::Result;
use serde_json::Value;
use std::collections::HashSet;
use std::time::Duration;
use tokio::task::JoinSet;

const BASE: &str = "http://localhost";
const TIMEOUT: Duration = Duration::from_secs(30);

/// Insert N feeds pointing at `feed_url`, with `last_checked = NULL`.
/// Returns the vector of inserted feed IDs.
fn populate_n_feeds(conn: &rusqlite::Connection, n: usize, feed_url: &str) -> Vec<i64> {
    let mut ids = Vec::with_capacity(n);
    for i in 0..n {
        conn.execute(
            "INSERT INTO feeds (title, url) VALUES (?1, ?2)",
            rusqlite::params![format!("feed-{i}"), feed_url],
        )
        .unwrap();
        ids.push(conn.last_insert_rowid());
    }
    ids
}

/// Poll `GET /v1/entries` until the entry count stabilizes or timeout.
async fn wait_for_entries_stable(client: &reqwest::Client, timeout: Duration) {
    let start = std::time::Instant::now();
    let mut last_count: i64 = -1;
    let mut stable_ticks = 0u32;
    while start.elapsed() < timeout {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let resp = client
            .get(format!("{BASE}/v1/entries?limit=1"))
            .send()
            .await;
        if let Ok(resp) = resp {
            if let Ok(body) = resp.json::<Value>().await {
                let count = body["count"].as_i64().unwrap_or(0);
                if count == last_count {
                    stable_ticks += 1;
                    if stable_ticks >= 5 {
                        return;
                    }
                } else {
                    stable_ticks = 0;
                    last_count = count;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// A. Concurrent Feed Creation (50 tasks)
// ---------------------------------------------------------------------------
#[tokio::test]
#[serial_test::serial]
async fn stress_concurrent_feed_creation() -> Result<()> {
    tokio::time::timeout(TIMEOUT, async {
        let mut tc = TestBuilder::all().build()?;
        tc.init_feed_server().await?;
        let client = tc.client()?;
        let feed_url = tc.rss_feed_url();

        let mut js = JoinSet::new();
        for i in 0..50 {
            let c = client.clone();
            let url = feed_url.clone();
            js.spawn(async move {
                let resp = c
                    .post(format!("{BASE}/v1/feeds/create"))
                    .json(&serde_json::json!({ "title": format!("feed-{i}"), "url": url }))
                    .send()
                    .await
                    .unwrap();
                (i, resp.status().as_u16(), resp.json::<Value>().await.ok())
            });
        }

        let mut success_ids: HashSet<i64> = HashSet::new();
        let mut success_titles: HashSet<String> = HashSet::new();
        let mut success_count = 0u32;

        while let Some(result) = js.join_next().await {
            let (i, status, body) = result?;
            if status == 201 {
                success_count += 1;
                success_titles.insert(format!("feed-{i}"));
                if let Some(b) = body {
                    if let Some(id) = b["id"].as_i64() {
                        success_ids.insert(id);
                    }
                }
            }
        }

        // All returned IDs are distinct
        assert_eq!(
            success_ids.len() as u32,
            success_count,
            "duplicate IDs returned"
        );

        // Count in DB matches success count
        let conn = tc.database_conn()?;
        let db_count: i64 = conn.query_row("SELECT COUNT(*) FROM feeds", [], |row| row.get(0))?;
        assert_eq!(
            db_count as u32, success_count,
            "DB feed count doesn't match 201 count"
        );

        // All feeds have the correct URL
        let mut stmt = conn.prepare("SELECT url FROM feeds")?;
        let urls: Vec<String> = stmt
            .query_map([], |row| row.get(0))?
            .map(|r| r.unwrap())
            .collect();
        for u in &urls {
            assert_eq!(u, &feed_url, "feed URL mismatch");
        }

        // GET /v1/feeds returns a superset of the successful titles
        let resp = client
            .get(format!("{BASE}/v1/feeds?limit=100"))
            .send()
            .await?;
        assert_eq!(resp.status(), 200);
        let body: Value = resp.json().await?;
        let listed_titles: HashSet<String> = body["feeds"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["title"].as_str().unwrap().to_string())
            .collect();
        assert!(
            success_titles.is_subset(&listed_titles),
            "listed feeds missing some successfully created titles"
        );

        tc.assert_db_integrity();

        // Final health check
        let resp = client.get(format!("{BASE}/v1/health")).send().await?;
        assert_eq!(resp.status(), 200);

        Ok(())
    })
    .await?
}

// ---------------------------------------------------------------------------
// B. Concurrent Fetch Triggers + Read Load
// ---------------------------------------------------------------------------
#[tokio::test]
#[serial_test::serial]
async fn stress_concurrent_fetch_and_reads() -> Result<()> {
    tokio::time::timeout(TIMEOUT, async {
        let mut tc = TestBuilder::all().build()?;
        tc.init_feed_server().await?;
        let client = tc.client()?;
        let feed_url = tc.rss_feed_url();

        let conn = tc.database_conn()?;
        let feed_ids = populate_n_feeds(&conn, 20, &feed_url);
        drop(conn);

        let mut js = JoinSet::new();

        // 100 fetch triggers
        for i in 0..100 {
            let c = client.clone();
            let fid = feed_ids[i % feed_ids.len()];
            js.spawn(async move {
                let resp = c
                    .post(format!("{BASE}/v1/feeds/refresh/{fid}"))
                    .send()
                    .await
                    .unwrap();
                ("fetch", resp.status().as_u16())
            });
        }

        // 50 concurrent reads
        for _ in 0..50 {
            let c = client.clone();
            js.spawn(async move {
                let resp = c.get(format!("{BASE}/v1/entries")).send().await.unwrap();
                ("read", resp.status().as_u16())
            });
        }

        while let Some(result) = js.join_next().await {
            let (kind, status) = result?;
            match kind {
                "fetch" => assert_eq!(status, 202, "fetch should return 202"),
                "read" => assert_eq!(status, 200, "GET entries should return 200"),
                _ => unreachable!(),
            }
        }

        // Wait for workers to drain
        wait_for_entries_stable(&client, Duration::from_secs(15)).await;

        let conn = tc.database_conn()?;

        // Every feed was fetched
        let distinct_feeds: i64 =
            conn.query_row("SELECT COUNT(DISTINCT feed_id) FROM entries", [], |row| {
                row.get(0)
            })?;
        assert_eq!(distinct_feeds, 20, "not all feeds were fetched");

        // Each feed has exactly 6 entries
        let mut stmt = conn.prepare("SELECT feed_id, COUNT(*) FROM entries GROUP BY feed_id")?;
        let counts: Vec<(i64, i64)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .map(|r| r.unwrap())
            .collect();
        for (fid, count) in &counts {
            assert_eq!(*count, 6, "feed {fid} has {count} entries, expected 6");
        }

        // No duplicate entries
        let total: i64 = conn.query_row("SELECT COUNT(*) FROM entries", [], |row| row.get(0))?;
        assert_eq!(total, 120, "expected 120 entries (20 * 6), got {total}");

        // All feeds marked as checked
        let checked: i64 = conn.query_row(
            "SELECT COUNT(*) FROM feeds WHERE last_checked IS NOT NULL",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(checked, 20, "not all feeds marked as checked");

        tc.assert_db_integrity();

        let resp = client.get(format!("{BASE}/v1/health")).send().await?;
        assert_eq!(resp.status(), 200);

        Ok(())
    })
    .await?
}

// ---------------------------------------------------------------------------
// C. Concurrent Tag Creation (unique constraint race)
// ---------------------------------------------------------------------------
#[tokio::test]
#[serial_test::serial]
async fn stress_concurrent_tag_creation() -> Result<()> {
    tokio::time::timeout(TIMEOUT, async {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;

        let tag_names: Vec<String> = (0..5).map(|i| format!("tag-{i}")).collect();

        let mut js = JoinSet::new();
        for i in 0..30 {
            let c = client.clone();
            let name = tag_names[i % 5].clone();
            js.spawn(async move {
                let resp = c
                    .post(format!("{BASE}/v1/tags/create"))
                    .json(&serde_json::json!({ "name": name }))
                    .send()
                    .await
                    .unwrap();
                let status = resp.status().as_u16();
                let body: Value = resp.json().await.unwrap_or_default();
                (name, status, body)
            });
        }

        let mut created_count = 0u32;
        let mut conflict_count = 0u32;
        let mut created_ids: HashSet<i64> = HashSet::new();
        let mut per_tag_201: std::collections::HashMap<String, u32> =
            std::collections::HashMap::new();

        while let Some(result) = js.join_next().await {
            let (name, status, body) = result?;
            assert!(
                status == 201 || status == 409,
                "unexpected status {status} for tag '{name}'"
            );
            if status == 201 {
                created_count += 1;
                *per_tag_201.entry(name).or_default() += 1;
                if let Some(id) = body["id"].as_i64() {
                    created_ids.insert(id);
                }
            } else {
                conflict_count += 1;
            }
        }

        assert_eq!(created_count, 5, "expected exactly 5 tags created");
        assert_eq!(conflict_count, 25, "expected 25 conflicts");
        for (name, count) in &per_tag_201 {
            assert_eq!(*count, 1, "tag '{name}' created {count} times");
        }
        assert_eq!(created_ids.len(), 5, "created IDs not all distinct");

        let conn = tc.database_conn()?;
        let db_count: i64 = conn.query_row("SELECT COUNT(*) FROM tags", [], |row| row.get(0))?;
        assert_eq!(db_count, 5, "DB has {db_count} tags, expected 5");

        // GET /v1/tags returns exactly 5
        let resp = client.get(format!("{BASE}/v1/tags")).send().await?;
        assert_eq!(resp.status(), 200);
        let body: Value = resp.json().await?;
        let api_names: HashSet<String> = body["tags"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        let expected_names: HashSet<String> = tag_names.into_iter().collect();
        assert_eq!(api_names, expected_names, "tag names mismatch");

        tc.assert_db_integrity();

        let resp = client.get(format!("{BASE}/v1/health")).send().await?;
        assert_eq!(resp.status(), 200);

        Ok(())
    })
    .await?
}

// ---------------------------------------------------------------------------
// D. Feed Refresh vs. Retention Cleanup Race
// ---------------------------------------------------------------------------
#[tokio::test]
#[serial_test::serial]
async fn stress_refresh_vs_cleanup_race() -> Result<()> {
    tokio::time::timeout(TIMEOUT, async {
        let mut tc = TestBuilder::all().build()?;
        tc.init_feed_server().await?;
        let client = tc.client()?;
        let feed_url = tc.rss_feed_url();

        let conn = tc.database_conn()?;
        let feed_ids = populate_n_feeds(&conn, 5, &feed_url);

        drop(conn);

        // Set a retention policy so cleanup has work to do
        let resp = client
            .put(format!("{BASE}/v1/settings/retention"))
            .json(&serde_json::json!({"max_age_days": 1}))
            .send()
            .await?;
        assert_eq!(resp.status(), 200);

        // Trigger initial fetches and wait
        for fid in &feed_ids {
            let resp = client
                .post(format!("{BASE}/v1/feeds/refresh/{fid}"))
                .send()
                .await?;
            assert_eq!(resp.status(), 202);
        }
        wait_for_entries_stable(&client, Duration::from_secs(10)).await;

        // Now spawn concurrent fetches + cleanups
        let mut js = JoinSet::new();

        // Reset last_checked so re-fetches proceed
        let conn = tc.database_conn()?;
        conn.execute("UPDATE feeds SET last_checked = NULL", [])?;
        drop(conn);

        for i in 0..20 {
            let c = client.clone();
            let fid = feed_ids[i % feed_ids.len()];
            js.spawn(async move {
                let resp = c
                    .post(format!("{BASE}/v1/feeds/refresh/{fid}"))
                    .send()
                    .await
                    .unwrap();
                ("fetch", resp.status().as_u16())
            });
        }

        for _ in 0..10 {
            let c = client.clone();
            js.spawn(async move {
                let resp = c
                    .post(format!("{BASE}/v1/entries/cleanup"))
                    .send()
                    .await
                    .unwrap();
                ("cleanup", resp.status().as_u16())
            });
        }

        while let Some(result) = js.join_next().await {
            let (kind, status) = result?;
            match kind {
                "fetch" => assert_eq!(status, 202, "fetch should return 202"),
                "cleanup" => assert_eq!(status, 200, "cleanup should return 200"),
                _ => unreachable!(),
            }
        }

        // Wait for everything to settle
        wait_for_entries_stable(&client, Duration::from_secs(10)).await;

        let conn = tc.database_conn()?;

        // FK integrity
        let fk: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_foreign_key_check()",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(fk, 0, "foreign key violations after race");

        // No orphaned entry_tags
        let orphaned: i64 = conn.query_row(
            "SELECT COUNT(*) FROM entry_tags WHERE entry_id NOT IN (SELECT id FROM entries)",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(orphaned, 0, "orphaned entry_tags after race");

        // Every remaining entry has a valid feed_id
        let bad_feed: i64 = conn.query_row(
            "SELECT COUNT(*) FROM entries WHERE feed_id NOT IN (SELECT id FROM feeds)",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(bad_feed, 0, "entries with invalid feed_id");

        // No duplicate entries per feed
        let dupes: i64 = conn.query_row(
            "SELECT COUNT(*) FROM (
                SELECT feed_id, guid, COUNT(*) AS c
                FROM entries GROUP BY feed_id, guid HAVING c > 1
            )",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(dupes, 0, "duplicate entries per feed after race");

        tc.assert_db_integrity();

        let resp = client.get(format!("{BASE}/v1/health")).send().await?;
        assert_eq!(resp.status(), 200);

        Ok(())
    })
    .await?
}

// ---------------------------------------------------------------------------
// E. Concurrent CRUD on Same Feed
// ---------------------------------------------------------------------------
#[tokio::test]
#[serial_test::serial]
async fn stress_concurrent_crud_same_feed() -> Result<()> {
    tokio::time::timeout(TIMEOUT, async {
        let mut tc = TestBuilder::all().build()?;
        tc.init_feed_server().await?;
        let client = tc.client()?;
        let feed_url = tc.rss_feed_url();

        // Create one feed via API
        let resp = client
            .post(format!("{BASE}/v1/feeds/create"))
            .json(&serde_json::json!({ "title": "original", "url": feed_url }))
            .send()
            .await?;
        assert_eq!(resp.status(), 201);
        let body: Value = resp.json().await?;
        let feed_id = body["id"].as_i64().unwrap();

        let titles: Vec<String> = (0..20).map(|i| format!("title-{i}")).collect();

        let mut js = JoinSet::new();

        // 20 PUT tasks
        for title in titles.clone() {
            let c = client.clone();
            js.spawn(async move {
                let resp = c
                    .put(format!("{BASE}/v1/feeds/id/{feed_id}"))
                    .json(&serde_json::json!({ "title": title }))
                    .send()
                    .await
                    .unwrap();
                ("put", resp.status().as_u16())
            });
        }

        // 20 fetch triggers
        for _ in 0..20 {
            let c = client.clone();
            js.spawn(async move {
                let resp = c
                    .post(format!("{BASE}/v1/feeds/refresh/{feed_id}"))
                    .send()
                    .await
                    .unwrap();
                ("fetch", resp.status().as_u16())
            });
        }

        // 10 GET feed
        for _ in 0..10 {
            let c = client.clone();
            js.spawn(async move {
                let resp = c
                    .get(format!("{BASE}/v1/feeds/id/{feed_id}"))
                    .send()
                    .await
                    .unwrap();
                ("get_feed", resp.status().as_u16())
            });
        }

        // 5 GET entries
        for _ in 0..5 {
            let c = client.clone();
            js.spawn(async move {
                let resp = c.get(format!("{BASE}/v1/entries")).send().await.unwrap();
                ("get_entries", resp.status().as_u16())
            });
        }

        while let Some(result) = js.join_next().await {
            let (kind, status) = result?;
            match kind {
                "put" => assert_eq!(status, 200, "PUT returned {status}"),
                "fetch" => assert_eq!(status, 202, "fetch should return 202"),
                "get_feed" => assert_eq!(status, 200, "GET feed should return 200"),
                "get_entries" => assert_eq!(status, 200, "GET entries should return 200"),
                _ => unreachable!(),
            }
        }

        // Wait for workers
        wait_for_entries_stable(&client, Duration::from_secs(10)).await;

        // Final feed state is consistent
        let resp = client
            .get(format!("{BASE}/v1/feeds/id/{feed_id}"))
            .send()
            .await?;
        assert_eq!(resp.status(), 200);
        let body: Value = resp.json().await?;
        let final_title = body["title"].as_str().unwrap().to_string();
        let valid_titles: HashSet<&str> = titles.iter().map(|s| s.as_str()).collect();
        assert!(
            valid_titles.contains(final_title.as_str()),
            "final title '{final_title}' is not one of the valid titles"
        );

        // Feed URL still valid
        let final_url = body["url"].as_str().unwrap();
        assert_eq!(final_url, &feed_url, "feed URL was corrupted");

        // All entries belong to the feed
        let conn = tc.database_conn()?;
        let wrong_feed: i64 = conn.query_row(
            "SELECT COUNT(*) FROM entries WHERE feed_id != ?1",
            [feed_id],
            |row| row.get(0),
        )?;
        assert_eq!(wrong_feed, 0, "entries belong to wrong feed");

        tc.assert_db_integrity();

        let resp = client.get(format!("{BASE}/v1/health")).send().await?;
        assert_eq!(resp.status(), 200);

        Ok(())
    })
    .await?
}

// ---------------------------------------------------------------------------
// F. Script Reload During Active Processing
// ---------------------------------------------------------------------------
#[cfg(feature = "lua")]
#[tokio::test]
#[serial_test::serial]
async fn stress_script_reload_during_processing() -> Result<()> {
    tokio::time::timeout(TIMEOUT, async {
        let mut tc = TestBuilder::all().build()?;
        tc.init_feed_server().await?;
        let client = tc.client()?;
        let feed_url = tc.rss_feed_url();

        let conn = tc.database_conn()?;
        let feed_ids = populate_n_feeds(&conn, 10, &feed_url);

        // Install a simple pass-through Lua plugin
        tc.install_lua_plugin(
            "passthrough",
            r#"kiki.on("entry.ingest", function(entry) return entry end)"#,
            serde_json::json!({}),
        )?;
        drop(conn);

        let mut js = JoinSet::new();

        // 50 fetch triggers
        for i in 0..50 {
            let c = client.clone();
            let fid = feed_ids[i % feed_ids.len()];
            js.spawn(async move {
                let resp = c
                    .post(format!("{BASE}/v1/feeds/refresh/{fid}"))
                    .send()
                    .await
                    .unwrap();
                ("fetch", resp.status().as_u16())
            });
        }

        // 20 plugin reload requests
        for _ in 0..20 {
            let c = client.clone();
            js.spawn(async move {
                let resp = c
                    .post(format!("{BASE}/v1/plugins/reload"))
                    .send()
                    .await
                    .unwrap();
                ("reload", resp.status().as_u16())
            });
        }

        while let Some(result) = js.join_next().await {
            let (kind, status) = result?;
            match kind {
                "fetch" => assert_eq!(status, 202, "fetch should return 202"),
                "reload" => assert_eq!(status, 202, "reload should return 202"),
                _ => unreachable!(),
            }
        }

        // Wait for workers to finish
        wait_for_entries_stable(&client, Duration::from_secs(15)).await;

        // Server is still responsive
        let resp = client.get(format!("{BASE}/v1/health")).send().await?;
        assert_eq!(resp.status(), 200);

        // Workers still functional
        let resp = client
            .post(format!("{BASE}/v1/feeds/refresh/{}", feed_ids[0]))
            .send()
            .await?;
        assert_eq!(resp.status(), 202);

        // At least some feeds were processed
        let conn = tc.database_conn()?;
        let checked: i64 = conn.query_row(
            "SELECT COUNT(*) FROM feeds WHERE last_checked IS NOT NULL",
            [],
            |row| row.get(0),
        )?;
        assert!(checked > 0, "no feeds were processed");

        tc.assert_db_integrity();

        Ok(())
    })
    .await?
}
