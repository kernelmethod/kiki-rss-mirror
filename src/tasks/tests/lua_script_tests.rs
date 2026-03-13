#![allow(clippy::unwrap_used, clippy::expect_used)]

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

/// Insert a feed and a Lua script linked to it, then return the feed id,
/// an HTTP client, and a connection pool ready to call [`refresh_feed`].
async fn setup_feed_with_script(
    tc: &crate::test::TestConfig,
    script_text: &str,
) -> Result<(i64, reqwest::Client, r2d2::Pool<SqliteConnectionManager>)> {
    let conn = tc.database_conn()?;
    conn.execute(
        "INSERT INTO feeds (title, url) VALUES ('test feed', ?1)",
        [tc.example_feed_url()],
    )?;
    let feed_id = conn.last_insert_rowid();

    conn.execute(
        "INSERT INTO scripts (engine, text) VALUES ('lua', ?1)",
        [script_text],
    )?;
    let script_id = conn.last_insert_rowid();

    conn.execute(
        "INSERT INTO feed_scripts (feed_id, script_id) VALUES (?1, ?2)",
        rusqlite::params![feed_id, script_id],
    )?;

    let client = reqwest::Client::builder()
        .user_agent(crate::http::USER_AGENT)
        .build()?;
    let pool = make_pool(&tc.database_path())?;

    Ok((feed_id, client, pool))
}

/// A filter-all script should result in zero entries being inserted.
#[tokio::test]
async fn integration_filter_script_drops_all_entries() -> Result<()> {
    let tc = TestBuilder::default().init_database().build()?;
    let (feed_id, client, pool) =
        setup_feed_with_script(&tc, "return function(entry) return nil end").await?;

    let runner = {
        let conn = pool.get()?;
        let sources = load_all_script_sources(&conn)?;
        crate::scripting::lua::LuaScriptRunner::new(&sources)?
    };
    refresh_feed(&client, feed_id, pool, Some(&runner as &dyn ScriptRunner)).await?;

    let conn = tc.database_conn()?;
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM entries WHERE feed_id = ?1",
        [feed_id],
        |row| row.get(0),
    )?;
    assert_eq!(count, 0, "filter script should have dropped all entries");

    Ok(())
}

/// A modifying script should persist its changes to the database.
#[tokio::test]
async fn integration_modify_script_changes_titles() -> Result<()> {
    let tc = TestBuilder::default().init_database().build()?;
    let (feed_id, client, pool) = setup_feed_with_script(
        &tc,
        r#"return function(entry) entry.title = "[MODIFIED] " .. entry.title; return entry end"#,
    )
    .await?;

    let runner = {
        let conn = pool.get()?;
        let sources = load_all_script_sources(&conn)?;
        crate::scripting::lua::LuaScriptRunner::new(&sources)?
    };
    refresh_feed(&client, feed_id, pool, Some(&runner as &dyn ScriptRunner)).await?;

    let conn = tc.database_conn()?;
    let mut stmt = conn.prepare("SELECT title FROM entries WHERE feed_id = ?1")?;
    let titles: Vec<String> = stmt
        .query_map([feed_id], |row| row.get(0))?
        .collect::<Result<Vec<_>, _>>()?;

    assert!(!titles.is_empty(), "expected entries to be inserted");
    for title in &titles {
        assert!(
            title.starts_with("[MODIFIED] "),
            "title was not modified by script: {title}"
        );
    }

    Ok(())
}

/// A tagging script should add the specified tag to every entry.
#[tokio::test]
async fn integration_tagging_script_adds_tags() -> Result<()> {
    let tc = TestBuilder::default().init_database().build()?;
    let (feed_id, client, pool) = setup_feed_with_script(
        &tc,
        r#"return function(entry) table.insert(entry.tags, "test-tag"); return entry end"#,
    )
    .await?;

    let runner = {
        let conn = pool.get()?;
        let sources = load_all_script_sources(&conn)?;
        crate::scripting::lua::LuaScriptRunner::new(&sources)?
    };
    refresh_feed(&client, feed_id, pool, Some(&runner as &dyn ScriptRunner)).await?;

    let conn = tc.database_conn()?;
    let entry_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM entries WHERE feed_id = ?1",
        [feed_id],
        |row| row.get(0),
    )?;
    assert!(entry_count > 0, "expected entries to be inserted");

    let tagged_count: i64 = conn.query_row(
        "SELECT COUNT(DISTINCT e.id) FROM entries e
         JOIN entry_tags et ON et.entry_id = e.id
         JOIN tags t ON t.id = et.tag_id
         WHERE e.feed_id = ?1 AND t.name = 'test-tag'",
        [feed_id],
        |row| row.get(0),
    )?;
    assert_eq!(
        entry_count, tagged_count,
        "every entry should have the test-tag"
    );

    Ok(())
}

/// When a filter script runs before a tagging script, the filter should
/// prevent all entries from being inserted and the tagging script should
/// never run.
#[tokio::test]
async fn integration_filter_script_prevents_tagging_script() -> Result<()> {
    let tc = TestBuilder::default().init_database().build()?;

    let conn = tc.database_conn()?;
    conn.execute(
        "INSERT INTO feeds (title, url) VALUES ('test feed', ?1)",
        [tc.example_feed_url()],
    )?;
    let feed_id = conn.last_insert_rowid();

    // Insert the filter script first so it runs first in the chain.
    conn.execute(
        "INSERT INTO scripts (engine, text) VALUES ('lua', 'return function(entry) return nil end')",
        [],
    )?;
    let filter_script_id = conn.last_insert_rowid();

    // Insert the tagging script second.
    conn.execute(
        "INSERT INTO scripts (engine, text) VALUES ('lua', 'return function(entry) table.insert(entry.tags, \"should-not-appear\"); return entry end')",
        [],
    )?;
    let tag_script_id = conn.last_insert_rowid();

    conn.execute(
        "INSERT INTO feed_scripts (feed_id, script_id) VALUES (?1, ?2)",
        rusqlite::params![feed_id, filter_script_id],
    )?;
    conn.execute(
        "INSERT INTO feed_scripts (feed_id, script_id) VALUES (?1, ?2)",
        rusqlite::params![feed_id, tag_script_id],
    )?;

    let client = reqwest::Client::builder()
        .user_agent(crate::http::USER_AGENT)
        .build()?;
    let pool = make_pool(&tc.database_path())?;

    let runner = {
        let conn = pool.get()?;
        let sources = load_all_script_sources(&conn)?;
        crate::scripting::lua::LuaScriptRunner::new(&sources)?
    };
    refresh_feed(&client, feed_id, pool, Some(&runner as &dyn ScriptRunner)).await?;

    let conn = tc.database_conn()?;
    let entry_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM entries WHERE feed_id = ?1",
        [feed_id],
        |row| row.get(0),
    )?;
    assert_eq!(
        entry_count, 0,
        "filter script should have prevented all entries from being inserted"
    );

    let tag_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM tags WHERE name = 'should-not-appear'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(
        tag_count, 0,
        "tagging script should not have run after filter script dropped the entry"
    );

    Ok(())
}
