use crate::server::AppState;
use axum::{
    extract::State,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};
use quick_xml::events::BytesText;
use quick_xml::Writer;
use std::io::Cursor;
use tokio::task;
use tracing::{event, Level};

/// Feed with its tags for OPML export.
struct FeedWithTags {
    title: String,
    url: Option<String>,
    tags: Vec<String>,
}

/// Route handler for exporting feeds as OPML.
///
/// Feeds are grouped into folders by tag. Untagged feeds appear at the
/// top level. Feeds with multiple tags appear in each corresponding folder.
#[axum::debug_handler]
pub async fn export_opml(State(state): State<AppState>) -> Result<Response, Response> {
    let conn = state.conn_pool.get().map_err(|e| {
        event!(Level::ERROR, "failed to get database connection: {:?}", e);
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
    })?;

    let result = task::spawn_blocking(move || {
        // Get all feeds with their tags
        let mut stmt = conn
            .prepare("SELECT f.id, f.title, f.url FROM feeds f ORDER BY f.title")
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?;

        let feeds_raw: Vec<(i64, String, Option<String>)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<Result<Vec<_>, _>>()?;

        let mut feeds = Vec::new();
        for (feed_id, title, url) in feeds_raw {
            let tags: Vec<String> = conn
                .prepare(
                    "SELECT t.name FROM tags t
                     INNER JOIN feed_tags ft ON ft.tag_id = t.id
                     WHERE ft.feed_id = ?1
                     ORDER BY t.name",
                )?
                .query_map([feed_id], |row| row.get(0))?
                .collect::<Result<Vec<_>, _>>()?;

            feeds.push(FeedWithTags { title, url, tags });
        }

        Ok::<Vec<FeedWithTags>, rusqlite::Error>(feeds)
    })
    .await
    .inspect_err(|e| {
        event!(Level::ERROR, "task error in export_opml: {:?}", e);
    });

    let feeds = match result {
        Ok(Ok(f)) => f,
        _ => {
            return Err(
                (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
            );
        }
    };

    // Build OPML XML using quick_xml Writer
    let xml = build_opml_xml(&feeds).map_err(|e| {
        event!(Level::ERROR, "failed to build OPML XML: {:?}", e);
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
    })?;

    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/xml")],
        xml,
    )
        .into_response())
}

/// Write a single feed outline element as an empty (self-closing) tag.
fn write_feed_outline(
    writer: &mut Writer<Cursor<Vec<u8>>>,
    feed: &FeedWithTags,
) -> std::io::Result<()> {
    writer
        .create_element("outline")
        .with_attribute(("type", "rss"))
        .with_attribute(("text", feed.title.as_str()))
        .with_attribute(("xmlUrl", feed.url.as_deref().unwrap_or("")))
        .write_empty()?;
    Ok(())
}

fn build_opml_xml(feeds: &[FeedWithTags]) -> Result<String, std::io::Error> {
    let mut writer = Writer::new_with_indent(Cursor::new(Vec::new()), b' ', 2);

    writer.write_event(quick_xml::events::Event::Decl(
        quick_xml::events::BytesDecl::new("1.0", Some("UTF-8"), None),
    ))?;

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
                    // Collect all unique tags and group feeds
                    let mut tag_feeds: std::collections::BTreeMap<&str, Vec<&FeedWithTags>> =
                        std::collections::BTreeMap::new();
                    let mut untagged: Vec<&FeedWithTags> = Vec::new();

                    for feed in feeds {
                        if feed.tags.is_empty() {
                            untagged.push(feed);
                        } else {
                            for tag in &feed.tags {
                                tag_feeds.entry(tag.as_str()).or_default().push(feed);
                            }
                        }
                    }

                    // Write untagged feeds at top level
                    for feed in &untagged {
                        write_feed_outline(writer, feed)?;
                    }

                    // Write tagged feeds grouped in folders
                    for (tag, tag_feeds) in &tag_feeds {
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
    String::from_utf8(buf).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}
