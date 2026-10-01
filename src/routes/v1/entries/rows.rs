//! Reading entries out of the database into [`ListEntriesResponseEntry`]
//! values, shared by every endpoint that lists entries.

use crate::db::favicons::favicon_hash_sql;
use crate::routes::v1::assets::read_asset_url_column;
use crate::routes::v1::entries::ListEntriesResponseEntry;
use crate::routes::v1::tags::list_tags::TagResponse;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// The columns [`entry_from_row`] reads, in order, for the entries table
/// aliased `e`. Columns selected after these start at index
/// [`ENTRY_COLUMN_COUNT`].
pub(crate) fn entry_columns() -> String {
    format!(
        "e.id, e.feed_id, e.source_id, e.syndication_format, e.guid, e.published_at, \
         e.title, e.url, e.content, e.ingested_at, {}, \
         (SELECT f.title FROM feeds f WHERE f.id = e.feed_id)",
        favicon_hash_sql("e.feed_id")
    )
}

/// The number of columns in [`entry_columns`].
pub(crate) const ENTRY_COLUMN_COUNT: usize = 12;

/// Format a Unix timestamp as RFC3339, as entries' times are reported.
pub(crate) fn rfc3339(secs: i64) -> Option<String> {
    chrono::DateTime::from_timestamp_secs(secs).map(|d| d.to_rfc3339())
}

/// Read an entry from a row whose first columns are [`entry_columns`].
///
/// The entry's `tags` are left empty; fill them in with [`attach_tags`].
pub(crate) fn entry_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<ListEntriesResponseEntry> {
    Ok(ListEntriesResponseEntry {
        id: row.get(0)?,
        feed_id: row.get(1)?,
        source_id: row.get(2)?,
        syndication_format: row.get(3)?,
        guid: row.get(4)?,
        published_at: rfc3339(row.get(5)?),
        title: row.get(6)?,
        url: row.get(7)?,
        content: row.get(8)?,
        ingested_at: rfc3339(row.get(9)?),
        feed_favicon_url: read_asset_url_column(row, 10)?,
        feed_title: row.get(11)?,
        tags: Vec::new(),
    })
}

/// Load the tags of each of the entries `ids`, user and system tags alike,
/// in one query. Entries without tags are left out of the map.
pub(crate) fn load_entry_tags(
    conn: &Connection,
    ids: &[i64],
) -> rusqlite::Result<HashMap<i64, Vec<TagResponse>>> {
    let mut tags: HashMap<i64, Vec<TagResponse>> = HashMap::new();
    if ids.is_empty() {
        return Ok(tags);
    }
    let ids = serde_json::to_string(ids)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(e.into()))?;
    let mut stmt = conn.prepare(
        "SELECT t.id, t.name, t.kind, et.entry_id
         FROM entry_tags et JOIN tags t ON t.id = et.tag_id
         WHERE et.entry_id IN (SELECT value FROM json_each(?1))
         ORDER BY t.id",
    )?;
    let rows = stmt.query_map([ids], |row| {
        Ok((row.get::<_, i64>(3)?, TagResponse::from_row(row)?))
    })?;
    for row in rows {
        let (entry_id, tag) = row?;
        tags.entry(entry_id).or_default().push(tag);
    }
    Ok(tags)
}

/// Fill in the `tags` of each of `entries`.
pub(crate) fn attach_tags(
    conn: &Connection,
    entries: &mut [ListEntriesResponseEntry],
) -> rusqlite::Result<()> {
    let ids: Vec<i64> = entries.iter().map(|e| e.id).collect();
    let mut tags = load_entry_tags(conn, &ids)?;
    for entry in entries {
        entry.tags = tags.remove(&entry.id).unwrap_or_default();
    }
    Ok(())
}

/// The order to list entries in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum EntrySort {
    /// Newest publication time first, then descending ID.
    #[default]
    PublishedAt,
    /// Ascending ID, i.e. in the order Kiki stored the entries. Combined
    /// with `since_id`, pages through entries stored since a client last
    /// synced without missing any.
    Id,
    /// Descending ID, i.e. the entries Kiki stored most recently first.
    /// Combined with `max_id`, pages back through older entries.
    IdDesc,
}

impl EntrySort {
    /// The `ORDER BY` clause for this order, for the entries table aliased
    /// `e`.
    pub(crate) fn order_by(self) -> &'static str {
        match self {
            EntrySort::PublishedAt => "e.published_at DESC, e.id DESC",
            EntrySort::Id => "e.id ASC",
            EntrySort::IdDesc => "e.id DESC",
        }
    }
}

/// An SQL condition on the entries table aliased `e` that holds when its id
/// is greater than `since_id` and less than `max_id`, either of which may be
/// `None` to leave that side open.
///
/// The bounds are written into the SQL as literals, rather than bound as
/// parameters that might be NULL, so that SQLite can seek straight to them
/// in the table; being integers, they need no escaping.
pub(crate) fn id_range(since_id: Option<i64>, max_id: Option<i64>) -> String {
    let mut conditions = vec!["1".to_string()];
    if let Some(since_id) = since_id {
        conditions.push(format!("e.id > {since_id}"));
    }
    if let Some(max_id) = max_id {
        conditions.push(format!("e.id < {max_id}"));
    }
    conditions.join(" AND ")
}
