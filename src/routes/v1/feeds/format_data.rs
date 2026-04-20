use crate::routes::v1::entries::format_data::{
    query_atom_categories, query_optional, query_strings, AtomCategory,
};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

/// Placeholder struct for RSS feed-level data. Currently has no fields; a
/// non-null value in the response simply signals "this feed is being
/// ingested as RSS".
#[derive(Debug, Clone, Default, Deserialize, Serialize, utoipa::ToSchema)]
pub struct RssFeedData {}

/// The `<generator>` element of an Atom feed.
#[derive(Debug, Clone, Deserialize, Serialize, utoipa::ToSchema)]
pub struct AtomGenerator {
    pub value: String,
    pub uri: Option<String>,
    pub version: Option<String>,
}

/// Format-specific data for an Atom feed: language tag, rights, generator,
/// logo, icon, authors, contributors, categories.
#[derive(Debug, Clone, Deserialize, Serialize, utoipa::ToSchema)]
pub struct AtomFeedData {
    pub atom_uri: Option<String>,
    pub atom_language_tag: Option<String>,
    pub rights: Option<String>,
    pub generator: Option<AtomGenerator>,
    pub logo: Option<String>,
    pub icon: Option<String>,
    pub authors: Vec<String>,
    pub contributors: Vec<String>,
    pub categories: Vec<AtomCategory>,
}

/// Look up a single `String` column keyed on `feed_id`, returning `None`
/// both when the row is absent and when the column itself is NULL.
fn lookup_feed_scalar(
    conn: &Connection,
    sql: &str,
    feed_id: i64,
) -> rusqlite::Result<Option<String>> {
    Ok(query_optional(conn, sql, [feed_id], |row| row.get::<_, Option<String>>(0))?.flatten())
}

/// Load the RSS-specific sub-object for a single feed. Returns `Ok(None)`
/// for feeds that aren't being ingested as RSS (including those with an
/// unknown format, i.e. that have never been successfully fetched).
pub fn load_rss_feed_data(
    conn: &Connection,
    feed_id: i64,
) -> rusqlite::Result<Option<RssFeedData>> {
    // `syndication_format` is nullable, so read the comparison as
    // `Option<bool>` — NULL compares to NULL, not false.
    let is_rss = query_optional(
        conn,
        "SELECT syndication_format = 'rss' FROM feeds WHERE id = ?1 LIMIT 1",
        [feed_id],
        |row| row.get::<_, Option<bool>>(0),
    )?
    .flatten()
    .unwrap_or(false);
    Ok(is_rss.then(RssFeedData::default))
}

/// Load the Atom-specific sub-object for a single feed. Returns `Ok(None)`
/// when no `atom_feed_data` row exists (i.e. the feed has never been
/// ingested as Atom).
pub fn load_atom_feed_data(
    conn: &Connection,
    feed_id: i64,
) -> rusqlite::Result<Option<AtomFeedData>> {
    let Some((atom_uri, atom_language_tag)) = query_optional(
        conn,
        "SELECT atom_uri, atom_language_tag
         FROM atom_feed_data WHERE feed_id = ?1 LIMIT 1",
        [feed_id],
        |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
            ))
        },
    )?
    else {
        return Ok(None);
    };

    let rights = lookup_feed_scalar(
        conn,
        "SELECT rights FROM atom_feed_rights WHERE feed_id = ?1 LIMIT 1",
        feed_id,
    )?;

    let generator = query_optional(
        conn,
        "SELECT value, uri, version FROM atom_feed_generators WHERE feed_id = ?1 LIMIT 1",
        [feed_id],
        |row| {
            Ok(AtomGenerator {
                value: row.get(0)?,
                uri: row.get(1)?,
                version: row.get(2)?,
            })
        },
    )?;

    let logo = lookup_feed_scalar(
        conn,
        "SELECT uri FROM atom_feed_logos WHERE feed_id = ?1 LIMIT 1",
        feed_id,
    )?;

    let icon = lookup_feed_scalar(
        conn,
        "SELECT uri FROM atom_feed_icons WHERE feed_id = ?1 LIMIT 1",
        feed_id,
    )?;

    let authors = query_strings(
        conn,
        "SELECT author FROM atom_feed_authors WHERE feed_id = ?1 ORDER BY id",
        feed_id,
    )?;

    let contributors = query_strings(
        conn,
        "SELECT contributor FROM atom_feed_contributors WHERE feed_id = ?1 ORDER BY id",
        feed_id,
    )?;

    let categories = query_atom_categories(
        conn,
        "SELECT ac.category, ac.scheme, ac.label
         FROM atom_feed_categories afc
         JOIN atom_categories ac ON afc.category_id = ac.id
         WHERE afc.feed_id = ?1
         ORDER BY ac.id",
        feed_id,
    )?;

    Ok(Some(AtomFeedData {
        atom_uri,
        atom_language_tag,
        rights,
        generator,
        logo,
        icon,
        authors,
        contributors,
        categories,
    }))
}
