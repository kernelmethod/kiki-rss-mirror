use crate::fetcher::FetchManagerCommand;
use crate::server::AppState;
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use quick_xml::events::Event;
use quick_xml::reader::Reader;
use serde::{Deserialize, Serialize};
use tokio::task;
use tracing::{event, Level};

#[derive(Serialize, Deserialize)]
pub struct ImportOpmlResponse {
    pub imported: usize,
}

/// A feed parsed from an OPML outline element.
struct OpmlFeed {
    title: String,
    url: String,
    /// Tag names from the folder hierarchy.
    tags: Vec<String>,
}

/// Route handler for importing feeds from OPML.
///
/// Accepts raw OPML XML in the request body. Parses outline elements,
/// creates feeds and tags, and associates tags based on the OPML folder
/// structure. Triggers a fetch for each newly imported feed.
#[axum::debug_handler]
pub async fn import_opml(
    State(state): State<AppState>,
    body: String,
) -> Result<Response, Response> {
    // Parse OPML to extract feeds
    let feeds = parse_opml(&body).map_err(|e| {
        event!(Level::ERROR, "failed to parse OPML: {:?}", e);
        (StatusCode::BAD_REQUEST, "Invalid OPML format").into_response()
    })?;

    if feeds.is_empty() {
        return Ok((StatusCode::OK, Json(ImportOpmlResponse { imported: 0 })).into_response());
    }

    let conn = state.conn_pool.get().map_err(|e| {
        event!(Level::ERROR, "failed to get database connection: {:?}", e);
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
    })?;

    let result = task::spawn_blocking(move || {
        let mut imported = 0;
        let mut feed_ids = Vec::new();

        for feed in &feeds {
            // Insert the feed (skip if URL already exists)
            let insert_result = conn
                .prepare("INSERT INTO feeds (title, url) VALUES (?1, ?2) RETURNING id")
                .inspect_err(|e| {
                    event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
                })?
                .query_row(rusqlite::params![&feed.title, &feed.url], |row| {
                    row.get::<_, i64>(0)
                });

            let feed_id = match insert_result {
                Ok(id) => id,
                Err(rusqlite::Error::SqliteFailure(err, _))
                    if err.code == rusqlite::ErrorCode::ConstraintViolation =>
                {
                    // Feed with this URL may already exist, skip
                    continue;
                }
                Err(e) => return Err(e),
            };

            imported += 1;
            feed_ids.push(feed_id);

            // Create tags and associations
            for tag_name in &feed.tags {
                // Insert or get existing tag
                conn.execute("INSERT OR IGNORE INTO tags (name) VALUES (?1)", [tag_name])?;

                let tag_id: i64 = conn
                    .prepare("SELECT id FROM tags WHERE name = ?1")?
                    .query_row([tag_name], |row| row.get(0))?;

                // Associate tag with feed
                conn.execute(
                    "INSERT OR IGNORE INTO feed_tags (feed_id, tag_id) VALUES (?1, ?2)",
                    rusqlite::params![feed_id, tag_id],
                )?;
            }
        }

        Ok::<(usize, Vec<i64>), rusqlite::Error>((imported, feed_ids))
    })
    .await
    .inspect_err(|e| {
        event!(Level::ERROR, "task error in import_opml: {:?}", e);
    });

    match result {
        Ok(Ok((imported, feed_ids))) => {
            // Queue fetches for all new feeds
            for id in feed_ids {
                if let Err(e) = state
                    .fetcher_tx
                    .send(FetchManagerCommand::RefreshFeed(id))
                    .await
                {
                    event!(
                        Level::ERROR,
                        "failed to send fetch command for feed {}: {:?}",
                        id,
                        e
                    );
                }
            }

            Ok((StatusCode::CREATED, Json(ImportOpmlResponse { imported })).into_response())
        }
        _ => Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()),
    }
}

fn parse_opml(xml: &str) -> Result<Vec<OpmlFeed>, String> {
    let mut reader = Reader::from_str(xml);
    let mut feeds = Vec::new();
    let mut folder_stack: Vec<String> = Vec::new();
    let mut depth = 0;

    // Track which depth levels are folders (have children)
    // We process outlines in a streaming fashion
    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) if e.name().as_ref() == b"outline" => {
                let attrs = parse_outline_attrs(e);
                depth += 1;

                // If it has an xmlUrl, it's a feed leaf node
                if let Some(url) = &attrs.xml_url {
                    let title = attrs.text.unwrap_or_else(|| url.clone());
                    feeds.push(OpmlFeed {
                        title,
                        url: url.clone(),
                        tags: folder_stack.clone(),
                    });
                } else {
                    // It's a folder — push its name onto the stack
                    let folder_name = attrs.text.unwrap_or_default();
                    if !folder_name.is_empty() {
                        folder_stack.push(folder_name);
                    }
                }
            }
            Ok(Event::Empty(ref e)) if e.name().as_ref() == b"outline" => {
                let attrs = parse_outline_attrs(e);
                if let Some(url) = &attrs.xml_url {
                    let title = attrs.text.unwrap_or_else(|| url.clone());
                    feeds.push(OpmlFeed {
                        title,
                        url: url.clone(),
                        tags: folder_stack.clone(),
                    });
                }
            }
            Ok(Event::End(ref e)) if e.name().as_ref() == b"outline" => {
                depth -= 1;
                // Pop folder if we were in one
                if folder_stack.len() > depth {
                    folder_stack.pop();
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(format!("XML parse error: {}", e)),
            _ => {}
        }
    }

    Ok(feeds)
}

struct OutlineAttrs {
    text: Option<String>,
    xml_url: Option<String>,
}

fn parse_outline_attrs(e: &quick_xml::events::BytesStart) -> OutlineAttrs {
    let mut text = None;
    let mut xml_url = None;

    for attr in e.attributes().flatten() {
        let key = std::str::from_utf8(attr.key.as_ref()).unwrap_or("");
        let val = std::str::from_utf8(&attr.value).unwrap_or("").to_string();

        match key {
            "text" | "title" => {
                if text.is_none() {
                    text = Some(val);
                }
            }
            "xmlUrl" => xml_url = Some(val),
            _ => {}
        }
    }

    OutlineAttrs { text, xml_url }
}
