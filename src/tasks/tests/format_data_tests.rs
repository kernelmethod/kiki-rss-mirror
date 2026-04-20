#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::type_complexity
)]

use super::super::*;
use crate::test::TestBuilder;
use anyhow::Result;
use rusqlite::OpenFlags;

fn make_pool(path: &std::path::Path) -> Result<r2d2::Pool<SqliteConnectionManager>> {
    let manager = SqliteConnectionManager::file(path)
        .with_flags(OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .with_init(|c| c.execute_batch("PRAGMA foreign_keys=ON;"));
    Ok(r2d2::Pool::new(manager)?)
}

async fn refresh_feed_at_url(tc: &crate::test::TestConfig, url: String) -> Result<i64> {
    let conn = tc.database_conn()?;
    conn.execute(
        "INSERT INTO feeds (title, url) VALUES ('test feed', ?1)",
        [url],
    )?;
    let feed_id = conn.last_insert_rowid();

    let client = reqwest::Client::builder()
        .user_agent(crate::http::USER_AGENT)
        .build()?;
    let pool = make_pool(&tc.database_path())?;

    refresh_feed(&client, feed_id, pool, None, &super::test_metrics()).await?;
    Ok(feed_id)
}

#[tokio::test]
async fn ingest_rss_populates_rss_entry_data() -> Result<()> {
    let tc = TestBuilder::default().init_database().build()?;
    let feed_id = refresh_feed_at_url(&tc, tc.rich_rss_feed_url()).await?;

    let conn = tc.database_conn()?;

    // The feed's syndication_format gets set to 'rss' by ingestion.
    let fmt: String = conn.query_row(
        "SELECT syndication_format FROM feeds WHERE id = ?1",
        [feed_id],
        |row| row.get(0),
    )?;
    assert_eq!(fmt, "rss");

    // All three items produce an rss_entry_data row.
    let ed_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM rss_entry_data red
         JOIN entries e ON red.entry_id = e.id
         WHERE e.feed_id = ?1",
        [feed_id],
        |row| row.get(0),
    )?;
    assert_eq!(ed_count, 3);

    // Item 1 has everything.
    let (desc, comments, author, enc_url, enc_len, enc_mime): (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<i64>,
        Option<String>,
    ) = conn.query_row(
        "SELECT description, comments, author,
                enclosure_url, enclosure_length, enclosure_mime_type
         FROM rss_entry_data red
         JOIN entries e ON red.entry_id = e.id
         WHERE e.feed_id = ?1 AND e.guid = 'http://example.com/items/1'",
        [feed_id],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        },
    )?;
    assert_eq!(desc.as_deref(), Some("A full-featured item."));
    assert_eq!(
        comments.as_deref(),
        Some("http://example.com/items/1/comments")
    );
    assert_eq!(author.as_deref(), Some("alice@example.com (Alice)"));
    assert_eq!(enc_url.as_deref(), Some("http://example.com/audio.mp3"));
    assert_eq!(enc_len, Some(12345));
    assert_eq!(enc_mime.as_deref(), Some("audio/mpeg"));

    // Two categories for item 1.
    let item1_cats: i64 = conn.query_row(
        "SELECT COUNT(*) FROM rss_categories rc
         JOIN entries e ON rc.entry_id = e.id
         WHERE e.feed_id = ?1 AND e.guid = 'http://example.com/items/1'",
        [feed_id],
        |row| row.get(0),
    )?;
    assert_eq!(item1_cats, 2);

    // Total: 2 + 1 + 0 = 3.
    let total_cats: i64 = conn.query_row(
        "SELECT COUNT(*) FROM rss_categories rc
         JOIN entries e ON rc.entry_id = e.id
         WHERE e.feed_id = ?1",
        [feed_id],
        |row| row.get(0),
    )?;
    assert_eq!(total_cats, 3);

    tc.assert_db_integrity();
    Ok(())
}

#[tokio::test]
async fn ingest_rss_handles_missing_enclosure() -> Result<()> {
    let tc = TestBuilder::default().init_database().build()?;
    let feed_id = refresh_feed_at_url(&tc, tc.rich_rss_feed_url()).await?;

    let conn = tc.database_conn()?;

    // Item 3 — no enclosure, no categories, no author.
    let (enc_url, author, cat_count): (Option<String>, Option<String>, i64) = conn.query_row(
        "SELECT red.enclosure_url, red.author,
                (SELECT COUNT(*) FROM rss_categories rc WHERE rc.entry_id = e.id)
         FROM rss_entry_data red
         JOIN entries e ON red.entry_id = e.id
         WHERE e.feed_id = ?1 AND e.guid = 'http://example.com/items/3'",
        [feed_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    assert_eq!(enc_url, None);
    assert_eq!(author, None);
    assert_eq!(cat_count, 0);

    tc.assert_db_integrity();
    Ok(())
}

#[tokio::test]
async fn ingest_atom_populates_atom_feed_data() -> Result<()> {
    let tc = TestBuilder::default().init_database().build()?;
    let feed_id = refresh_feed_at_url(&tc, tc.rich_atom_feed_url()).await?;

    let conn = tc.database_conn()?;

    let lang_tag: Option<String> = conn.query_row(
        "SELECT atom_language_tag FROM atom_feed_data WHERE feed_id = ?1",
        [feed_id],
        |row| row.get(0),
    )?;
    assert_eq!(lang_tag.as_deref(), Some("en-US"));

    let rights: Option<String> = conn
        .query_row(
            "SELECT rights FROM atom_feed_rights WHERE feed_id = ?1",
            [feed_id],
            |row| row.get(0),
        )
        .ok();
    assert_eq!(rights.as_deref(), Some("(c) 2026 Example Corp"));

    let author_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM atom_feed_authors WHERE feed_id = ?1",
        [feed_id],
        |row| row.get(0),
    )?;
    assert_eq!(author_count, 2);

    let contrib_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM atom_feed_contributors WHERE feed_id = ?1",
        [feed_id],
        |row| row.get(0),
    )?;
    assert_eq!(contrib_count, 2);

    let (gen_value, gen_uri, gen_version): (String, Option<String>, Option<String>) = conn
        .query_row(
            "SELECT value, uri, version FROM atom_feed_generators WHERE feed_id = ?1",
            [feed_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
    assert_eq!(gen_value, "Example Generator");
    assert_eq!(gen_uri.as_deref(), Some("https://example.com/gen"));
    assert_eq!(gen_version.as_deref(), Some("1.2"));

    let logo: String = conn.query_row(
        "SELECT uri FROM atom_feed_logos WHERE feed_id = ?1",
        [feed_id],
        |row| row.get(0),
    )?;
    assert_eq!(logo, "http://example.com/logo.png");

    let icon: String = conn.query_row(
        "SELECT uri FROM atom_feed_icons WHERE feed_id = ?1",
        [feed_id],
        |row| row.get(0),
    )?;
    assert_eq!(icon, "http://example.com/icon.png");

    // Two categories joined through atom_feed_categories.
    let cats: Vec<(String, Option<String>, Option<String>)> = conn
        .prepare(
            "SELECT ac.category, ac.scheme, ac.label
             FROM atom_feed_categories afc
             JOIN atom_categories ac ON afc.category_id = ac.id
             WHERE afc.feed_id = ?1
             ORDER BY ac.id",
        )?
        .query_map([feed_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    assert_eq!(cats.len(), 2);
    assert_eq!(cats[0].0, "t1");
    assert_eq!(cats[0].2.as_deref(), Some("Label One"));
    assert_eq!(cats[1].0, "t2");
    assert_eq!(cats[1].2.as_deref(), Some("Label Two"));

    tc.assert_db_integrity();
    Ok(())
}

#[tokio::test]
async fn ingest_atom_populates_atom_entry_data() -> Result<()> {
    let tc = TestBuilder::default().init_database().build()?;
    let feed_id = refresh_feed_at_url(&tc, tc.rich_atom_feed_url()).await?;

    let conn = tc.database_conn()?;

    // Entry A (rich) should have rights, 2 authors, 1 contributor, 1 category.
    let entry_a_id: i64 = conn.query_row(
        "SELECT id FROM entries WHERE feed_id = ?1 AND guid = ?2",
        rusqlite::params![feed_id, "urn:uuid:1225c695-cfb8-4ebb-aaaa-80da344efa6a"],
        |row| row.get(0),
    )?;

    let rights: String = conn.query_row(
        "SELECT rights FROM atom_entry_rights WHERE entry_id = ?1",
        [entry_a_id],
        |row| row.get(0),
    )?;
    assert_eq!(rights, "(c) 2026 Entry Author");

    let authors: Vec<String> = conn
        .prepare("SELECT author FROM atom_entry_authors WHERE entry_id = ?1 ORDER BY id")?
        .query_map([entry_a_id], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<_>>()?;
    assert_eq!(
        authors,
        vec![
            "Entry Author One".to_string(),
            "Entry Author Two".to_string()
        ]
    );

    let contributors: Vec<String> = conn
        .prepare("SELECT contributor FROM atom_entry_contributors WHERE entry_id = ?1 ORDER BY id")?
        .query_map([entry_a_id], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<_>>()?;
    assert_eq!(contributors, vec!["Entry Contributor".to_string()]);

    let cat_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM atom_entry_categories WHERE entry_id = ?1",
        [entry_a_id],
        |row| row.get(0),
    )?;
    assert_eq!(cat_count, 1);

    // Entry B (bare) should have no child rows at all.
    let entry_b_id: i64 = conn.query_row(
        "SELECT id FROM entries WHERE feed_id = ?1 AND guid = ?2",
        rusqlite::params![feed_id, "urn:uuid:1225c695-cfb8-4ebb-bbbb-80da344efa6a"],
        |row| row.get(0),
    )?;
    let b_rights_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM atom_entry_rights WHERE entry_id = ?1",
        [entry_b_id],
        |row| row.get(0),
    )?;
    assert_eq!(b_rights_count, 0);
    let b_authors: i64 = conn.query_row(
        "SELECT COUNT(*) FROM atom_entry_authors WHERE entry_id = ?1",
        [entry_b_id],
        |row| row.get(0),
    )?;
    assert_eq!(b_authors, 0);

    tc.assert_db_integrity();
    Ok(())
}

#[tokio::test]
async fn re_ingest_does_not_duplicate() -> Result<()> {
    let tc = TestBuilder::default().init_database().build()?;

    // Insert the feed once and refresh twice.
    let conn = tc.database_conn()?;
    conn.execute(
        "INSERT INTO feeds (title, url) VALUES ('test feed', ?1)",
        [tc.rich_atom_feed_url()],
    )?;
    let feed_id = conn.last_insert_rowid();
    // min_fetch_interval defaults to 10800; clear last_checked between
    // refreshes so the second call isn't skipped.
    drop(conn);

    let client = reqwest::Client::builder()
        .user_agent(crate::http::USER_AGENT)
        .build()?;
    let pool = make_pool(&tc.database_path())?;

    refresh_feed(&client, feed_id, pool.clone(), None, &super::test_metrics()).await?;

    // Clear last_checked so the second refresh is not skipped.
    tc.database_conn()?
        .execute("UPDATE feeds SET last_checked = NULL", [])?;

    refresh_feed(&client, feed_id, pool, None, &super::test_metrics()).await?;

    let conn = tc.database_conn()?;
    let afd_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM atom_feed_data WHERE feed_id = ?1",
        [feed_id],
        |row| row.get(0),
    )?;
    assert_eq!(
        afd_count, 1,
        "re-ingest should not duplicate atom_feed_data"
    );

    // Two entries in the fixture; re-ingest should still yield two entry-level
    // author aggregates (one per atom entry with `<author>` elements).
    let entry_author_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM atom_entry_authors aea
         JOIN entries e ON aea.entry_id = e.id
         WHERE e.feed_id = ?1",
        [feed_id],
        |row| row.get(0),
    )?;
    assert_eq!(entry_author_count, 2);

    // Feed-level authors must still be exactly 2, not 4.
    let author_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM atom_feed_authors WHERE feed_id = ?1",
        [feed_id],
        |row| row.get(0),
    )?;
    assert_eq!(author_count, 2);

    tc.assert_db_integrity();
    Ok(())
}
