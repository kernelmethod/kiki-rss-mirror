#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::super::scripting::load_plugin_sources;
use super::super::*;
use crate::scripting::ScriptRunner;
use crate::test::TestBuilder;
use anyhow::Result;

fn make_pool(path: &std::path::Path) -> Result<crate::db::Db> {
    crate::db::Db::open(path, Default::default())
}

/// Insert a feed and install a Lua plugin, then return the feed id, an
/// HTTP client, and a database handle ready to call [`refresh_feed`].
async fn setup_feed_with_script(
    tc: &crate::test::TestConfig,
    script_text: &str,
) -> Result<(i64, reqwest::Client, crate::db::Db)> {
    setup_feed_with_configured_script(tc, script_text, serde_json::json!({})).await
}

/// As [`setup_feed_with_script`], giving the plugin the config `config`.
async fn setup_feed_with_configured_script(
    tc: &crate::test::TestConfig,
    script_text: &str,
    config: serde_json::Value,
) -> Result<(i64, reqwest::Client, crate::db::Db)> {
    let conn = tc.database_conn()?;
    conn.execute(
        "INSERT INTO feeds (title, url) VALUES ('test feed', ?1)",
        [tc.example_feed_url()],
    )?;
    let feed_id = conn.last_insert_rowid();

    tc.install_lua_plugin("test-plugin", script_text, config)?;

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
    let (feed_id, client, pool) = setup_feed_with_script(
        &tc,
        r#"kiki.on("entry.ingest", function(entry) return nil end)"#,
    )
    .await?;

    let runner = {
        let sources = load_plugin_sources(&crate::plugins::discover(&tc.plugins_dir())?);
        crate::scripting::lua::LuaScriptRunner::from_sources(&sources)?
    };
    refresh_feed(
        &client,
        feed_id,
        pool,
        Some(&runner as &dyn ScriptRunner),
        &super::test_metrics(),
        &super::test_tx(),
    )
    .await?;

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
        r#"kiki.on("entry.ingest", function(entry)
            entry.title = "[MODIFIED] " .. entry.title
            return entry
        end)"#,
    )
    .await?;

    let runner = {
        let sources = load_plugin_sources(&crate::plugins::discover(&tc.plugins_dir())?);
        crate::scripting::lua::LuaScriptRunner::from_sources(&sources)?
    };
    refresh_feed(
        &client,
        feed_id,
        pool,
        Some(&runner as &dyn ScriptRunner),
        &super::test_metrics(),
        &super::test_tx(),
    )
    .await?;

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

/// When a script rewrites an RSS entry's content, the original
/// `<description>` is kept in `rss_entry_data` rather than deduplicated away.
#[tokio::test]
async fn integration_content_script_preserves_rss_description() -> Result<()> {
    let tc = TestBuilder::default().init_database().build()?;
    let (feed_id, client, pool) = setup_feed_with_script(
        &tc,
        r#"kiki.on("entry.ingest", function(entry)
            if entry.content then
                entry.content = "[MODIFIED] " .. entry.content
            end
            return entry
        end)"#,
    )
    .await?;
    tc.database_conn()?.execute(
        "UPDATE feeds SET url = ?1 WHERE id = ?2",
        rusqlite::params![tc.rich_rss_feed_url(), feed_id],
    )?;

    let runner = {
        let sources = load_plugin_sources(&crate::plugins::discover(&tc.plugins_dir())?);
        crate::scripting::lua::LuaScriptRunner::from_sources(&sources)?
    };
    refresh_feed(
        &client,
        feed_id,
        pool,
        Some(&runner as &dyn ScriptRunner),
        &super::test_metrics(),
        &super::test_tx(),
    )
    .await?;

    let conn = tc.database_conn()?;
    let (entry_id, content, stored): (i64, Option<String>, Option<String>) = conn.query_row(
        "SELECT e.id, e.content, red.description
         FROM entries e JOIN rss_entry_data red ON red.entry_id = e.id
         WHERE e.feed_id = ?1 AND e.guid = 'http://example.com/items/1'",
        [feed_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    assert_eq!(content.as_deref(), Some("[MODIFIED] A full-featured item."));
    assert_eq!(stored.as_deref(), Some("A full-featured item."));

    let loaded = crate::routes::v1::entries::format_data::load_rss_entry_data(&conn, entry_id)?
        .expect("rss entry data present");
    assert_eq!(loaded.description.as_deref(), Some("A full-featured item."));

    Ok(())
}

/// A tagging script should add the specified tag to every entry.
#[tokio::test]
async fn integration_tagging_script_adds_tags() -> Result<()> {
    let tc = TestBuilder::default().init_database().build()?;
    let (feed_id, client, pool) = setup_feed_with_script(
        &tc,
        r#"kiki.on("entry.ingest", function(entry)
            table.insert(entry.tags, "test-tag")
            return entry
        end)"#,
    )
    .await?;

    let runner = {
        let sources = load_plugin_sources(&crate::plugins::discover(&tc.plugins_dir())?);
        crate::scripting::lua::LuaScriptRunner::from_sources(&sources)?
    };
    refresh_feed(
        &client,
        feed_id,
        pool,
        Some(&runner as &dyn ScriptRunner),
        &super::test_metrics(),
        &super::test_tx(),
    )
    .await?;

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

    // Plugins load in directory order, so the filter plugin runs first in
    // the chain and the tagging plugin second.
    tc.install_lua_plugin(
        "10-filter",
        r#"kiki.on("entry.ingest", function(entry) return nil end)"#,
        serde_json::json!({}),
    )?;
    tc.install_lua_plugin(
        "20-tag",
        r#"kiki.on("entry.ingest", function(entry) table.insert(entry.tags, "should-not-appear"); return entry end)"#,
        serde_json::json!({}),
    )?;

    let client = reqwest::Client::builder()
        .user_agent(crate::http::USER_AGENT)
        .build()?;
    let pool = make_pool(&tc.database_path())?;

    let runner = {
        let sources = load_plugin_sources(&crate::plugins::discover(&tc.plugins_dir())?);
        crate::scripting::lua::LuaScriptRunner::from_sources(&sources)?
    };
    refresh_feed(
        &client,
        feed_id,
        pool,
        Some(&runner as &dyn ScriptRunner),
        &super::test_metrics(),
        &super::test_tx(),
    )
    .await?;

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

/// A filter script configured with regexes hides matching entries when they
/// are first stored, and does not re-hide an entry the user has unhidden.
#[tokio::test]
async fn integration_configured_regex_filter_hides_entries() -> Result<()> {
    let tc = TestBuilder::default().init_database().build()?;
    let (feed_id, client, pool) = setup_feed_with_configured_script(
        &tc,
        r#"
        local config = ...
        local rules = {}
        for _, rule in ipairs(config.rules) do
            table.insert(rules, { field = rule.field, re = kiki.regex(rule.pattern, rule.flags) })
        end
        kiki.on("entry.ingest", function(entry)
            for _, rule in ipairs(rules) do
                local value = entry[rule.field]
                if value ~= nil and rule.re:is_match(value) then
                    table.insert(entry.tags, "system:hidden")
                end
            end
            return entry
        end)
        "#,
        serde_json::json!({"rules": [{"field": "title", "pattern": "\\blinux\\b", "flags": "i"}]}),
    )
    .await?;

    let runner = {
        let sources = load_plugin_sources(&crate::plugins::discover(&tc.plugins_dir())?);
        crate::scripting::lua::LuaScriptRunner::from_sources(&sources)?
    };
    let metrics = super::test_metrics();
    let task_tx = super::test_tx();
    let refresh = |pool| {
        refresh_feed(
            &client,
            feed_id,
            pool,
            Some(&runner as &dyn ScriptRunner),
            &metrics,
            &task_tx,
        )
    };
    let hidden_titles = || -> Result<Vec<String>> {
        let conn = tc.database_conn()?;
        let mut stmt = conn.prepare(
            "SELECT e.title FROM entries e
             JOIN entry_tags et ON et.entry_id = e.id
             JOIN tags t ON t.id = et.tag_id
             WHERE e.feed_id = ?1 AND t.name = 'system:hidden'
             ORDER BY e.title",
        )?;
        let titles = stmt
            .query_map([feed_id], |row| row.get(0))?
            .collect::<Result<Vec<String>, _>>()?;
        Ok(titles)
    };

    refresh(pool.clone()).await?;
    assert_eq!(
        hidden_titles()?,
        [
            "A short note on setting up a Linux kernel debugging environment",
            "Linux Security Modules",
        ]
    );

    // The user unhides an entry; refreshing the feed leaves it unhidden.
    tc.database_conn()?.execute(
        "DELETE FROM entry_tags
         WHERE tag_id = (SELECT id FROM tags WHERE name = 'system:hidden')
           AND entry_id = (SELECT id FROM entries WHERE title = 'Linux Security Modules')",
        [],
    )?;
    refresh(pool).await?;
    assert_eq!(
        hidden_titles()?,
        ["A short note on setting up a Linux kernel debugging environment"]
    );

    Ok(())
}

/// Only the entries a script leaves `cache_assets` on for have their
/// assets queued for caching; the others are stored all the same.
#[tokio::test]
async fn integration_scripts_can_skip_asset_caching() -> Result<()> {
    let tc = TestBuilder::default().init_database().build()?;
    let (feed_id, client, pool) = setup_feed_with_script(
        &tc,
        r#"kiki.on("entry.ingest", function(entry)
            entry.cache_assets = #entry.title % 2 == 0
            return entry
        end)"#,
    )
    .await?;

    let runner = {
        let sources = load_plugin_sources(&crate::plugins::discover(&tc.plugins_dir())?);
        crate::scripting::lua::LuaScriptRunner::from_sources(&sources)?
    };
    let (tx, rx) = async_channel::unbounded();
    let tx = crate::tasks::TaskSender::from(tx);
    refresh_feed(
        &client,
        feed_id,
        pool,
        Some(&runner as &dyn ScriptRunner),
        &super::test_metrics(),
        &tx,
    )
    .await?;

    let mut queued = Vec::new();
    while let Ok(command) = rx.try_recv() {
        if let TaskManagerCommand::CacheEntryAssets { entry_id } = command {
            queued.push(entry_id);
        }
    }
    queued.sort_unstable();

    let conn = tc.database_conn()?;
    let mut stmt = conn.prepare("SELECT id, title FROM entries WHERE feed_id = ?1 ORDER BY id")?;
    let entries: Vec<(i64, String)> = stmt
        .query_map([feed_id], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let expected: Vec<i64> = entries
        .iter()
        .filter(|(_, title)| title.len() % 2 == 0)
        .map(|(id, _)| *id)
        .collect();
    assert!(entries.len() > expected.len(), "every entry was cached");
    assert!(!expected.is_empty(), "no entry was cached");
    assert_eq!(queued, expected);
    Ok(())
}
