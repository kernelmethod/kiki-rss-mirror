//! Reading and writing [OPML](https://en.wikipedia.org/wiki/OPML) feed lists.
//!
//! This module holds the logic shared by the `/v1/feeds/import` and
//! `/v1/feeds/export` routes and the `kiki opml` subcommands: parsing an OPML
//! document into a list of feeds, rendering feeds as OPML, and moving those
//! feeds into and out of the database.
//!
//! Tags map onto OPML folders: a feed nested inside one or more folder
//! outlines is tagged with each folder's name, and on export each tag becomes
//! a folder containing the feeds that carry it.
//!
//! # Examples
//!
//! ```
//! use kiki_rss::opml::{build_opml, parse_opml, OpmlFeed};
//!
//! let feeds = vec![OpmlFeed {
//!     title: "Example".to_string(),
//!     url: "https://example.com/feed.xml".to_string(),
//!     tags: vec!["news".to_string()],
//! }];
//!
//! let xml = build_opml(&feeds).unwrap();
//! assert_eq!(parse_opml(&xml).unwrap(), feeds);
//! ```
use crate::db::tags::is_reserved_tag_name;
use quick_xml::events::{BytesDecl, BytesStart, BytesText, Event};
use quick_xml::reader::Reader;
use quick_xml::Writer;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use std::collections::{BTreeMap, HashMap};
use std::io::Cursor;
use thiserror::Error;

/// Errors that can occur while importing or exporting OPML.
#[derive(Debug, Error)]
pub enum OpmlError {
    /// The input was not well-formed XML.
    #[error("invalid OPML: {0}")]
    Parse(#[from] quick_xml::Error),

    /// An `<outline>` attribute could not be read or unescaped.
    #[error("invalid OPML attribute: {0}")]
    Attribute(String),

    /// Writing the OPML document failed.
    #[error("unable to write OPML: {0}")]
    Write(#[from] std::io::Error),

    /// A database query failed.
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
}

/// A feed as it appears in an OPML document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpmlFeed {
    /// Human-readable feed title (the outline's `text` or `title`).
    pub title: String,
    /// URL of the feed itself (the outline's `xmlUrl`).
    pub url: String,
    /// Tag names, taken from the folders the feed is nested in.
    pub tags: Vec<String>,
}

/// The outcome of [`import_feeds`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImportSummary {
    /// IDs of the feeds that were newly created, in document order.
    pub imported: Vec<i64>,
    /// Number of feeds skipped because a feed with the same URL already
    /// existed in the database.
    pub skipped: usize,
}

/// Parse an OPML document into the feeds it lists.
///
/// Every `<outline>` with an `xmlUrl` attribute is treated as a feed; any
/// other `<outline>` is a folder whose name is added as a tag to the feeds
/// nested inside it. Outlines with an empty `xmlUrl` are skipped. A feed
/// listed more than once (for instance, in several
/// folders, which is how [`build_opml`] writes a feed with several tags) is
/// returned once with the union of its tags.
///
/// # Errors
///
/// Returns [`OpmlError::Parse`] if the document is not well-formed XML, or
/// [`OpmlError::Attribute`] if an outline attribute cannot be decoded.
///
/// # Examples
///
/// ```
/// use kiki_rss::opml::parse_opml;
///
/// let feeds = parse_opml(r#"<opml version="2.0"><body>
///   <outline text="Tech">
///     <outline text="Example" xmlUrl="https://example.com/feed.xml"/>
///   </outline>
/// </body></opml>"#).unwrap();
///
/// assert_eq!(feeds[0].url, "https://example.com/feed.xml");
/// assert_eq!(feeds[0].tags, vec!["Tech".to_string()]);
/// ```
pub fn parse_opml(xml: &str) -> Result<Vec<OpmlFeed>, OpmlError> {
    let mut reader = Reader::from_str(xml);
    let mut feeds: Vec<OpmlFeed> = Vec::new();
    let mut by_url: HashMap<String, usize> = HashMap::new();
    let mut folders: Vec<String> = Vec::new();
    // One entry per open <outline>: whether it pushed a name onto `folders`.
    let mut open: Vec<bool> = Vec::new();

    let mut add_feed = |attrs: OutlineAttrs, url: String, folders: &[String]| {
        // A feed with no URL can't be fetched, and importing one would add a
        // new URL-less feed on every run since there is nothing to match on.
        if url.is_empty() {
            return;
        }
        let title = attrs.text.unwrap_or_else(|| url.clone());
        if let Some(existing) = by_url.get(&url).and_then(|&i| feeds.get_mut(i)) {
            for tag in folders {
                if !existing.tags.contains(tag) {
                    existing.tags.push(tag.clone());
                }
            }
        } else {
            by_url.insert(url.clone(), feeds.len());
            let mut tags = folders.to_vec();
            tags.dedup();
            feeds.push(OpmlFeed { title, url, tags });
        }
    };

    loop {
        match reader.read_event()? {
            Event::Start(ref e) if e.name().as_ref() == b"outline" => {
                let mut attrs = OutlineAttrs::from_element(e)?;
                let mut pushed = false;
                if let Some(url) = attrs.xml_url.take() {
                    add_feed(attrs, url, &folders);
                } else if let Some(name) = attrs.text.filter(|name| !name.is_empty()) {
                    folders.push(name);
                    pushed = true;
                }
                open.push(pushed);
            }
            Event::Empty(ref e) if e.name().as_ref() == b"outline" => {
                let mut attrs = OutlineAttrs::from_element(e)?;
                if let Some(url) = attrs.xml_url.take() {
                    add_feed(attrs, url, &folders);
                }
            }
            Event::End(ref e) if e.name().as_ref() == b"outline" => {
                if open.pop() == Some(true) {
                    folders.pop();
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }

    Ok(feeds)
}

/// The attributes of an `<outline>` element that Kiki cares about.
struct OutlineAttrs {
    text: Option<String>,
    xml_url: Option<String>,
}

impl OutlineAttrs {
    fn from_element(e: &BytesStart) -> Result<Self, OpmlError> {
        let mut text = None;
        let mut xml_url = None;

        for attr in e.attributes() {
            let attr = attr.map_err(|e| OpmlError::Attribute(e.to_string()))?;
            let value = || {
                attr.unescape_value()
                    .map(|v| v.into_owned())
                    .map_err(|e| OpmlError::Attribute(e.to_string()))
            };
            match attr.key.as_ref() {
                b"text" | b"title" if text.is_none() => text = Some(value()?),
                b"xmlUrl" => xml_url = Some(value()?),
                _ => {}
            }
        }

        Ok(Self { text, xml_url })
    }
}

/// Render feeds as an OPML 2.0 document.
///
/// Untagged feeds are written at the top level of the `<body>`. Each tag
/// becomes a folder outline containing every feed with that tag, so a feed
/// with several tags appears once per tag.
///
/// # Errors
///
/// Returns [`OpmlError::Write`] if the XML cannot be written.
///
/// # Examples
///
/// ```
/// use kiki_rss::opml::build_opml;
///
/// let xml = build_opml(&[]).unwrap();
/// assert!(xml.contains(r#"<opml version="2.0">"#));
/// ```
pub fn build_opml(feeds: &[OpmlFeed]) -> Result<String, OpmlError> {
    let mut writer = Writer::new_with_indent(Cursor::new(Vec::new()), b' ', 2);

    writer.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))?;

    writer
        .create_element("opml")
        .with_attribute(("version", "2.0"))
        .write_inner_content(|writer| {
            writer
                .create_element("head")
                .write_inner_content(|writer| {
                    writer
                        .create_element("title")
                        .write_text_content(BytesText::new("Kiki RSS Feeds"))?;
                    Ok(())
                })?;

            writer
                .create_element("body")
                .write_inner_content(|writer| {
                    let mut tagged: BTreeMap<&str, Vec<&OpmlFeed>> = BTreeMap::new();

                    // Untagged feeds go at the top level
                    for feed in feeds {
                        if feed.tags.is_empty() {
                            write_feed_outline(writer, feed)?;
                        }
                        for tag in &feed.tags {
                            tagged.entry(tag.as_str()).or_default().push(feed);
                        }
                    }

                    // Tagged feeds are grouped into one folder per tag
                    for (tag, tag_feeds) in &tagged {
                        writer
                            .create_element("outline")
                            .with_attribute(("text", *tag))
                            .write_inner_content(|writer| {
                                for feed in tag_feeds {
                                    write_feed_outline(writer, feed)?;
                                }
                                Ok(())
                            })?;
                    }

                    Ok(())
                })?;

            Ok(())
        })?;

    let buf = writer.into_inner().into_inner();
    String::from_utf8(buf)
        .map_err(|e| OpmlError::Write(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))
}

/// Write a single feed outline element as an empty (self-closing) tag.
fn write_feed_outline(
    writer: &mut Writer<Cursor<Vec<u8>>>,
    feed: &OpmlFeed,
) -> std::io::Result<()> {
    writer
        .create_element("outline")
        .with_attribute(("type", "rss"))
        .with_attribute(("text", feed.title.as_str()))
        .with_attribute(("xmlUrl", feed.url.as_str()))
        .write_empty()?;
    Ok(())
}

/// Load every feed in the database, along with its tags, ordered by title.
///
/// Feeds without a URL are exported with an empty `xmlUrl`.
///
/// # Errors
///
/// Returns [`OpmlError::Database`] if a query fails.
pub fn export_feeds(conn: &Connection) -> Result<Vec<OpmlFeed>, OpmlError> {
    let mut feeds_stmt = conn.prepare("SELECT id, title, url FROM feeds ORDER BY title")?;
    let mut tags_stmt = conn.prepare(
        "SELECT t.name FROM tags t
         INNER JOIN feed_tags ft ON ft.tag_id = t.id
         WHERE ft.feed_id = ?1
         ORDER BY t.name",
    )?;

    let rows = feeds_stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let mut feeds = Vec::with_capacity(rows.len());
    for (id, title, url) in rows {
        let tags = tags_stmt
            .query_map([id], |row| row.get(0))?
            .collect::<Result<Vec<String>, _>>()?;
        feeds.push(OpmlFeed {
            title,
            url: url.unwrap_or_default(),
            tags,
        });
    }

    Ok(feeds)
}

/// Add feeds to the database in a single transaction.
///
/// Feeds whose URL already exists in the database are skipped and left
/// untouched. New feeds are given `fetch_interval_seconds` as their
/// `min_fetch_interval_seconds` (normally
/// [`FeedFetchSettings::default_fetch_interval_seconds`](crate::config::FeedFetchSettings::default_fetch_interval_seconds))
/// and are created along with any tags they carry, except
/// for tag names reserved for system tags (see
/// [`is_reserved_tag_name`](crate::db::tags::is_reserved_tag_name)), which
/// are ignored. The
/// caller is responsible for scheduling fetches of the new feeds; a running
/// server picks them up on its next scheduling pass, since new feeds are
/// immediately due.
///
/// # Errors
///
/// Returns [`OpmlError::Database`] if a query fails, in which case no feeds
/// are imported.
pub fn import_feeds(
    conn: &mut Connection,
    feeds: &[OpmlFeed],
    fetch_interval_seconds: u64,
) -> Result<ImportSummary, OpmlError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut summary = ImportSummary::default();

    {
        let mut exists_stmt = tx.prepare("SELECT 1 FROM feeds WHERE url = ?1 LIMIT 1")?;
        let mut insert_feed_stmt = tx.prepare(
            "INSERT INTO feeds (title, url, min_fetch_interval_seconds)
             VALUES (?1, ?2, ?3) RETURNING id",
        )?;
        let mut insert_tag_stmt = tx.prepare("INSERT OR IGNORE INTO tags (name) VALUES (?1)")?;
        let mut tag_id_stmt = tx.prepare("SELECT id FROM tags WHERE name = ?1")?;
        let mut feed_tag_stmt =
            tx.prepare("INSERT OR IGNORE INTO feed_tags (feed_id, tag_id) VALUES (?1, ?2)")?;

        for feed in feeds {
            let exists = exists_stmt
                .query_row([&feed.url], |_| Ok(()))
                .optional()?
                .is_some();
            if exists {
                summary.skipped += 1;
                continue;
            }

            let feed_id: i64 = insert_feed_stmt
                .query_row((&feed.title, &feed.url, fetch_interval_seconds), |row| {
                    row.get(0)
                })?;
            summary.imported.push(feed_id);

            for tag in &feed.tags {
                if is_reserved_tag_name(tag) {
                    continue;
                }
                insert_tag_stmt.execute([tag])?;
                let tag_id: i64 = tag_id_stmt.query_row([tag], |row| row.get(0))?;
                feed_tag_stmt.execute((feed_id, tag_id))?;
            }
        }
    }

    tx.commit()?;
    Ok(summary)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::db::ConnectionBuilder;

    fn feed(title: &str, url: &str, tags: &[&str]) -> OpmlFeed {
        OpmlFeed {
            title: title.to_string(),
            url: url.to_string(),
            tags: tags.iter().map(|t| t.to_string()).collect(),
        }
    }

    #[test]
    fn test_parse_nested_folders() {
        let xml = r#"<?xml version="1.0"?>
<opml version="2.0"><body>
  <outline text="Top" xmlUrl="http://example.com/top"/>
  <outline text="Tech">
    <outline text="Rust">
      <outline title="Blog" xmlUrl="http://example.com/rust"/>
    </outline>
    <outline text="Other" xmlUrl="http://example.com/other"></outline>
  </outline>
  <outline text="After" xmlUrl="http://example.com/after"/>
</body></opml>"#;

        assert_eq!(
            parse_opml(xml).unwrap(),
            vec![
                feed("Top", "http://example.com/top", &[]),
                feed("Blog", "http://example.com/rust", &["Tech", "Rust"]),
                feed("Other", "http://example.com/other", &["Tech"]),
                feed("After", "http://example.com/after", &[]),
            ]
        );
    }

    #[test]
    fn test_parse_unescapes_attributes() {
        let xml = r#"<opml><body>
  <outline text="A &amp; B" xmlUrl="http://example.com/?a=1&amp;b=2"/>
</body></opml>"#;
        assert_eq!(
            parse_opml(xml).unwrap(),
            vec![feed("A & B", "http://example.com/?a=1&b=2", &[])]
        );
    }

    #[test]
    fn test_parse_merges_duplicate_feeds() {
        let xml = r#"<opml><body>
  <outline text="news"><outline text="F" xmlUrl="http://example.com/f"/></outline>
  <outline text="tech"><outline text="F" xmlUrl="http://example.com/f"/></outline>
</body></opml>"#;
        assert_eq!(
            parse_opml(xml).unwrap(),
            vec![feed("F", "http://example.com/f", &["news", "tech"])]
        );
    }

    #[test]
    fn test_parse_skips_empty_xml_url() {
        let xml = r#"<opml><body>
  <outline text="Empty" xmlUrl=""/>
  <outline text="Also empty" xmlUrl="">
    <outline text="F" xmlUrl="http://example.com/f"/>
  </outline>
</body></opml>"#;
        // An outline with an empty xmlUrl is neither a feed nor a folder
        assert_eq!(
            parse_opml(xml).unwrap(),
            vec![feed("F", "http://example.com/f", &[])]
        );
    }

    #[test]
    fn test_parse_invalid_xml() {
        assert!(parse_opml("<opml><body><outline></body>").is_err());
    }

    #[test]
    fn test_build_then_parse_round_trip() {
        let feeds = vec![
            feed("Plain", "http://example.com/plain", &[]),
            feed(
                "Tagged & <escaped>",
                "http://example.com/?x=1&y=2",
                &["a", "b"],
            ),
        ];
        assert_eq!(parse_opml(&build_opml(&feeds).unwrap()).unwrap(), feeds);
    }

    #[test]
    fn test_import_and_export_database() {
        let mut conn = ConnectionBuilder::default()
            .in_memory()
            .create()
            .build()
            .unwrap();

        let feeds = vec![
            feed("B", "http://example.com/b", &["tech"]),
            feed("A", "http://example.com/a", &[]),
        ];
        let summary = import_feeds(&mut conn, &feeds, 600).unwrap();
        assert_eq!(summary.imported.len(), 2);
        assert_eq!(summary.skipped, 0);

        let intervals = conn
            .prepare("SELECT min_fetch_interval_seconds FROM feeds")
            .unwrap()
            .query_map([], |row| row.get::<_, i64>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(intervals, vec![600, 600]);

        // Importing the same feeds again skips them all
        let summary = import_feeds(&mut conn, &feeds, 600).unwrap();
        assert!(summary.imported.is_empty());
        assert_eq!(summary.skipped, 2);

        assert_eq!(
            export_feeds(&conn).unwrap(),
            vec![
                feed("A", "http://example.com/a", &[]),
                feed("B", "http://example.com/b", &["tech"]),
            ]
        );
    }
}
