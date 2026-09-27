//! Writing the format-specific parts of a parsed feed into the database.
//!
//! The parsing itself lives in [`crate::fetcher::parse`], where it can run
//! in the isolated fetcher process; this module only stores the result.

use crate::fetcher::{AtomEntryIngestData, AtomFeedIngestData, RssEntryIngestData};
use anyhow::Result;

/// Upsert the single `atom_feed_data` row for `feed_id` and replace all
/// atom_feed_* child rows (authors, contributors, rights, generator, logo,
/// icon, categories). Children key directly on `feeds.id`, so the extra
/// `atom_feed_data.id` is not needed anywhere outside the row itself.
pub(super) fn upsert_atom_feed_data(
    tx: &rusqlite::Transaction,
    feed_id: i64,
    data: &AtomFeedIngestData,
) -> Result<()> {
    tx.execute(
        "INSERT INTO atom_feed_data (feed_id, atom_uri, atom_language_tag)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(feed_id) DO UPDATE SET
             atom_uri = excluded.atom_uri,
             atom_language_tag = excluded.atom_language_tag",
        rusqlite::params![feed_id, data.atom_uri, data.atom_language_tag],
    )?;

    // Replace per-feed child rows. Rights/generator/logo/icon have a
    // feed_id PRIMARY KEY — one-per-feed semantics — so we delete-and-
    // reinsert to keep the logic uniform.
    tx.execute("DELETE FROM atom_feed_rights WHERE feed_id = ?1", [feed_id])?;
    if let Some(ref rights) = data.rights {
        tx.execute(
            "INSERT INTO atom_feed_rights (feed_id, rights) VALUES (?1, ?2)",
            rusqlite::params![feed_id, rights],
        )?;
    }

    tx.execute(
        "DELETE FROM atom_feed_generators WHERE feed_id = ?1",
        [feed_id],
    )?;
    if let Some(ref gen_) = data.generator {
        tx.execute(
            "INSERT INTO atom_feed_generators (feed_id, value, uri, version)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![feed_id, gen_.value, gen_.uri, gen_.version],
        )?;
    }

    tx.execute("DELETE FROM atom_feed_logos WHERE feed_id = ?1", [feed_id])?;
    if let Some(ref logo) = data.logo {
        tx.execute(
            "INSERT INTO atom_feed_logos (feed_id, uri) VALUES (?1, ?2)",
            rusqlite::params![feed_id, logo],
        )?;
    }

    tx.execute("DELETE FROM atom_feed_icons WHERE feed_id = ?1", [feed_id])?;
    if let Some(ref icon) = data.icon {
        tx.execute(
            "INSERT INTO atom_feed_icons (feed_id, uri) VALUES (?1, ?2)",
            rusqlite::params![feed_id, icon],
        )?;
    }

    tx.execute(
        "DELETE FROM atom_feed_authors WHERE feed_id = ?1",
        [feed_id],
    )?;
    for author in &data.authors {
        tx.execute(
            "INSERT INTO atom_feed_authors (feed_id, author) VALUES (?1, ?2)",
            rusqlite::params![feed_id, author],
        )?;
    }

    tx.execute(
        "DELETE FROM atom_feed_contributors WHERE feed_id = ?1",
        [feed_id],
    )?;
    for contributor in &data.contributors {
        tx.execute(
            "INSERT INTO atom_feed_contributors (feed_id, contributor) VALUES (?1, ?2)",
            rusqlite::params![feed_id, contributor],
        )?;
    }

    tx.execute(
        "DELETE FROM atom_feed_categories WHERE feed_id = ?1",
        [feed_id],
    )?;
    for cat in &data.categories {
        tx.execute(
            "INSERT INTO atom_categories (category, scheme, label)
             VALUES (?1, ?2, ?3)",
            rusqlite::params![cat.term, cat.scheme, cat.label],
        )?;
        let cat_id = tx.last_insert_rowid();
        tx.execute(
            "INSERT INTO atom_feed_categories (feed_id, category_id)
             VALUES (?1, ?2)",
            rusqlite::params![feed_id, cat_id],
        )?;
    }

    Ok(())
}

/// Insert the atom-specific child rows for a single entry. Children key
/// directly on `entries.id`, which is stable across refreshes because
/// entries are updated in place, so the previous refresh's rows are
/// deleted first.
pub(super) fn insert_atom_entry_data(
    tx: &rusqlite::Transaction,
    entry_id: i64,
    data: &AtomEntryIngestData,
) -> Result<()> {
    tx.execute(
        "DELETE FROM atom_entry_rights WHERE entry_id = ?1",
        [entry_id],
    )?;
    if let Some(ref rights) = data.rights {
        tx.execute(
            "INSERT INTO atom_entry_rights (entry_id, rights) VALUES (?1, ?2)",
            rusqlite::params![entry_id, rights],
        )?;
    }

    tx.execute(
        "DELETE FROM atom_entry_authors WHERE entry_id = ?1",
        [entry_id],
    )?;
    for author in &data.authors {
        tx.execute(
            "INSERT INTO atom_entry_authors (entry_id, author) VALUES (?1, ?2)",
            rusqlite::params![entry_id, author],
        )?;
    }

    tx.execute(
        "DELETE FROM atom_entry_contributors WHERE entry_id = ?1",
        [entry_id],
    )?;
    for contributor in &data.contributors {
        tx.execute(
            "INSERT INTO atom_entry_contributors (entry_id, contributor) VALUES (?1, ?2)",
            rusqlite::params![entry_id, contributor],
        )?;
    }

    tx.execute(
        "DELETE FROM atom_entry_categories WHERE entry_id = ?1",
        [entry_id],
    )?;
    for cat in &data.categories {
        tx.execute(
            "INSERT INTO atom_categories (category, scheme, label)
             VALUES (?1, ?2, ?3)",
            rusqlite::params![cat.term, cat.scheme, cat.label],
        )?;
        let cat_id = tx.last_insert_rowid();
        tx.execute(
            "INSERT INTO atom_entry_categories (entry_id, category_id)
             VALUES (?1, ?2)",
            rusqlite::params![entry_id, cat_id],
        )?;
    }

    Ok(())
}

/// Insert the RSS-specific child rows for a single entry.
///
/// `content` is the value just written to `entries.content`. An RSS item's
/// `<description>` is also its content, so the description is only stored
/// when a script changed the content; otherwise the column is left NULL and
/// readers fall back to `entries.content`.
pub(super) fn insert_rss_entry_data(
    tx: &rusqlite::Transaction,
    entry_id: i64,
    data: &RssEntryIngestData,
    content: Option<&str>,
) -> Result<()> {
    let description = data.description.as_deref().filter(|d| Some(*d) != content);

    tx.execute("DELETE FROM rss_entry_data WHERE entry_id = ?1", [entry_id])?;
    tx.execute(
        "INSERT INTO rss_entry_data (
            entry_id, description, comments, author,
            enclosure_url, enclosure_length, enclosure_mime_type
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            entry_id,
            description,
            data.comments,
            data.author,
            data.enclosure_url,
            data.enclosure_length,
            data.enclosure_mime_type,
        ],
    )?;

    tx.execute("DELETE FROM rss_categories WHERE entry_id = ?1", [entry_id])?;
    for cat in &data.categories {
        tx.execute(
            "INSERT OR IGNORE INTO rss_categories (entry_id, category, domain)
             VALUES (?1, ?2, ?3)",
            rusqlite::params![entry_id, cat.name, cat.domain],
        )?;
    }

    Ok(())
}
