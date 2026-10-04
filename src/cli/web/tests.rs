#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::feeds::{render_favicon, render_meta};
use super::hosts::AllowedHosts;
use super::layout::SCRIPT_SRC;
use super::listing::fts_query;
use super::server::{api_client, serve_ui};
use super::WebArgs;
use crate::config::{HostPattern, HostPatternError};
use crate::db::tags::SystemTag;
use anyhow::{Context, Result};
use axum::http::{header, StatusCode};
use serde_json::{Map, Value};
use std::path::Path;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::test::TestBuilder;
use clap::Parser;

#[derive(Parser)]
struct TestCli {
    #[command(flatten)]
    web: WebArgs,
}

fn parse(argv: &[&str]) -> WebArgs {
    TestCli::parse_from(std::iter::once("kiki").chain(argv.iter().copied())).web
}

#[test]
fn the_web_ui_listens_on_localhost_by_default() {
    assert_eq!(parse(&[]).listen, "127.0.0.1:8080".parse().unwrap());
}

/// Favicons are shown from the web UI's asset proxy, and only when the
/// API gave a well-formed asset URL.
#[test]
fn favicons_are_rendered_from_the_asset_proxy() {
    let hash = "ab".repeat(32);
    let api_url = format!("/v1/assets/{hash}");
    let html = render_meta(None, Some("Feed"), Some(&api_url), None);
    let img = format!(r#"<img class="favicon" src="/assets/{hash}" alt="""#);
    assert!(html.contains(&img), "{html}");
    for bad in [
        "https://evil.example/x.png".to_owned(),
        "/v1/assets/../../etc".to_owned(),
        format!("{api_url}\"><script>"),
    ] {
        assert_eq!(render_favicon(Some(&bad)), "", "{bad}");
    }
    assert_eq!(render_favicon(None), "");
    // Without a feed name there is nothing to put the icon beside.
    let html = render_meta(None, None, Some(&api_url), None);
    assert!(!html.contains("favicon"), "{html}");
}

/// Server flags are accepted alongside the web UI's own, and reach the
/// `kiki serve` child.
#[test]
fn server_flags_are_passed_through() {
    let args = parse(&["--listen", "127.0.0.1:9000", "--no-sandbox"]);
    assert_eq!(args.listen, "127.0.0.1:9000".parse().unwrap());
    assert_eq!(
        args.serve.to_argv(Path::new("/tmp/k.sock")),
        ["--uds", "/tmp/k.sock", "--no-sandbox"]
    );
}

/// Serve the web UI on an ephemeral port with `api` as its API client,
/// and fetch `path` from it.
async fn get_page(api: reqwest::Client, path: &str) -> Result<(StatusCode, String)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let cancel = CancellationToken::new();
    let task = tokio::spawn(serve_ui(
        listener,
        api,
        AllowedHosts::default(),
        cancel.clone(),
    ));

    let resp = reqwest::get(format!("http://{addr}{path}")).await?;
    let status = resp.status();
    let body = resp.text().await?;

    cancel.cancel();
    task.await??;
    Ok((status, body))
}

async fn get_index(api: reqwest::Client) -> Result<(StatusCode, String)> {
    get_page(api, "/").await
}

/// Insert `n` entries titled "Entry 1" through "Entry n", each
/// published a day after the one before.
fn insert_entries(tc: &crate::test::TestConfig, n: i64) -> Result<()> {
    let conn = tc.database_conn()?;
    for i in 1..=n {
        conn.execute(
            "INSERT INTO entries (syndication_format, guid, published_at, title, url)
             VALUES ('rss', ?1, ?2, ?3, ?4)",
            rusqlite::params![
                format!("guid-{i}"),
                1_700_000_000 + i * 86_400,
                format!("Entry {i}"),
                format!("http://example.com/{i}"),
            ],
        )?;
    }
    Ok(())
}

/// Titles of the entries listed on `body`, in order.
fn listed_titles(body: &str) -> Vec<String> {
    let re = regex::Regex::new(
        r#"<li(?: class="swipe-read" data-entry="\d+")?><a href="[^"]*">([^<]*)</a>"#,
    )
    .unwrap();
    re.captures_iter(body).map(|c| c[1].to_owned()).collect()
}

#[tokio::test]
async fn the_index_page_says_when_there_are_no_entries() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    let (status, body) = get_index(tc.client()?).await?;

    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("0 unread entries"), "{body}");
    assert!(body.contains("No unread entries."), "{body}");
    assert!(body.contains("Page 1 of 1"), "{body}");
    assert!(!body.contains("{{content}}"), "{body}");
    Ok(())
}

/// The header shows Kiki's version.
#[tokio::test]
async fn the_index_page_shows_the_version() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    let (_, body) = get_index(tc.client()?).await?;

    let version = format!(
        r#"<small class="version">v{}</small>"#,
        env!("CARGO_PKG_VERSION")
    );
    assert!(body.contains(&version), "{body}");
    assert!(!body.contains("{{version}}"), "{body}");
    Ok(())
}

/// The index lists the newest entries first, with the total count, and
/// pages through the rest.
#[tokio::test]
async fn the_index_page_lists_entries_newest_first() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    insert_entries(&tc, 30)?;

    let (status, body) = get_index(tc.client()?).await?;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("30 unread entries"), "{body}");
    assert!(body.contains("Page 1 of 2"), "{body}");
    assert!(body.contains(r#"href="/?page=2""#), "{body}");
    assert!(!body.contains(r#"rel="prev""#), "{body}");
    let expected: Vec<_> = (6..=30).rev().map(|i| format!("Entry {i}")).collect();
    assert_eq!(listed_titles(&body), expected);

    let (status, body) = get_page(tc.client()?, "/?page=2").await?;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Page 2 of 2"), "{body}");
    assert!(body.contains(r#"href="/?page=1""#), "{body}");
    assert!(!body.contains(r#"rel="next""#), "{body}");
    let expected: Vec<_> = (1..=5).rev().map(|i| format!("Entry {i}")).collect();
    assert_eq!(listed_titles(&body), expected);
    Ok(())
}

/// Entries a plugin, or the user, hid are left out of the list.
#[tokio::test]
async fn the_index_page_leaves_out_hidden_entries() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    insert_entries(&tc, 3)?;
    tag_entry(&tc, 2, "system:hidden")?;

    let (status, body) = get_index(tc.client()?).await?;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("2 unread entries"), "{body}");
    assert_eq!(listed_titles(&body), ["Entry 3", "Entry 1"]);

    // Its own page still shows it, tagged hidden.
    let (status, body) = get_page(tc.client()?, "/entries/2").await?;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains(r#"<li class="tag system" title="system:hidden">hidden</li>"#),
        "{body}"
    );
    Ok(())
}

/// Read entries are left out of the index, unless the filter menu's
/// checkbox asks for them.
#[tokio::test]
async fn the_index_page_hides_read_entries_by_default() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    insert_entries(&tc, 3)?;
    tag_entry(&tc, 2, "system:read")?;

    let (status, body) = get_index(tc.client()?).await?;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("2 unread entries"), "{body}");
    assert_eq!(listed_titles(&body), ["Entry 3", "Entry 1"]);
    assert!(
        body.contains(
            r#"<input type="checkbox" class="filter-toggle" data-href="/?show_read=true"> Show read entries</label>"#
        ),
        "{body}"
    );

    let (status, body) = get_page(tc.client()?, "/?show_read=true").await?;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("3 entries"), "{body}");
    assert_eq!(listed_titles(&body), ["Entry 3", "Entry 2", "Entry 1"]);
    // Unticking the checkbox hides them again.
    assert!(
        body.contains(
            r#"<input type="checkbox" class="filter-toggle" data-href="/" checked> Show read entries</label>"#
        ),
        "{body}"
    );
    Ok(())
}

/// With read entries shown, the page links, entry links, and the links
/// back from entry pages keep showing them.
#[tokio::test]
async fn showing_read_entries_carries_through_links() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    insert_entries(&tc, 30)?;
    for id in 1..=30 {
        tag_entry(&tc, id, "system:read")?;
    }

    let (_, body) = get_index(tc.client()?).await?;
    assert!(body.contains("No unread entries."), "{body}");

    let (_, body) = get_page(tc.client()?, "/?show_read=true").await?;
    assert!(body.contains("Page 1 of 2"), "{body}");
    assert!(
        body.contains(r#"href="/?page=2&amp;show_read=true" rel="next""#),
        "{body}"
    );

    let (_, body) = get_page(tc.client()?, "/?page=2&show_read=true").await?;
    assert!(
        body.contains(r#"href="/?page=1&amp;show_read=true" rel="prev""#),
        "{body}"
    );
    assert!(
        body.contains(r#"href="/entries/5?page=2&amp;show_read=true""#),
        "{body}"
    );
    // The checkbox goes back to the first page of unread entries.
    assert!(body.contains(r#"data-href="/" checked"#), "{body}");

    let (_, body) = get_page(tc.client()?, "/entries/5?page=2&show_read=true").await?;
    assert!(
        body.contains(r#"<a href="/?page=2&amp;show_read=true">"#),
        "{body}"
    );
    Ok(())
}

#[tokio::test]
async fn a_page_past_the_end_links_back_to_the_last_page() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    insert_entries(&tc, 3)?;

    let (status, body) = get_page(tc.client()?, "/?page=9").await?;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("No entries on this page."), "{body}");
    assert!(body.contains(r#"href="/?page=1" rel="prev""#), "{body}");
    Ok(())
}

/// Entry titles and URLs come from feeds, so they are escaped, and
/// only `http(s)` URLs are linked.
#[tokio::test]
async fn entries_are_rendered_safely() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    let conn = tc.database_conn()?;
    conn.execute(
        "INSERT INTO entries (syndication_format, guid, published_at, title, url)
         VALUES ('rss', 'a', 1, '<script>alert(1)</script>', 'javascript:alert(1)'),
                ('rss', 'b', 2, 'Quotes', 'http://example.com/?a=\"><b>')",
        [],
    )?;

    let (_, body) = get_index(tc.client()?).await?;
    assert!(!body.contains("<script>alert"), "{body}");
    assert!(
        body.contains("&lt;script&gt;alert(1)&lt;/script&gt;"),
        "{body}"
    );
    assert!(!body.contains("javascript:"), "{body}");
    assert!(!body.contains(r#""><b>"#), "{body}");
    Ok(())
}

/// Each entry on the index names the feed it came from, and links to
/// its own page rather than straight to the entry's URL.
#[tokio::test]
async fn the_index_page_shows_each_entrys_feed() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    let conn = tc.database_conn()?;
    conn.execute(
        "INSERT INTO feeds (id, title, url)
         VALUES (1, 'Feed <One>', 'http://example.com/1.xml'),
                (2, 'Feed Two', 'http://example.com/2.xml')",
        [],
    )?;
    conn.execute(
        "INSERT INTO entries (id, feed_id, syndication_format, guid, published_at, title, url)
         VALUES (1, 1, 'rss', 'a', 1, 'From one', 'http://example.com/a'),
                (2, 2, 'rss', 'b', 2, 'From two', 'http://example.com/b'),
                (3, NULL, 'rss', 'c', 3, 'Orphan', 'http://example.com/c')",
        [],
    )?;

    let (status, body) = get_index(tc.client()?).await?;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains(r#"<a href="/entries/1">From one</a>"#),
        "{body}"
    );
    assert!(
        body.contains(r#"<span class="feed">Feed &lt;One&gt;</span>"#),
        "{body}"
    );
    assert!(
        body.contains(r#"<span class="feed">Feed Two</span>"#),
        "{body}"
    );
    assert!(!body.contains("http://example.com/a"), "{body}");
    assert_eq!(body.matches(r#"class="feed""#).count(), 2, "{body}");
    Ok(())
}

/// Attach the tag named `name`, creating it as a user tag if there is
/// no such tag, to entry `entry_id`.
fn tag_entry(tc: &crate::test::TestConfig, entry_id: i64, name: &str) -> Result<()> {
    let conn = tc.database_conn()?;
    conn.execute("INSERT OR IGNORE INTO tags (name) VALUES (?1)", [name])?;
    conn.execute(
        "INSERT INTO entry_tags (entry_id, tag_id)
         SELECT ?1, id FROM tags WHERE name = ?2",
        rusqlite::params![entry_id, name],
    )?;
    Ok(())
}

/// Entries on the index show their tags, system tags first and without
/// their prefix, with user tag names escaped.
#[tokio::test]
async fn the_index_page_shows_each_entrys_tags() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    insert_entries(&tc, 2)?;
    tag_entry(&tc, 1, "<b>news</b>")?;
    tag_entry(&tc, 1, "system:read")?;
    tag_entry(&tc, 1, "system:saved")?;

    // Entry 1 is read, so only shows when read entries are.
    let (status, body) = get_page(tc.client()?, "/?show_read=true").await?;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains(
            r#"<ul class="tags" aria-label="Tags"><li class="tag system" title="system:read">read</li><li class="tag system" title="system:saved">saved</li><li class="tag">&lt;b&gt;news&lt;/b&gt;</li></ul>"#
        ),
        "{body}"
    );
    assert!(!body.contains("<b>news"), "{body}");
    // Entry 2 has no tags, so only entry 1 gets a list.
    assert_eq!(body.matches(r#"class="tags""#).count(), 1, "{body}");
    // The tags don't disturb the list of entries.
    assert_eq!(listed_titles(&body), ["Entry 2", "Entry 1"]);
    Ok(())
}

/// An entry's page, and its feed's page, show the entry's tags.
#[tokio::test]
async fn entry_and_feed_pages_show_tags() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    let conn = tc.database_conn()?;
    conn.execute(
        "INSERT INTO feeds (id, title, url) VALUES (1, 'Feed', 'http://example.com/feed.xml')",
        [],
    )?;
    conn.execute(
        "INSERT INTO entries (id, feed_id, syndication_format, guid, published_at, title, url)
         VALUES (3, 1, 'rss', 'a', 1, 'Tagged', 'http://example.com/a')",
        [],
    )?;
    tag_entry(&tc, 3, "tech")?;
    tag_entry(&tc, 3, "system:saved")?;
    let expected = r#"<ul class="tags" aria-label="Tags"><li class="tag system" title="system:saved">saved</li><li class="tag">tech</li></ul>"#;

    let (status, body) = get_page(tc.client()?, "/entries/3").await?;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(expected), "{body}");

    let (status, body) = get_page(tc.client()?, "/feeds/1").await?;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(expected), "{body}");
    Ok(())
}

/// Every page links to the list of tags, which links to each tag's page.
#[tokio::test]
async fn the_tags_page_lists_every_tag() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    insert_entries(&tc, 1)?;
    tag_entry(&tc, 1, "<b>news</b>")?;

    let (_, body) = get_index(tc.client()?).await?;
    assert!(body.contains(r#"<a href="/tags">Tags</a>"#), "{body}");

    let (status, body) = get_page(tc.client()?, "/tags").await?;
    assert_eq!(status, StatusCode::OK);
    let id: i64 = tc.database_conn()?.query_row(
        "SELECT id FROM tags WHERE name = '<b>news</b>'",
        [],
        |row| row.get(0),
    )?;
    assert!(
        body.contains(&format!(
            r#"<li><a href="/tags/{id}" class="tag">&lt;b&gt;news&lt;/b&gt;</a></li>"#
        )),
        "{body}"
    );
    let saved: i64 = tc.database_conn()?.query_row(
        "SELECT id FROM tags WHERE name = 'system:saved'",
        [],
        |row| row.get(0),
    )?;
    assert!(
        body.contains(&format!(
            r#"<li><a href="/tags/{saved}" class="tag system" title="system:saved">saved</a></li>"#
        )),
        "{body}"
    );
    Ok(())
}

/// A tag's page lists the unread entries with the tag, and its entries
/// link back to it.
#[tokio::test]
async fn a_tags_page_lists_its_entries() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    insert_entries(&tc, 4)?;
    tag_entry(&tc, 1, "tech")?;
    tag_entry(&tc, 2, "tech")?;
    tag_entry(&tc, 3, "tech")?;
    tag_entry(&tc, 3, "system:read")?;
    let id: i64 =
        tc.database_conn()?
            .query_row("SELECT id FROM tags WHERE name = 'tech'", [], |row| {
                row.get(0)
            })?;

    let (status, body) = get_page(tc.client()?, &format!("/tags/{id}")).await?;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("<title>tech - Kiki</title>"), "{body}");
    assert_eq!(listed_titles(&body), ["Entry 2", "Entry 1"]);
    assert!(
        body.contains(&format!(r#"href="/entries/2?tag={id}""#)),
        "{body}"
    );
    // Entries can't be marked as read by tag.
    assert!(!body.contains(r#"class="mark-read""#), "{body}");

    let (_, body) = get_page(tc.client()?, &format!("/tags/{id}?show_read=true")).await?;
    assert_eq!(listed_titles(&body), ["Entry 3", "Entry 2", "Entry 1"]);

    let (_, body) = get_page(tc.client()?, &format!("/entries/2?tag={id}")).await?;
    assert!(
        body.contains(&format!(r#"<a href="/tags/{id}">&larr; Back to tag</a>"#)),
        "{body}"
    );

    let (status, _) = get_page(tc.client()?, "/tags/999").await?;
    assert_eq!(status, StatusCode::NOT_FOUND);
    Ok(())
}

/// A user tag's page has a button that deletes the tag; a system tag's
/// page has none.
#[tokio::test]
async fn only_user_tag_pages_have_a_delete_button() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    insert_entries(&tc, 1)?;
    tag_entry(&tc, 1, "<b>news</b>")?;
    let conn = tc.database_conn()?;
    let id: i64 = conn.query_row(
        "SELECT id FROM tags WHERE name = '<b>news</b>'",
        [],
        |row| row.get(0),
    )?;

    let (status, body) = get_page(tc.client()?, &format!("/tags/{id}")).await?;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains(&format!(
            r#"<button type="button" class="delete-tag" data-tag="{id}" data-name="&lt;b&gt;news&lt;/b&gt;">Delete tag</button>"#
        )),
        "{body}"
    );

    for tag in SystemTag::ALL {
        let id = tag.id(&conn)?;
        let (status, body) = get_page(tc.client()?, &format!("/tags/{id}")).await?;
        assert_eq!(status, StatusCode::OK);
        assert!(!body.contains(r#"class="delete-tag""#), "{body}");
    }
    Ok(())
}

/// `DELETE /tags/{id}` deletes a user tag, but not a system tag;
/// requests from other sites are refused.
#[tokio::test]
async fn user_tags_can_be_deleted() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    insert_entries(&tc, 1)?;
    tag_entry(&tc, 1, "tech")?;
    let conn = tc.database_conn()?;
    let tag_exists = |name: &str| -> Result<bool> {
        Ok(conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM tags WHERE name = ?1)",
            [name],
            |row| row.get(0),
        )?)
    };
    let id: i64 = conn.query_row("SELECT id FROM tags WHERE name = 'tech'", [], |row| {
        row.get(0)
    })?;
    let path = format!("/tags/{id}");
    let delete = reqwest::Method::DELETE;

    for headers in [
        &[("Sec-Fetch-Site", "cross-site")][..],
        &[("Origin", "http://evil.example")],
    ] {
        let status = send_request(tc.client()?, delete.clone(), &path, headers).await?;
        assert_eq!(status, StatusCode::FORBIDDEN, "{headers:?}");
    }
    assert!(tag_exists("tech")?);

    let same_origin = [("Sec-Fetch-Site", "same-origin")];
    for tag in SystemTag::ALL {
        let path = format!("/tags/{}", tag.id(&conn)?);
        let status = send_request(tc.client()?, delete.clone(), &path, &same_origin).await?;
        assert_eq!(status, StatusCode::FORBIDDEN, "{tag}");
        assert!(tag_exists(tag.name())?);
    }

    let status = send_request(tc.client()?, delete.clone(), &path, &same_origin).await?;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(!tag_exists("tech")?);

    let status = send_request(tc.client()?, delete, &path, &same_origin).await?;
    assert_eq!(status, StatusCode::NOT_FOUND);
    Ok(())
}

/// The pages of the `system:read` and `system:hidden` tags list the
/// entries that other lists leave out.
#[tokio::test]
async fn read_and_hidden_tag_pages_list_their_entries() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    insert_entries(&tc, 2)?;
    tag_entry(&tc, 1, "system:read")?;
    tag_entry(&tc, 2, "system:hidden")?;
    let tag_id = |name: &str| -> Result<i64> {
        Ok(tc
            .database_conn()?
            .query_row("SELECT id FROM tags WHERE name = ?1", [name], |row| {
                row.get(0)
            })?)
    };

    let read = tag_id("system:read")?;
    let (_, body) = get_page(tc.client()?, &format!("/tags/{read}")).await?;
    assert_eq!(listed_titles(&body), ["Entry 1"]);

    let hidden = tag_id("system:hidden")?;
    let (_, body) = get_page(tc.client()?, &format!("/tags/{hidden}")).await?;
    assert_eq!(listed_titles(&body), ["Entry 2"]);
    Ok(())
}

/// Every entry, in a list or on its own page, has a save button that
/// shows whether the entry is saved.
#[tokio::test]
async fn entries_have_save_buttons() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    let conn = tc.database_conn()?;
    conn.execute(
        "INSERT INTO feeds (id, title, url) VALUES (1, 'Feed', 'http://example.com/feed.xml')",
        [],
    )?;
    conn.execute(
        "INSERT INTO entries (id, feed_id, syndication_format, guid, published_at, title, url)
         VALUES (1, 1, 'rss', 'a', 1, 'Saved', 'http://example.com/a'),
                (2, 1, 'rss', 'b', 2, 'Unsaved', 'http://example.com/b')",
        [],
    )?;
    tag_entry(&tc, 1, "system:saved")?;
    let saved = r#"<button type="button" class="save" data-entry="1" aria-pressed="true" aria-label="Save" title="Unsave">"#;
    let unsaved = r#"<button type="button" class="save" data-entry="2" aria-pressed="false" aria-label="Save" title="Save">"#;

    for path in ["/", "/feeds/1"] {
        let (status, body) = get_page(tc.client()?, path).await?;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(saved), "{path}: {body}");
        assert!(body.contains(unsaved), "{path}: {body}");
        assert_eq!(listed_titles(&body), ["Unsaved", "Saved"], "{path}");
    }

    let (_, body) = get_page(tc.client()?, "/entries/1").await?;
    assert!(body.contains(saved), "{body}");
    let (_, body) = get_page(tc.client()?, "/entries/2").await?;
    assert!(body.contains(unsaved), "{body}");
    Ok(())
}

/// Send a `method` request for `path` to the web UI, with `headers`, and
/// return the response's status.
async fn send_request(
    api: reqwest::Client,
    method: reqwest::Method,
    path: &str,
    headers: &[(&str, &str)],
) -> Result<StatusCode> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let cancel = CancellationToken::new();
    let task = tokio::spawn(serve_ui(
        listener,
        api,
        AllowedHosts::default(),
        cancel.clone(),
    ));

    let mut req = reqwest::Client::new().request(method, format!("http://{addr}{path}"));
    for (name, value) in headers {
        req = req.header(*name, *value);
    }
    let status = req.send().await?.status();

    cancel.cancel();
    task.await??;
    Ok(status)
}

/// Whether entry `id` has the `system:saved` tag.
fn is_saved(tc: &crate::test::TestConfig, id: i64) -> Result<bool> {
    Ok(tc.database_conn()?.query_row(
        "SELECT EXISTS(SELECT 1 FROM entry_tags et JOIN tags t ON t.id = et.tag_id
         WHERE et.entry_id = ?1 AND t.name = 'system:saved')",
        [id],
        |row| row.get(0),
    )?)
}

/// `PUT /entries/{id}/system-tags/saved` saves an entry and `DELETE`
/// unsaves it, each any number of times; an unknown entry is not found.
#[tokio::test]
async fn entries_can_be_saved_and_unsaved() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    insert_entries(&tc, 1)?;
    let same_origin = [("Sec-Fetch-Site", "same-origin")];

    for _ in 0..2 {
        let status = send_request(
            tc.client()?,
            reqwest::Method::PUT,
            "/entries/1/system-tags/saved",
            &same_origin,
        )
        .await?;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(is_saved(&tc, 1)?);
    }
    for _ in 0..2 {
        let status = send_request(
            tc.client()?,
            reqwest::Method::DELETE,
            "/entries/1/system-tags/saved",
            &same_origin,
        )
        .await?;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(!is_saved(&tc, 1)?);
    }

    for method in [reqwest::Method::PUT, reqwest::Method::DELETE] {
        let status = send_request(
            tc.client()?,
            method,
            "/entries/99/system-tags/saved",
            &same_origin,
        )
        .await?;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
    Ok(())
}

/// Requests to save or unsave an entry from other sites are refused.
#[tokio::test]
async fn cross_site_saves_are_refused() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    insert_entries(&tc, 2)?;
    tag_entry(&tc, 2, "system:saved")?;

    for headers in [
        &[("Sec-Fetch-Site", "cross-site")][..],
        &[("Sec-Fetch-Site", "same-site")],
        &[("Origin", "http://evil.example")],
    ] {
        for (method, id) in [(reqwest::Method::PUT, 1), (reqwest::Method::DELETE, 2)] {
            let path = format!("/entries/{id}/system-tags/saved");
            let status = send_request(tc.client()?, method, &path, headers).await?;
            assert_eq!(status, StatusCode::FORBIDDEN, "{path} {headers:?}");
        }
    }
    assert!(!is_saved(&tc, 1)?);
    assert!(is_saved(&tc, 2)?);
    Ok(())
}

/// `PUT /entries/{id}/system-tags/read` marks an entry as read and
/// `DELETE` marks it unread again, by either of the tag's names. Only
/// system tags can be changed this way: other names, including ones
/// that would reach other API routes, are not found.
#[tokio::test]
async fn entries_can_be_marked_read_and_unread() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    insert_entries(&tc, 2)?;
    let same_origin = [("Sec-Fetch-Site", "same-origin")];

    for name in ["read", "system:read"] {
        let path = format!("/entries/2/system-tags/{name}");
        let status = send_request(tc.client()?, reqwest::Method::PUT, &path, &same_origin).await?;
        assert_eq!(status, StatusCode::NO_CONTENT, "{path}");
        assert_eq!(read_entries(&tc)?, [2], "{path}");
        let status =
            send_request(tc.client()?, reqwest::Method::DELETE, &path, &same_origin).await?;
        assert_eq!(status, StatusCode::NO_CONTENT, "{path}");
        assert!(read_entries(&tc)?.is_empty(), "{path}");
    }

    tag_entry(&tc, 1, "news")?;
    for path in [
        "/entries/1/system-tags/news",
        "/entries/1/system-tags/starred",
        "/entries/1/system-tags/..%2F..%2F..%2Ftags%2Fid%2F1",
    ] {
        for method in [reqwest::Method::PUT, reqwest::Method::DELETE] {
            let status = send_request(tc.client()?, method, path, &same_origin).await?;
            assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        }
    }
    assert_eq!(
        tc.database_conn()?.query_row(
            "SELECT COUNT(*) FROM tags WHERE name = 'news'",
            [],
            |row| { row.get::<_, i64>(0) }
        )?,
        1
    );
    Ok(())
}

/// Unread entries can be swiped away where read entries are left out:
/// not once read entries are shown, nor in search results.
#[tokio::test]
async fn unread_entries_can_be_swiped_where_read_ones_are_hidden() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    insert_entries(&tc, 2)?;
    tag_entry(&tc, 2, "system:read")?;
    let swipeable = |id| format!(r#"<li class="swipe-read" data-entry="{id}">"#);

    let (_, body) = get_index(tc.client()?).await?;
    assert!(body.contains(&swipeable(1)), "{body}");
    let (_, body) = get_page(tc.client()?, "/?show_read=true").await?;
    assert_eq!(listed_titles(&body).len(), 2, "{body}");
    assert!(!body.contains(r#"class="swipe-read""#), "{body}");
    let (_, body) = get_page(tc.client()?, "/search?q=entry").await?;
    assert_eq!(listed_titles(&body).len(), 2, "{body}");
    assert!(!body.contains(r#"class="swipe-read""#), "{body}");
    Ok(())
}

/// IDs of the entries with the `system:read` tag.
fn read_entries(tc: &crate::test::TestConfig) -> Result<Vec<i64>> {
    let conn = tc.database_conn()?;
    let mut stmt = conn.prepare(
        "SELECT et.entry_id FROM entry_tags et JOIN tags t ON t.id = et.tag_id
         WHERE t.name = 'system:read' ORDER BY et.entry_id",
    )?;
    let ids = stmt
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(ids)
}

/// The index and feed pages have a "Mark all as read" button, which
/// marks only the feed's entries on a feed's page. The index leaves it
/// out when there are no entries.
#[tokio::test]
async fn entry_lists_have_a_mark_all_as_read_button() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    let (_, body) = get_index(tc.client()?).await?;
    assert!(!body.contains("class=\"mark-read\""), "{body}");

    insert_entries(&tc, 1)?;
    let (_, body) = get_index(tc.client()?).await?;
    assert!(
        body.contains(
            r#"<button type="button" class="mark-read" title="Mark every entry as read">"#
        ),
        "{body}"
    );

    let conn = tc.database_conn()?;
    conn.execute(
        "INSERT INTO feeds (id, title, url) VALUES (3, 'Feed', 'http://example.com/feed.xml')",
        [],
    )?;
    conn.execute("UPDATE entries SET feed_id = 3", [])?;
    let (_, body) = get_page(tc.client()?, "/feeds/3").await?;
    assert!(
        body.contains(r#"<button type="button" class="mark-read" data-feed="3""#),
        "{body}"
    );
    Ok(())
}

/// `POST /entries/read` marks every entry as read, or with `?feed=`,
/// only that feed's entries; requests from other sites are refused.
#[tokio::test]
async fn entries_can_be_marked_as_read() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    insert_entries(&tc, 3)?;
    let conn = tc.database_conn()?;
    conn.execute(
        "INSERT INTO feeds (id, title, url) VALUES (1, 'Feed', 'http://example.com/feed.xml')",
        [],
    )?;
    conn.execute("UPDATE entries SET feed_id = 1 WHERE id = 2", [])?;
    let post = reqwest::Method::POST;

    for headers in [
        &[("Sec-Fetch-Site", "cross-site")][..],
        &[("Origin", "http://evil.example")],
    ] {
        let status = send_request(tc.client()?, post.clone(), "/entries/read", headers).await?;
        assert_eq!(status, StatusCode::FORBIDDEN, "{headers:?}");
    }
    assert!(read_entries(&tc)?.is_empty());

    let same_origin = [("Sec-Fetch-Site", "same-origin")];
    let status = send_request(
        tc.client()?,
        post.clone(),
        "/entries/read?feed=1",
        &same_origin,
    )
    .await?;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(read_entries(&tc)?, [2]);

    let status = send_request(tc.client()?, post, "/entries/read", &same_origin).await?;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(read_entries(&tc)?, [1, 2, 3]);
    Ok(())
}

/// What is typed into the search box becomes an FTS5 query that
/// cannot be a syntax error: every word is quoted, phrases are kept
/// together, and a trailing `*` matches a prefix.
#[test]
fn search_input_becomes_a_safe_fts_query() {
    let cases = [
        ("rust", Some(r#""rust""#)),
        ("  rust   release ", Some(r#""rust" "release""#)),
        (r#""rust release" notes"#, Some(r#""rust release" "notes""#)),
        ("rel* \"rust rel\"*", Some(r#""rel"* "rust rel"*"#)),
        ("don't", Some(r#""don't""#)),
        ("rust OR go NOT c", Some(r#""rust" "OR" "go" "NOT" "c""#)),
        (r#"unclosed "quote"#, Some(r#""unclosed" "quote""#)),
        (r#"a"b"#, Some(r#""a" "b""#)),
        ("c++ -x", Some(r#""c++" "-x""#)),
        ("", None),
        ("  \"\" * - ", None),
    ];
    for (input, expected) in cases {
        assert_eq!(fts_query(input).as_deref(), expected, "{input}");
    }
}

/// Insert the entries the search tests look for: two about Rust, one
/// matching far better than the other but published earlier, one that
/// does not mention it, and one that does but is hidden. The weaker
/// match is read, which does not keep it out of search results.
fn insert_search_entries(tc: &crate::test::TestConfig) -> Result<()> {
    let conn = tc.database_conn()?;
    for (id, title, content) in [
        (1, "Rust Rust Rust", "All about rust, and more rust."),
        (2, "Weekly notes", "Don't panic: a little rust this week."),
        (3, "Gardening", "Tomatoes."),
        (4, "Hidden rust", "Rust."),
    ] {
        conn.execute(
            "INSERT INTO entries (id, syndication_format, guid, published_at, title, url, content)
             VALUES (?1, 'rss', ?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                id,
                1_700_000_000 + id * 86_400,
                title,
                format!("http://example.com/{id}"),
                content,
            ],
        )?;
    }
    tag_entry(tc, 2, "system:read")?;
    tag_entry(tc, 4, "system:hidden")?;
    Ok(())
}

/// Every page has the search box, empty unless it shows search results.
#[tokio::test]
async fn every_page_has_a_search_box() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    for path in ["/", "/feeds", "/tags", "/plugins"] {
        let (_, body) = get_page(tc.client()?, path).await?;
        assert!(
            body.contains(r#"<input type="search" name="q" value="""#),
            "{path}: {body}"
        );
    }
    Ok(())
}

/// On narrow screens the nav has a button that opens the search box in
/// a popup instead; the popup's box is filled in with the search too.
#[tokio::test]
async fn the_search_box_has_a_popup_for_narrow_screens() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    let (_, body) = get_page(tc.client()?, "/search?q=rust").await?;
    assert!(
        body.contains(r#"<button type="button" class="search-open""#),
        "{body}"
    );
    assert!(body.contains(r#"<dialog class="search-dialog""#), "{body}");
    assert_eq!(
        body.matches(r#"name="q" value="rust""#).count(),
        2,
        "{body}"
    );
    Ok(())
}

/// The search page lists the entries matching the search, best match
/// first, read ones included and hidden ones left out.
#[tokio::test]
async fn the_search_page_lists_matching_entries() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    insert_search_entries(&tc)?;

    let (status, body) = get_page(tc.client()?, "/search?q=rust").await?;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("<title>rust - Search - Kiki</title>"),
        "{body}"
    );
    assert!(body.contains("2 results"), "{body}");
    assert!(body.contains(r#"value="rust""#), "{body}");
    assert_eq!(listed_titles(&body), ["Rust Rust Rust", "Weekly notes"]);
    assert!(
        body.contains(r#"<a href="/search?q=rust&amp;sort=newest">newest</a>"#),
        "{body}"
    );
    assert!(!body.contains(r#"class="mark-read""#), "{body}");
    assert!(!body.contains(r#"class="filter-toggle""#), "{body}");

    let (_, body) = get_page(tc.client()?, "/search?q=rust&sort=newest").await?;
    assert_eq!(listed_titles(&body), ["Weekly notes", "Rust Rust Rust"]);
    assert!(
        body.contains(r#"<a href="/search?q=rust">best match</a>"#),
        "{body}"
    );

    let (_, body) = get_page(tc.client()?, "/search?q=tomatoes%20rust").await?;
    assert!(body.contains("0 results"), "{body}");
    assert!(body.contains("No entries match your search."), "{body}");
    Ok(())
}

/// Searches that FTS5 would reject as syntax errors are searched for
/// word by word, rather than failing.
#[tokio::test]
async fn searches_with_stray_punctuation_still_work() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    insert_search_entries(&tc)?;

    let (status, body) = get_page(tc.client()?, "/search?q=don%27t%20%22panic").await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed_titles(&body), ["Weekly notes"]);

    let (status, body) = get_page(tc.client()?, "/search?q=-%20%2A").await?;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("No entries match your search."), "{body}");
    Ok(())
}

/// Without anything to search for, the search page says how searching
/// works rather than listing entries.
#[tokio::test]
async fn an_empty_search_lists_nothing() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    insert_search_entries(&tc)?;

    for path in ["/search", "/search?q=%20%20"] {
        let (status, body) = get_page(tc.client()?, path).await?;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("<h2>Search</h2>"), "{body}");
        assert!(listed_titles(&body).is_empty(), "{body}");
    }
    Ok(())
}

/// What was searched for is escaped wherever the page shows it.
#[tokio::test]
async fn searches_are_rendered_safely() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    let (_, body) = get_page(tc.client()?, "/search?q=%22%3E%3Cb%3Ex").await?;
    assert!(!body.contains("<b>x"), "{body}");
    assert!(body.contains(r#"value="&quot;&gt;&lt;b&gt;x""#), "{body}");
    assert!(
        body.contains("Search results for &ldquo;&quot;&gt;&lt;b&gt;x&rdquo;"),
        "{body}"
    );
    Ok(())
}

/// Search results link to entry pages that link back to them, sorted
/// and paged as they were.
#[tokio::test]
async fn entry_pages_link_back_to_search_results() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    insert_search_entries(&tc)?;

    let (_, body) = get_page(tc.client()?, "/search?q=rust%20all&sort=newest").await?;
    let href = r#"href="/entries/1?q=rust%20all&amp;sort=newest""#;
    assert!(body.contains(href), "{body}");

    let (status, body) =
        get_page(tc.client()?, "/entries/1?q=rust%20all&sort=newest&page=2").await?;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains(
            r#"<a href="/search?q=rust%20all&amp;sort=newest&amp;page=2">&larr; Back to search results</a>"#
        ),
        "{body}"
    );
    Ok(())
}

/// On later pages of the index, entries link to pages that link back.
#[tokio::test]
async fn entry_pages_link_back_to_the_index_page() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    insert_entries(&tc, 30)?;

    let (_, body) = get_page(tc.client()?, "/?page=2").await?;
    assert!(body.contains(r#"href="/entries/5?page=2""#), "{body}");

    let (status, body) = get_page(tc.client()?, "/entries/5?page=2").await?;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(r#"<a href="/?page=2">"#), "{body}");
    Ok(())
}

/// An entry's page summarizes it from the feed's data: its title, date,
/// feed, author, categories and content, with a link through to it.
#[tokio::test]
async fn the_entry_page_summarizes_the_entry() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    let conn = tc.database_conn()?;
    conn.execute(
        "INSERT INTO feeds (id, title, url) VALUES (1, 'Example Feed', 'http://example.com/feed.xml')",
        [],
    )?;
    conn.execute(
        "INSERT INTO entries (id, feed_id, syndication_format, guid, published_at, title, url, content)
         VALUES (7, 1, 'rss', 'a', 1700000000, 'An <Entry>', 'http://example.com/posts/a',
                 '<p onclick=\"x()\">Hello <a href=\"/about\">there</a></p><script>alert(1)</script>')",
        [],
    )?;
    conn.execute(
        "INSERT INTO rss_entry_data (entry_id, author, comments)
         VALUES (7, 'Ann Author', 'http://example.com/posts/a#comments')",
        [],
    )?;
    conn.execute(
        "INSERT INTO rss_categories (entry_id, category) VALUES (7, 'news')",
        [],
    )?;

    let (status, body) = get_page(tc.client()?, "/entries/7").await?;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("<title>An &lt;Entry&gt; - Kiki</title>"),
        "{body}"
    );
    assert!(body.contains("<h2>An &lt;Entry&gt;</h2>"), "{body}");
    assert!(body.contains("2023-11-14"), "{body}");
    assert!(
        body.contains(r#"<span class="feed">Example Feed</span>"#),
        "{body}"
    );
    assert!(body.contains("by Ann Author"), "{body}");
    assert!(body.contains("Filed under news"), "{body}");
    assert!(
        body.contains(
            r#"<p>Hello <a href="http://example.com/about" rel="noopener noreferrer nofollow">there</a></p>"#
        ),
        "{body}"
    );
    // The page's only script is its own.
    assert_eq!(body.matches("<script>").count(), 1, "{body}");
    assert!(!body.contains("alert(1)"), "{body}");
    assert!(!body.contains("onclick"), "{body}");
    assert!(
        body.contains(
            r#"<a href="http://example.com/posts/a" rel="noopener noreferrer">Read the full entry"#
        ),
        "{body}"
    );
    assert!(
        body.contains(r#"<a href="http://example.com/posts/a#comments" rel="noopener noreferrer">Comments</a>"#),
        "{body}"
    );
    assert!(
        body.contains(r#"<a href="/">&larr; Back to entries</a>"#),
        "{body}"
    );
    Ok(())
}

/// Cache `bytes` as the asset at `original_url`, of type `content_type`,
/// for entry `entry_id` as an asset of `kind`, and return its hash.
fn cache_asset(
    tc: &crate::test::TestConfig,
    entry_id: i64,
    bytes: &[u8],
    original_url: &str,
    content_type: &str,
    kind: &str,
) -> Result<String> {
    let conn = tc.database_conn()?;
    let hash = blake3::hash(bytes).to_hex().to_string();
    let path = crate::tasks::assets::asset_path(tc.config_dir(), &hash);
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(&path, bytes)?;
    let asset_id = crate::db::assets::insert_asset(
        &conn,
        &hash,
        original_url,
        Some(content_type),
        bytes.len() as i64,
        None,
        None,
    )?;
    crate::db::assets::link_entry_asset(&conn, entry_id, asset_id, kind)?;
    Ok(hash)
}

/// An entry's attachment links to its cached copy when there is one, and
/// to where the feed says it is when there isn't.
#[tokio::test]
async fn attachments_link_to_the_asset_cache() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    let conn = tc.database_conn()?;
    conn.execute(
        "INSERT INTO entries (id, syndication_format, guid, published_at, title, url)
         VALUES (1, 'rss', 'a', 1, 'Cached', 'http://example.com/a'),
                (2, 'rss', 'b', 2, 'Uncached', 'http://example.com/b')",
        [],
    )?;
    conn.execute(
        "INSERT INTO rss_entry_data (entry_id, enclosure_url, enclosure_mime_type)
         VALUES (1, 'HTTP://Example.com/episode.mp3', 'audio/mpeg'),
                (2, 'http://example.com/other.mp3', 'audio/mpeg')",
        [],
    )?;
    let hash = cache_asset(
        &tc,
        1,
        b"episode",
        "http://example.com/episode.mp3",
        "audio/mpeg",
        "enclosure",
    )?;

    let (_, body) = get_page(tc.client()?, "/entries/1").await?;
    assert!(
        body.contains(&format!(
            r#"<a href="/assets/{hash}" rel="noopener noreferrer">Attachment (audio/mpeg)</a>"#
        )),
        "{body}"
    );
    assert!(!body.contains("episode.mp3"), "{body}");

    let (_, body) = get_page(tc.client()?, "/entries/2").await?;
    assert!(
        body.contains(
            r#"<a href="http://example.com/other.mp3" rel="noopener noreferrer">Attachment"#
        ),
        "{body}"
    );
    Ok(())
}

/// Cached media is shown in the browser; anything else is downloaded
/// rather than rendered from the web UI's origin.
#[tokio::test]
async fn only_media_assets_are_shown_inline() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    tc.database_conn()?.execute(
        "INSERT INTO entries (id, syndication_format, guid, published_at, title, url)
         VALUES (1, 'rss', 'a', 1, 'Entry', 'http://example.com/a')",
        [],
    )?;
    let cases = [
        ("image/png", "inline"),
        ("audio/mpeg", "inline"),
        ("video/mp4", "inline"),
        ("text/html", "attachment"),
        ("application/pdf", "attachment"),
    ];
    let mut hashes = Vec::new();
    for (i, (content_type, _)) in cases.iter().enumerate() {
        hashes.push(cache_asset(
            &tc,
            1,
            format!("asset {i}").as_bytes(),
            &format!("http://example.com/{i}"),
            content_type,
            "enclosure",
        )?);
    }

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let cancel = CancellationToken::new();
    let task = tokio::spawn(serve_ui(
        listener,
        tc.client()?,
        AllowedHosts::default(),
        cancel.clone(),
    ));

    for ((content_type, disposition), hash) in cases.iter().zip(&hashes) {
        let resp = reqwest::get(format!("http://{addr}/assets/{hash}")).await?;
        assert_eq!(
            resp.headers()[header::CONTENT_DISPOSITION],
            *disposition,
            "{content_type}"
        );
    }

    cancel.cancel();
    task.await??;
    Ok(())
}

/// Cached images are shown from the web UI's asset route, which serves
/// the bytes from the cache; images that aren't cached become links.
#[tokio::test]
async fn entry_images_are_served_from_the_asset_cache() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    let conn = tc.database_conn()?;
    conn.execute(
        "INSERT INTO entries (id, syndication_format, guid, published_at, title, url, content)
         VALUES (1, 'rss', 'a', 1, 'Pictures', 'http://example.com/posts/a',
                 '<img src=\"cached.png\" alt=\"Cached\"><img src=\"http://example.com/missing.png\">')",
        [],
    )?;
    let bytes = b"not really a png";
    let hash = cache_asset(
        &tc,
        1,
        bytes,
        "http://example.com/posts/cached.png",
        "image/png",
        "inline_img",
    )?;

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let cancel = CancellationToken::new();
    let task = tokio::spawn(serve_ui(
        listener,
        tc.client()?,
        AllowedHosts::default(),
        cancel.clone(),
    ));

    let resp = reqwest::get(format!("http://{addr}/entries/1")).await?;
    let csp = resp.headers()[header::CONTENT_SECURITY_POLICY].to_str()?;
    assert!(csp.contains("img-src 'self';"), "{csp}");
    let body = resp.text().await?;
    assert!(
        body.contains(&format!(
            r#"<img src="/assets/{hash}" alt="Cached" loading="lazy">"#
        )),
        "{body}"
    );
    assert!(
        body.contains(r#"<a href="http://example.com/missing.png" rel="noopener noreferrer nofollow">[Image]</a>"#),
        "{body}"
    );

    let resp = reqwest::get(format!("http://{addr}/assets/{hash}")).await?;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()[header::CONTENT_TYPE], "image/png");
    assert_eq!(resp.headers()[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
    assert!(resp.headers()[header::CONTENT_SECURITY_POLICY]
        .to_str()?
        .ends_with("sandbox"));
    assert_eq!(resp.headers()[header::X_DNS_PREFETCH_CONTROL], "off");
    let etag = resp.headers()[header::ETAG].clone();
    assert_eq!(resp.bytes().await?.as_ref(), bytes);

    let resp = reqwest::Client::new()
        .get(format!("http://{addr}/assets/{hash}"))
        .header(header::IF_NONE_MATCH, etag)
        .send()
        .await?;
    assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);

    for path in [
        format!("/assets/{}", "0".repeat(64)),
        "/assets/..%2Fentries".to_owned(),
    ] {
        let resp = reqwest::get(format!("http://{addr}{path}")).await?;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{path}");
    }

    cancel.cancel();
    task.await??;
    Ok(())
}

/// Pages forbid any script but their own, in case anything from a feed
/// slips through, and ask the browser not to look up the hosts they link
/// to.
#[tokio::test]
async fn pages_only_run_their_own_script() -> Result<()> {
    use base64::Engine;
    use sha2::{Digest, Sha256};
    let tc = TestBuilder::all().build()?;
    insert_entries(&tc, 1)?;
    tc.database_conn()?.execute(
        "INSERT INTO feeds (id, title, url) VALUES (1, 'Feed', 'http://example.com/1.xml')",
        [],
    )?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let cancel = CancellationToken::new();
    let task = tokio::spawn(serve_ui(
        listener,
        tc.client()?,
        AllowedHosts::default(),
        cancel.clone(),
    ));

    for path in ["/", "/entries/1", "/feeds", "/feeds/1", "/plugins"] {
        let resp = reqwest::get(format!("http://{addr}{path}")).await?;
        let csp = resp.headers()[header::CONTENT_SECURITY_POLICY]
            .to_str()?
            .to_owned();
        assert!(csp.starts_with("default-src 'none';"), "{path}: {csp}");
        assert!(csp.contains("connect-src 'self';"), "{path}: {csp}");
        assert_eq!(
            resp.headers()[header::X_DNS_PREFETCH_CONTROL],
            "off",
            "{path}"
        );

        // The only script allowed is the one inlined into the page.
        let body = resp.text().await?;
        let script = body
            .split_once("<script>")
            .and_then(|(_, rest)| rest.split_once("</script>"))
            .map(|(script, _)| script)
            .context("page has no script")?;
        let hash = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(script));
        let script_src: Vec<&str> = csp
            .split(';')
            .map(str::trim)
            .filter(|d| d.starts_with("script-src"))
            .collect();
        assert_eq!(
            script_src,
            [format!("script-src 'sha256-{hash}'")],
            "{path}: {csp}"
        );
    }

    cancel.cancel();
    task.await??;
    Ok(())
}

/// Placeholders in a feed's text are shown as they are, not filled in.
#[tokio::test]
async fn placeholders_in_entries_are_not_filled_in() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    tc.database_conn()?.execute(
        "INSERT INTO entries (id, syndication_format, guid, published_at, title, url, content)
         VALUES (1, 'rss', 'a', 1, '{{content}}', 'http://example.com/a', '{{version}}')",
        [],
    )?;

    let (_, body) = get_page(tc.client()?, "/entries/1").await?;
    assert!(body.contains("<h2>{{content}}</h2>"), "{body}");
    assert!(body.contains("{{version}}"), "{body}");
    Ok(())
}

/// Every page links to the index, to the list of feeds and to the list
/// of plugins.
#[tokio::test]
async fn pages_link_to_the_site_sections() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    insert_entries(&tc, 1)?;
    for path in ["/", "/entries/1", "/feeds", "/tags", "/plugins"] {
        let (_, body) = get_page(tc.client()?, path).await?;
        assert!(
            body.contains(r#"<nav class="site-nav"><a href="/feeds">Feeds</a><a href="/tags">Tags</a><a href="/plugins">Plugins</a>"#),
            "{path}: {body}"
        );
    }
    Ok(())
}

/// The list of feeds links each feed to its page. Titles and URLs come
/// from feeds, so they are escaped, and the URLs aren't linked. Only
/// each URL's domain is shown, with the full URL as its tooltip.
#[tokio::test]
async fn the_feeds_page_lists_every_feed() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    tc.database_conn()?.execute(
        "INSERT INTO feeds (id, title, url, last_checked)
         VALUES (1, 'Feed <One>', 'http://example.com/1.xml?a=\"><b>', 1700000000),
                (2, '', 'http://example.com/2.xml', NULL)",
        [],
    )?;

    let (status, body) = get_page(tc.client()?, "/feeds").await?;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("<title>Feeds - Kiki</title>"), "{body}");
    assert!(body.contains("2 feeds"), "{body}");
    assert!(
        body.contains(r#"<a href="/feeds/1">Feed &lt;One&gt;</a>"#),
        "{body}"
    );
    assert!(
        body.contains(r#"<a href="/feeds/2">(untitled feed)</a>"#),
        "{body}"
    );
    assert!(!body.contains(r#""><b>"#), "{body}");
    assert!(!body.contains(r#"href="http://example.com"#), "{body}");
    assert!(
        body.contains(r#"<span class="url" title="http://example.com/2.xml">example.com</span>"#),
        "{body}"
    );
    assert!(!body.contains(">http://example.com/2.xml<"), "{body}");
    assert!(body.contains("last checked"), "{body}");
    assert!(body.contains("not checked yet"), "{body}");
    assert!(body.contains("Page 1 of 1"), "{body}");
    Ok(())
}

#[tokio::test]
async fn the_feeds_page_says_when_there_are_no_feeds() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    let (status, body) = get_page(tc.client()?, "/feeds").await?;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("0 feeds"), "{body}");
    assert!(body.contains("No feeds have been added yet."), "{body}");
    Ok(())
}

#[tokio::test]
async fn the_feeds_page_pages_through_the_feeds() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    let conn = tc.database_conn()?;
    for i in 1..=30 {
        conn.execute(
            "INSERT INTO feeds (id, title, url) VALUES (?1, ?2, ?3)",
            rusqlite::params![
                i,
                format!("Feed {i}"),
                format!("http://example.com/{i}.xml")
            ],
        )?;
    }

    let (_, body) = get_page(tc.client()?, "/feeds").await?;
    assert!(body.contains("30 feeds"), "{body}");
    assert!(body.contains("Page 1 of 2"), "{body}");
    assert!(
        body.contains(r#"href="/feeds?page=2" rel="next""#),
        "{body}"
    );
    assert_eq!(body.matches(r#"<li><a href="/feeds/"#).count(), 25);

    let (_, body) = get_page(tc.client()?, "/feeds?page=2").await?;
    assert!(
        body.contains(r#"href="/feeds?page=1" rel="prev""#),
        "{body}"
    );
    assert_eq!(body.matches(r#"<li><a href="/feeds/"#).count(), 5);
    Ok(())
}

/// A feed's page lists only that feed's entries, newest first, and its
/// entries link to pages that link back to it.
#[tokio::test]
async fn the_feed_page_lists_the_feeds_entries() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    let conn = tc.database_conn()?;
    conn.execute(
        "INSERT INTO feeds (id, title, url, description)
         VALUES (1, 'Feed <One>', 'http://example.com/1.xml', 'About <b>one</b>'),
                (2, 'Feed Two', 'http://example.com/2.xml', NULL)",
        [],
    )?;
    for i in 1..=30 {
        conn.execute(
            "INSERT INTO entries (feed_id, syndication_format, guid, published_at, title, url)
             VALUES (?1, 'rss', ?2, ?3, ?4, ?5)",
            rusqlite::params![
                if i % 2 == 0 { 1 } else { 2 },
                format!("guid-{i}"),
                1_700_000_000 + i * 86_400,
                format!("Entry {i}"),
                format!("http://example.com/{i}"),
            ],
        )?;
    }

    let (status, body) = get_page(tc.client()?, "/feeds/1").await?;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("<title>Feed &lt;One&gt; - Kiki</title>"),
        "{body}"
    );
    assert!(body.contains("<h2>Feed &lt;One&gt;</h2>"), "{body}");
    assert!(body.contains("About &lt;b&gt;one&lt;/b&gt;"), "{body}");
    assert!(body.contains("15 unread entries"), "{body}");
    let expected: Vec<_> = (1..=15).rev().map(|i| format!("Entry {}", i * 2)).collect();
    assert_eq!(listed_titles(&body), expected);
    // Every entry is from this feed, so none is labelled with it.
    assert!(!body.contains(r#"class="feed""#), "{body}");
    assert!(body.contains(r#"href="/entries/30?feed=1""#), "{body}");
    assert!(
        body.contains(r#"<a href="/feeds">&larr; Back to feeds</a>"#),
        "{body}"
    );

    let (status, body) = get_page(tc.client()?, "/entries/30?feed=1").await?;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains(r#"<a href="/feeds/1">&larr; Back to feed</a>"#),
        "{body}"
    );
    Ok(())
}

/// A feed's page leaves out its read entries too, unless asked for them.
#[tokio::test]
async fn the_feed_page_hides_read_entries_by_default() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    let conn = tc.database_conn()?;
    conn.execute(
        "INSERT INTO feeds (id, title, url) VALUES (1, 'Feed', 'http://example.com/1.xml')",
        [],
    )?;
    insert_entries(&tc, 3)?;
    conn.execute("UPDATE entries SET feed_id = 1", [])?;
    tag_entry(&tc, 3, "system:read")?;

    let (_, body) = get_page(tc.client()?, "/feeds/1").await?;
    assert!(body.contains("2 unread entries"), "{body}");
    assert_eq!(listed_titles(&body), ["Entry 2", "Entry 1"]);
    assert!(
        body.contains(r#"data-href="/feeds/1?show_read=true""#),
        "{body}"
    );

    let (_, body) = get_page(tc.client()?, "/feeds/1?show_read=true").await?;
    assert!(body.contains("3 entries"), "{body}");
    assert_eq!(listed_titles(&body), ["Entry 3", "Entry 2", "Entry 1"]);
    assert!(
        body.contains(r#"href="/entries/3?feed=1&amp;show_read=true""#),
        "{body}"
    );
    Ok(())
}

#[tokio::test]
async fn the_feed_page_pages_through_the_entries() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    let conn = tc.database_conn()?;
    conn.execute(
        "INSERT INTO feeds (id, title, url) VALUES (1, 'Feed', 'http://example.com/1.xml')",
        [],
    )?;
    insert_entries(&tc, 30)?;
    conn.execute("UPDATE entries SET feed_id = 1", [])?;

    let (_, body) = get_page(tc.client()?, "/feeds/1").await?;
    assert!(body.contains("Page 1 of 2"), "{body}");
    assert!(
        body.contains(r#"href="/feeds/1?page=2" rel="next""#),
        "{body}"
    );

    let (_, body) = get_page(tc.client()?, "/feeds/1?page=2").await?;
    assert!(body.contains("Page 2 of 2"), "{body}");
    assert!(
        body.contains(r#"href="/feeds/1?page=1" rel="prev""#),
        "{body}"
    );
    let expected: Vec<_> = (1..=5).rev().map(|i| format!("Entry {i}")).collect();
    assert_eq!(listed_titles(&body), expected);
    assert!(
        body.contains(r#"href="/entries/5?feed=1&amp;page=2""#),
        "{body}"
    );

    let (_, body) = get_page(tc.client()?, "/entries/5?feed=1&page=2").await?;
    assert!(
        body.contains(r#"<a href="/feeds/1?page=2">&larr; Back to feed</a>"#),
        "{body}"
    );
    Ok(())
}

/// The list of plugins shows each installed plugin, and each directory
/// that could not be loaded as one and why.
#[tokio::test]
async fn the_plugins_page_lists_every_plugin() -> Result<()> {
    let tc = TestBuilder::default().init_database().build()?;
    tc.install_lua_plugin("passthrough", "", serde_json::json!({}))?;
    std::fs::create_dir_all(tc.plugins_dir().join("broken"))?;
    let tc = tc.init_server()?;

    let (status, body) = get_page(tc.client()?, "/plugins").await?;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("<title>Plugins - Kiki</title>"), "{body}");
    assert!(body.contains("1 plugin<"), "{body}");
    assert!(
        body.contains(r#"<a href="/plugins/passthrough"><strong>passthrough</strong></a> <span class="version">v1.0.0</span>"#),
        "{body}"
    );
    assert!(body.contains("Could not be loaded"), "{body}");
    assert!(body.contains("<strong>broken</strong>"), "{body}");
    assert!(body.contains("manifest.toml"), "{body}");
    Ok(())
}

#[tokio::test]
async fn the_plugins_page_says_when_there_are_no_plugins() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    let (status, body) = get_page(tc.client()?, "/plugins").await?;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("0 plugins"), "{body}");
    assert!(body.contains("No plugins are installed."), "{body}");
    assert!(!body.contains("Could not be loaded"), "{body}");
    Ok(())
}

/// Serve the web UI on an ephemeral port with `api` as its API client,
/// and submit `form` to `path` from it, sending `headers` as well. The
/// client follows redirects.
async fn post_form(
    api: reqwest::Client,
    path: &str,
    form: &[(&str, &str)],
    headers: &[(&str, &str)],
) -> Result<(StatusCode, String)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let cancel = CancellationToken::new();
    let task = tokio::spawn(serve_ui(
        listener,
        api,
        AllowedHosts::default(),
        cancel.clone(),
    ));

    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(form)
        .finish();
    let mut req = reqwest::Client::new()
        .post(format!("http://{addr}{path}"))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(body);
    for (name, value) in headers {
        req = req.header(*name, value.replace("{addr}", &addr.to_string()));
    }
    let resp = req.send().await?;
    let status = resp.status();
    let body = resp.text().await?;

    cancel.cancel();
    task.await??;
    Ok((status, body))
}

/// A test context with one plugin, `hello`, whose defaults are
/// `{"greeting": "hi", "count": 1, "tags": ["a"]}`, and the server running.
fn hello_plugin() -> Result<crate::test::TestConfig> {
    let tc = TestBuilder::default().init_database().build()?;
    tc.install_lua_plugin(
        "hello",
        "",
        serde_json::json!({"greeting": "hi", "count": 1, "tags": ["a"]}),
    )?;
    tc.init_server()
}

/// The overrides stored in the database for `hello`.
fn stored_overrides(tc: &crate::test::TestConfig) -> Result<Value> {
    Ok(Value::Object(crate::db::plugins::get_config_overrides(
        &tc.database_conn()?,
        "hello",
    )?))
}

/// A plugin's page shows its config, one form per setting, and allows
/// forms to be submitted to the web UI.
#[tokio::test]
async fn the_plugin_page_shows_the_config() -> Result<()> {
    let tc = hello_plugin()?;

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let cancel = CancellationToken::new();
    let task = tokio::spawn(serve_ui(
        listener,
        tc.client()?,
        AllowedHosts::default(),
        cancel.clone(),
    ));
    let resp = reqwest::get(format!("http://{addr}/plugins/hello")).await?;
    let csp = resp.headers()[header::CONTENT_SECURITY_POLICY]
        .to_str()?
        .to_owned();
    let status = resp.status();
    let body = resp.text().await?;
    cancel.cancel();
    task.await??;

    assert_eq!(status, StatusCode::OK);
    assert!(csp.contains("form-action 'self'"), "{csp}");
    assert!(csp.contains(SCRIPT_SRC.as_str()), "{csp}");
    assert!(
        body.contains("<title>hello - Plugins - Kiki</title>"),
        "{body}"
    );
    assert!(body.contains("<h2>hello <small"), "{body}");
    assert!(
        body.contains(r#"<form method="post" action="/plugins/hello/config""#),
        "{body}"
    );
    assert!(
        body.contains(r#"<input type="hidden" name="key" value="greeting"><input type="text" name="value" value="&quot;hi&quot;""#),
        "{body}"
    );
    assert!(body.contains(r#"name="value" value="1""#), "{body}");
    assert!(body.contains("<textarea name=\"value\""), "{body}");
    // Settings the manifest does not describe get fields that fit their
    // defaults, and can be edited as JSON too.
    assert!(
        body.contains(r#"<input type="text" name="v" id="setting-1-v" value="hi""#),
        "{body}"
    );
    assert!(
        body.contains(r#"<input type="number" step="1" name="v" id="setting-0-v" value="1""#),
        "{body}"
    );
    assert!(
        body.contains(
            r#"<textarea name="v" id="setting-2-v" rows="3" aria-label="tags">a</textarea>"#
        ),
        "{body}"
    );
    assert!(body.contains("<summary>Edit as JSON</summary>"), "{body}");
    assert!(!body.contains("Reset to default"), "{body}");
    assert!(!body.contains("reset_all"), "{body}");
    assert!(!body.contains(r#"class="notice""#), "{body}");
    Ok(())
}

/// Saving a setting overrides it, and the page then says so; resetting
/// it restores the default.
#[tokio::test]
async fn settings_can_be_changed_from_the_plugin_page() -> Result<()> {
    let tc = hello_plugin()?;
    let path = "/plugins/hello/config";

    let (status, body) = post_form(
        tc.client()?,
        path,
        &[
            ("action", "set"),
            ("key", "greeting"),
            ("value", "\"hello\""),
        ],
        &[],
    )
    .await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        stored_overrides(&tc)?,
        serde_json::json!({"greeting": "hello"})
    );
    assert!(body.contains("<h2>hello <small"), "{body}");
    assert!(body.contains(r#"value="&quot;hello&quot;""#), "{body}");
    assert!(
        body.contains("overrides the default, <code>&quot;hi&quot;</code>"),
        "{body}"
    );
    // The plugins reloaded, so the plugin runs with the new value.
    assert!(!body.contains("running with"), "{body}");
    assert!(!body.contains(r#"class="notice""#), "{body}");
    assert!(body.contains("Reset to default"), "{body}");

    // Settings the manifest has no default for can be added, too.
    let (status, _) = post_form(
        tc.client()?,
        path,
        &[
            ("action", "set"),
            ("key", "extra/key"),
            ("value", "{\"a\": [1, 2]}"),
        ],
        &[],
    )
    .await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        stored_overrides(&tc)?,
        serde_json::json!({"greeting": "hello", "extra/key": {"a": [1, 2]}})
    );

    // Keys are sent to the API as one path segment.
    let (status, body) = post_form(
        tc.client()?,
        path,
        &[("action", "reset"), ("key", "extra/key"), ("value", "")],
        &[],
    )
    .await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        stored_overrides(&tc)?,
        serde_json::json!({"greeting": "hello"})
    );
    assert!(body.contains("reset_all"), "{body}");

    let (status, body) = post_form(tc.client()?, path, &[("action", "reset_all")], &[]).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(stored_overrides(&tc)?, serde_json::json!({}));
    assert!(!body.contains(r#"class="notice""#), "{body}");
    Ok(())
}

/// A plugin, `rules`, whose manifest describes its settings: a
/// boolean, a choice and a list of objects. The server is running.
fn rules_plugin() -> Result<crate::test::TestConfig> {
    let tc = TestBuilder::default().init_database().build()?;
    let dir = tc.plugins_dir().join("rules");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(
        dir.join("manifest.toml"),
        r#"
        name = "rules"
        version = "1.0.0"
        engine = "lua"

        [config]
        on = true
        mode = "fast"
        rules = [{ pattern = "a", fields = ["title"] }]
        toggles = [{ enabled = false }]

        [[settings]]
        name = "on"
        type = "boolean"
        label = "Enabled"
        description = "Whether to do anything."

        [[settings]]
        name = "mode"
        type = "choice"
        choices = ["fast", "slow"]

        [[settings]]
        name = "rules"
        type = "list"
        label = "Rules"
        [settings.items]
        type = "object"
        [[settings.items.fields]]
        name = "pattern"
        type = "string"
        required = true
        [[settings.items.fields]]
        name = "fields"
        type = "list"
        items = { type = "choice", choices = ["title", "content"] }
        [[settings.items.fields]]
        name = "feeds"
        type = "list"
        items = { type = "integer" }

        [[settings]]
        name = "toggles"
        type = "list"
        [settings.items]
        type = "object"
        [[settings.items.fields]]
        name = "enabled"
        type = "boolean"
        [[settings.items.fields]]
        name = "note"
        type = "string"
        "#,
    )?;
    std::fs::write(dir.join("main.lua"), "")?;
    tc.init_server()
}

fn stored_rules(tc: &crate::test::TestConfig) -> Result<Value> {
    Ok(Value::Object(crate::db::plugins::get_config_overrides(
        &tc.database_conn()?,
        "rules",
    )?))
}

/// Settings the manifest describes get fields that fit them, in the
/// order the manifest gives.
#[tokio::test]
async fn described_settings_get_fitting_fields() -> Result<()> {
    let tc = rules_plugin()?;
    let (status, body) = get_page(tc.client()?, "/plugins/rules").await?;
    assert_eq!(status, StatusCode::OK);

    assert!(body.contains("<h4>Enabled <code>on</code></h4>"), "{body}");
    assert!(body.contains("Whether to do anything."), "{body}");
    assert!(
        body.contains(
            r#"<input type="checkbox" name="v" id="setting-0-v" value="true" checked> Enabled"#
        ),
        "{body}"
    );
    assert!(
        body.contains(
            r#"<option value="fast" selected>fast</option><option value="slow">slow</option>"#
        ),
        "{body}"
    );
    assert!(
        !body.contains("(not set)</option><option value=\"fast\" selected"),
        "{body}"
    );
    // One fieldset per rule, and a blank one to add a rule.
    assert!(body.contains(r#"name="v#count" value="2""#), "{body}");
    assert!(
        body.contains(r#"name="v.0.pattern" id="setting-2-v.0.pattern" value="a""#),
        "{body}"
    );
    assert!(
        body.contains(r#"name="v.0.fields" value="title" checked"#),
        "{body}"
    );
    assert!(
        body.contains(r#"name="v.0.fields" value="content">"#),
        "{body}"
    );
    assert!(body.contains(r#"name="remove" value="v.0""#), "{body}");
    assert!(
        body.contains(r#"name="v.1.pattern" id="setting-2-v.1.pattern" value="""#),
        "{body}"
    );
    assert!(body.contains(r#"<legend>Add to Rules</legend>"#), "{body}");
    assert!(body.find("<code>on</code>") < body.find("<code>mode</code>"));
    assert!(body.find("<code>mode</code>") < body.find("<code>rules</code>"));
    assert!(
        body.contains("Add a setting the plugin does not describe"),
        "{body}"
    );
    Ok(())
}

/// Saving a described setting reads its value from its fields.
#[tokio::test]
async fn described_settings_are_saved_from_their_fields() -> Result<()> {
    let tc = rules_plugin()?;
    let path = "/plugins/rules/config";

    // An unticked box is false.
    let (status, body) = post_form(
        tc.client()?,
        path,
        &[("action", "save"), ("key", "on")],
        &[],
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(stored_rules(&tc)?, serde_json::json!({"on": false}));

    let (status, _) = post_form(
        tc.client()?,
        path,
        &[("action", "save"), ("key", "mode"), ("v", "slow")],
        &[],
    )
    .await?;
    assert_eq!(status, StatusCode::OK);

    // Edit the first rule, remove it, keep the second, and add a third
    // from the blank fieldset; a blank one adds nothing.
    let (status, body) = post_form(
        tc.client()?,
        path,
        &[
            ("action", "save"),
            ("key", "rules"),
            ("v#count", "4"),
            ("v.0.pattern", "gone"),
            ("remove", "v.0"),
            ("v.1.pattern", "b"),
            ("v.1.fields", "title"),
            ("v.1.fields", "content"),
            ("v.1.feeds", "1\r\n\r\n2\r\n"),
            ("v.2.pattern", "c"),
            ("v.3.pattern", ""),
            ("v.3.feeds", ""),
        ],
        &[],
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        stored_rules(&tc)?,
        serde_json::json!({
            "on": false,
            "mode": "slow",
            "rules": [
                {"pattern": "b", "fields": ["title", "content"], "feeds": [1, 2]},
                {"pattern": "c"},
            ],
        })
    );
    assert!(
        body.contains(r#"name="v.1.pattern" id="setting-2-v.1.pattern" value="c""#),
        "{body}"
    );

    // Removing every item leaves an empty list.
    let (status, _) = post_form(
        tc.client()?,
        path,
        &[
            ("action", "save"),
            ("key", "rules"),
            ("v#count", "1"),
            ("v.0.pattern", "b"),
            ("remove", "v.0"),
        ],
        &[],
    )
    .await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        stored_rules(&tc)?.get("rules"),
        Some(&serde_json::json!([]))
    );
    Ok(())
}

/// An item already in a list is kept when all its fields are left empty
/// or unticked; only the blank fieldset for a new item is dropped.
#[tokio::test]
async fn existing_items_are_kept_when_left_blank() -> Result<()> {
    let tc = rules_plugin()?;
    let (_, body) = get_page(tc.client()?, "/plugins/rules").await?;
    assert!(body.contains(r#"name="v.0#present" value="1""#), "{body}");
    assert!(!body.contains(r#"name="v.1#present""#), "{body}");

    // What the browser submits for the page as it is: the item's box is
    // unticked, and its note and the blank fieldset are empty.
    let (status, body) = post_form(
        tc.client()?,
        "/plugins/rules/config",
        &[
            ("action", "save"),
            ("key", "toggles"),
            ("v#count", "2"),
            ("v.0#present", "1"),
            ("v.0.note", ""),
            ("v.1.note", ""),
        ],
        &[],
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        stored_rules(&tc)?,
        serde_json::json!({"toggles": [{"enabled": false}]})
    );
    Ok(())
}

/// Fields that do not hold a value of their setting's type are reported,
/// and nothing is saved; so are JSON values the API refuses.
#[tokio::test]
async fn described_settings_are_checked() -> Result<()> {
    let tc = rules_plugin()?;
    let path = "/plugins/rules/config";

    for (form, error) in [
        (
            &[("v#count", "1"), ("v.0.fields", "title")][..],
            "Rules was not saved: rules[0].pattern: is required.",
        ),
        (
            &[("v#count", "1"), ("v.0.pattern", "a"), ("v.0.feeds", "x")],
            "Rules was not saved: rules[0].feeds[0]: &quot;x&quot; is not a whole number.",
        ),
        (
            &[
                ("v#count", "1"),
                ("v.0.pattern", "a"),
                ("v.0.fields", "url"),
            ],
            "Rules was not saved: rules[0].fields[0]: expected one of",
        ),
    ] {
        let mut pairs = vec![("action", "save"), ("key", "rules")];
        pairs.extend_from_slice(form);
        let (status, body) = post_form(tc.client()?, path, &pairs, &[]).await?;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(body.contains(error), "{error}: {body}");
    }

    let (status, body) = post_form(
        tc.client()?,
        path,
        &[("action", "set"), ("key", "mode"), ("value", "\"medium\"")],
        &[],
    )
    .await?;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        body.contains("The config was not saved. Invalid config: mode: expected one of"),
        "{body}"
    );
    assert_eq!(stored_rules(&tc)?, serde_json::json!({}));
    Ok(())
}

/// A value that does not fit its setting's fields, set some other way,
/// is shown as JSON.
#[tokio::test]
async fn values_that_do_not_fit_are_shown_as_json() -> Result<()> {
    let tc = rules_plugin()?;
    let overrides = serde_json::json!({"rules": [{"pattern": "a", "fields": "title"}]});
    crate::db::plugins::set_config_overrides(
        &tc.database_conn()?,
        "rules",
        overrides.as_object().unwrap_or(&Map::new()),
    )?;
    let (status, body) = get_page(tc.client()?, "/plugins/rules").await?;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !body.contains(r#"<div class="items" id="setting-2-v">"#),
        "{body}"
    );
    assert!(
        body.contains("shown as JSON, since the value does not fit the setting's fields"),
        "{body}"
    );
    Ok(())
}

/// A value that is not JSON is reported on the page, and not saved.
#[tokio::test]
async fn invalid_settings_are_reported() -> Result<()> {
    let tc = hello_plugin()?;
    let path = "/plugins/hello/config";

    let (status, body) = post_form(
        tc.client()?,
        path,
        &[("action", "set"), ("key", "greeting"), ("value", "<hello>")],
        &[],
    )
    .await?;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        body.contains(r#"<p class="error">The value for greeting is not valid JSON"#),
        "{body}"
    );
    assert!(!body.contains("<hello>"), "{body}");

    let (status, body) = post_form(
        tc.client()?,
        path,
        &[("action", "set"), ("key", ""), ("value", "1")],
        &[],
    )
    .await?;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body.contains("Give the setting a name."), "{body}");

    assert_eq!(stored_overrides(&tc)?, serde_json::json!({}));
    Ok(())
}

/// A setting the plugin fails to load with is saved, and the page says
/// the plugin keeps running with its old config.
#[tokio::test]
async fn settings_that_fail_to_load_are_reported() -> Result<()> {
    let tc = TestBuilder::default().init_database().build()?;
    tc.install_lua_plugin(
        "hello",
        "local config = ...\nif config.fail then error('refusing to load') end",
        serde_json::json!({}),
    )?;
    let tc = tc.init_server()?;

    let (status, body) = post_form(
        tc.client()?,
        "/plugins/hello/config",
        &[("action", "set"), ("key", "fail"), ("value", "true")],
        &[],
    )
    .await?;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body.contains("refusing to load"), "{body}");
    assert!(body.contains(r#"class="notice""#), "{body}");
    assert_eq!(stored_overrides(&tc)?, serde_json::json!({"fail": true}));
    Ok(())
}

/// Forms submitted from other sites are refused; forms from the web
/// UI's own pages are not.
#[tokio::test]
async fn cross_site_forms_are_refused() -> Result<()> {
    let tc = hello_plugin()?;
    let path = "/plugins/hello/config";
    let form = [("action", "set"), ("key", "count"), ("value", "2")];

    for headers in [
        &[("Sec-Fetch-Site", "cross-site")][..],
        &[("Sec-Fetch-Site", "same-site")],
        &[("Origin", "http://evil.example")],
        &[("Origin", "null")],
    ] {
        let (status, _) = post_form(tc.client()?, path, &form, headers).await?;
        assert_eq!(status, StatusCode::FORBIDDEN, "{headers:?}");
    }
    assert_eq!(stored_overrides(&tc)?, serde_json::json!({}));

    for headers in [
        &[("Sec-Fetch-Site", "same-origin")][..],
        &[("Origin", "http://{addr}")],
    ] {
        let (status, _) = post_form(tc.client()?, path, &form, headers).await?;
        assert_eq!(status, StatusCode::OK, "{headers:?}");
    }
    assert_eq!(stored_overrides(&tc)?, serde_json::json!({"count": 2}));
    Ok(())
}

#[tokio::test]
async fn a_missing_plugin_is_reported() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    let (status, body) = get_page(tc.client()?, "/plugins/missing").await?;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("Plugin not found."), "{body}");

    let (status, body) = post_form(
        tc.client()?,
        "/plugins/missing/config",
        &[("action", "set"), ("key", "a"), ("value", "1")],
        &[],
    )
    .await?;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("Plugin not found."), "{body}");
    Ok(())
}

#[tokio::test]
async fn a_missing_feed_is_reported() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    let (status, body) = get_page(tc.client()?, "/feeds/42").await?;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("Feed not found."), "{body}");
    Ok(())
}

#[tokio::test]
async fn a_missing_entry_is_reported() -> Result<()> {
    let tc = TestBuilder::all().build()?;
    let (status, body) = get_page(tc.client()?, "/entries/42").await?;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("Entry not found."), "{body}");
    Ok(())
}

#[tokio::test]
async fn an_unreachable_server_is_reported_on_the_page() -> Result<()> {
    let td = tempfile::TempDir::with_prefix("kiki_")?;
    let api = api_client(&td.path().join("missing.sock"))?;
    let (status, body) = get_index(api).await?;

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(body.contains("unavailable"), "{body}");
    Ok(())
}

#[test]
fn allowed_hosts_are_parsed() {
    let parsed = parse(&[
        "--allowed-host",
        "Kiki.LAN.",
        "--allowed-host",
        "*.example.com",
        "--allowed-host",
        "[fe80::1]",
        "--allowed-host",
        "*",
    ]);
    assert_eq!(
        parsed.allowed_hosts,
        [
            HostPattern::Exact("kiki.lan".into()),
            HostPattern::Subdomains("example.com".into()),
            HostPattern::Exact("fe80::1".into()),
            HostPattern::Any,
        ]
    );
    assert!(parse(&[]).allowed_hosts.is_empty());

    assert_eq!("".parse::<HostPattern>(), Err(HostPatternError::Empty));
    for bad in [
        "kiki.lan:8080",
        "http://kiki.lan",
        "kiki.lan/x",
        "*.[::1]",
        "a b",
    ] {
        assert!(
            matches!(
                bad.parse::<HostPattern>(),
                Err(HostPatternError::Invalid(_))
            ),
            "{bad}"
        );
    }
}

/// Only the localhost names are allowed by default, with or without a
/// port; `--allowed-host` adds to them, and `*` allows anything.
#[test]
fn allowed_hosts_are_matched() {
    let localhost = AllowedHosts::default();
    for host in [
        "localhost",
        "LOCALHOST:8080",
        "localhost.",
        "127.0.0.1:8080",
        "[::1]",
        "[::1]:8080",
    ] {
        assert!(localhost.allows(host), "{host}");
    }
    for host in [
        "evil.example",
        "evil.example:8080",
        "localhost.evil.example",
        "127.0.0.2",
        "[fe80::1]:8080",
        "",
        ":8080",
        "localhost:",
        "localhost:http",
        "[::1",
        "[not-v6]:8080",
    ] {
        assert!(!localhost.allows(host), "{host}");
    }

    let patterns = ["kiki.lan", "*.example.com", "192.168.1.5"]
        .map(|p| p.parse().unwrap())
        .to_vec();
    let allowed = AllowedHosts::new(patterns);
    for host in [
        "localhost:8080",
        "Kiki.Lan:8080",
        "a.example.com",
        "a.b.example.com",
        "192.168.1.5",
    ] {
        assert!(allowed.allows(host), "{host}");
    }
    for host in [
        "example.com",
        "badexample.com",
        "kiki.lan.evil.example",
        "192.168.1.6",
    ] {
        assert!(!allowed.allows(host), "{host}");
    }

    let any = AllowedHosts::new(vec![HostPattern::Any]);
    assert!(any.allows("evil.example:8080"));
    assert!(!any.allows("evil.example:port"));
}

/// Fetch `/` from a web UI that answers to `allowed`, naming `host` in the
/// request's `Host` header, and return the response's status.
async fn get_with_host(
    api: reqwest::Client,
    allowed: AllowedHosts,
    host: &str,
) -> Result<StatusCode> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let cancel = CancellationToken::new();
    let task = tokio::spawn(serve_ui(listener, api, allowed, cancel.clone()));

    let status = reqwest::Client::new()
        .get(format!("http://{addr}/"))
        .header(header::HOST, host)
        .send()
        .await?
        .status();

    cancel.cancel();
    task.await??;
    Ok(status)
}

/// Requests naming a host the web UI was not told to answer to, as a site
/// rebinding its domain to this machine would send, are refused, page
/// loads and changes alike.
#[tokio::test]
async fn requests_for_other_hosts_are_refused() -> Result<()> {
    let tc = TestBuilder::all().build()?;

    for host in ["localhost:8080", "127.0.0.1", "[::1]:8080"] {
        let status = get_with_host(tc.client()?, AllowedHosts::default(), host).await?;
        assert_eq!(status, StatusCode::OK, "{host}");
    }
    for host in ["evil.example", "evil.example:8080", "kiki.lan"] {
        let status = get_with_host(tc.client()?, AllowedHosts::default(), host).await?;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{host}");
    }

    // A same-origin change from a rebound domain is refused too.
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let cancel = CancellationToken::new();
    let task = tokio::spawn(serve_ui(
        listener,
        tc.client()?,
        AllowedHosts::default(),
        cancel.clone(),
    ));
    let status = reqwest::Client::new()
        .post(format!("http://{addr}/entries/read"))
        .header(header::HOST, "evil.example")
        .header("Sec-Fetch-Site", "same-origin")
        .send()
        .await?
        .status();
    cancel.cancel();
    task.await??;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let allowed = AllowedHosts::new(vec!["kiki.lan".parse()?]);
    let status = get_with_host(tc.client()?, allowed.clone(), "kiki.lan:8080").await?;
    assert_eq!(status, StatusCode::OK);
    let status = get_with_host(tc.client()?, allowed, "evil.example").await?;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let any = AllowedHosts::new(vec![HostPattern::Any]);
    let status = get_with_host(tc.client()?, any, "evil.example").await?;
    assert_eq!(status, StatusCode::OK);
    Ok(())
}

/// Hosts from `--allowed-host` and from the config file's
/// `web_ui.allowed_hosts` are both allowed.
#[test]
fn allowed_hosts_combine_flags_and_config() {
    let args = parse(&["--allowed-host", "kiki.lan"]);
    let allowed = args.allowed_hosts(&["*.example.com".parse().unwrap()]);
    assert!(allowed.allows("localhost"));
    assert!(allowed.allows("kiki.lan"));
    assert!(allowed.allows("a.example.com"));
    assert!(!allowed.allows("evil.example"));

    let allowed = parse(&[]).allowed_hosts(&[HostPattern::Any]);
    assert!(allowed.allows("evil.example"));
    assert!(parse(&[]).allowed_hosts(&[]).only_localhost());
}
