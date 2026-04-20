use rusqlite::{Connection, Params};
use serde::{Deserialize, Serialize};

/// RSS `<category>` element for a single entry.
#[derive(Debug, Clone, Deserialize, Serialize, utoipa::ToSchema)]
pub struct RssCategory {
    pub category: String,
    pub domain: Option<String>,
}

/// Format-specific data for an RSS entry: description, comments link,
/// author, enclosure (url/length/mime_type), and categories.
#[derive(Debug, Clone, Deserialize, Serialize, utoipa::ToSchema)]
pub struct RssEntryData {
    pub description: Option<String>,
    pub comments: Option<String>,
    pub author: Option<String>,
    pub enclosure_url: Option<String>,
    pub enclosure_length: Option<i64>,
    pub enclosure_mime_type: Option<String>,
    pub categories: Vec<RssCategory>,
}

/// An Atom `<category>` element as stored in the `atom_categories` table.
#[derive(Debug, Clone, Deserialize, Serialize, utoipa::ToSchema)]
pub struct AtomCategory {
    pub term: String,
    pub scheme: Option<String>,
    pub label: Option<String>,
}

/// Format-specific data for an Atom entry: rights, authors, contributors,
/// categories.
#[derive(Debug, Clone, Deserialize, Serialize, utoipa::ToSchema)]
pub struct AtomEntryData {
    pub rights: Option<String>,
    pub authors: Vec<String>,
    pub contributors: Vec<String>,
    pub categories: Vec<AtomCategory>,
}

/// `query_row` that maps `QueryReturnedNoRows` to `Ok(None)`.
pub(crate) fn query_optional<T, P, F>(
    conn: &Connection,
    sql: &str,
    params: P,
    map: F,
) -> rusqlite::Result<Option<T>>
where
    P: Params,
    F: FnOnce(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
{
    match conn.query_row(sql, params, map) {
        Ok(v) => Ok(Some(v)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Execute a single-column `String` query keyed on one `i64` parameter.
pub(crate) fn query_strings(
    conn: &Connection,
    sql: &str,
    id: i64,
) -> rusqlite::Result<Vec<String>> {
    conn.prepare(sql)?
        .query_map([id], |row| row.get::<_, String>(0))?
        .collect()
}

/// Execute an `atom_categories`-returning query keyed on one `i64` parameter.
/// The `sql` must return `(category, scheme, label)` in that order.
pub(crate) fn query_atom_categories(
    conn: &Connection,
    sql: &str,
    id: i64,
) -> rusqlite::Result<Vec<AtomCategory>> {
    conn.prepare(sql)?
        .query_map([id], |row| {
            Ok(AtomCategory {
                term: row.get(0)?,
                scheme: row.get(1)?,
                label: row.get(2)?,
            })
        })?
        .collect()
}

/// Load the RSS-specific sub-object for a single entry. Returns `Ok(None)`
/// when no `rss_entry_data` row exists for the entry (e.g. the entry
/// belongs to an Atom feed, or it was ingested before format-specific data
/// was being captured).
pub fn load_rss_entry_data(
    conn: &Connection,
    entry_id: i64,
) -> rusqlite::Result<Option<RssEntryData>> {
    let Some(mut data) = query_optional(
        conn,
        "SELECT description, comments, author,
                enclosure_url, enclosure_length, enclosure_mime_type
         FROM rss_entry_data WHERE entry_id = ?1 LIMIT 1",
        [entry_id],
        |row| {
            Ok(RssEntryData {
                description: row.get(0)?,
                comments: row.get(1)?,
                author: row.get(2)?,
                enclosure_url: row.get(3)?,
                enclosure_length: row.get(4)?,
                enclosure_mime_type: row.get(5)?,
                categories: Vec::new(),
            })
        },
    )?
    else {
        return Ok(None);
    };

    data.categories = conn
        .prepare("SELECT category, domain FROM rss_categories WHERE entry_id = ?1 ORDER BY rowid")?
        .query_map([entry_id], |row| {
            Ok(RssCategory {
                category: row.get(0)?,
                domain: row.get(1)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    Ok(Some(data))
}

/// Load the Atom-specific sub-object for a single entry. Returns `Ok(None)`
/// for entries that aren't from an Atom feed.
pub fn load_atom_entry_data(
    conn: &Connection,
    entry_id: i64,
) -> rusqlite::Result<Option<AtomEntryData>> {
    let is_atom = query_optional(
        conn,
        "SELECT syndication_format = 'atom'
         FROM entries WHERE id = ?1 LIMIT 1",
        [entry_id],
        |row| row.get::<_, bool>(0),
    )?
    .unwrap_or(false);
    if !is_atom {
        return Ok(None);
    }

    let rights = query_optional(
        conn,
        "SELECT rights FROM atom_entry_rights WHERE entry_id = ?1 LIMIT 1",
        [entry_id],
        |row| row.get::<_, Option<String>>(0),
    )?
    .flatten();

    let authors = query_strings(
        conn,
        "SELECT author FROM atom_entry_authors WHERE entry_id = ?1 ORDER BY id",
        entry_id,
    )?;

    let contributors = query_strings(
        conn,
        "SELECT contributor FROM atom_entry_contributors WHERE entry_id = ?1 ORDER BY id",
        entry_id,
    )?;

    let categories = query_atom_categories(
        conn,
        "SELECT ac.category, ac.scheme, ac.label
         FROM atom_entry_categories aec
         JOIN atom_categories ac ON aec.category_id = ac.id
         WHERE aec.entry_id = ?1
         ORDER BY ac.id",
        entry_id,
    )?;

    Ok(Some(AtomEntryData {
        rights,
        authors,
        contributors,
        categories,
    }))
}
