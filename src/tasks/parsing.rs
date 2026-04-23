use crate::scripting::FeedEntry;
use anyhow::Result;
use chrono::Utc;

/// Atom-specific feed-level data captured from a parsed feed.
#[derive(Default)]
pub(super) struct AtomFeedIngestData {
    pub(super) atom_uri: Option<String>,
    pub(super) atom_language_tag: Option<String>,
    pub(super) rights: Option<String>,
    pub(super) generator: Option<atom_syndication::Generator>,
    pub(super) logo: Option<String>,
    pub(super) icon: Option<String>,
    pub(super) authors: Vec<String>,
    pub(super) contributors: Vec<String>,
    pub(super) categories: Vec<atom_syndication::Category>,
}

/// Atom-specific per-entry data captured from a parsed entry.
#[derive(Default)]
pub(super) struct AtomEntryIngestData {
    pub(super) rights: Option<String>,
    pub(super) authors: Vec<String>,
    pub(super) contributors: Vec<String>,
    pub(super) categories: Vec<atom_syndication::Category>,
}

/// RSS-specific per-entry data captured from a parsed item.
#[derive(Default)]
pub(super) struct RssEntryIngestData {
    pub(super) description: Option<String>,
    pub(super) comments: Option<String>,
    pub(super) author: Option<String>,
    pub(super) enclosure_url: Option<String>,
    pub(super) enclosure_length: Option<i64>,
    pub(super) enclosure_mime_type: Option<String>,
    pub(super) categories: Vec<rss::Category>,
}

pub(super) fn extract_atom_feed_data(feed: &atom_syndication::Feed) -> AtomFeedIngestData {
    AtomFeedIngestData {
        atom_uri: feed.base.clone(),
        atom_language_tag: feed.lang.clone(),
        rights: feed.rights.as_ref().map(|r| r.value.clone()),
        generator: feed.generator.clone(),
        logo: feed.logo.clone(),
        icon: feed.icon.clone(),
        authors: feed.authors.iter().map(|p| p.name.clone()).collect(),
        contributors: feed.contributors.iter().map(|p| p.name.clone()).collect(),
        categories: feed.categories.clone(),
    }
}

/// Extract both a [`FeedEntry`] and the Atom-specific sub-object from an
/// Atom entry.
pub(super) fn atom_entry_to_parts(
    feed_id: i64,
    entry: atom_syndication::Entry,
) -> (FeedEntry, AtomEntryIngestData) {
    let ingest = AtomEntryIngestData {
        rights: entry.rights.as_ref().map(|r| r.value.clone()),
        authors: entry.authors.iter().map(|p| p.name.clone()).collect(),
        contributors: entry.contributors.iter().map(|p| p.name.clone()).collect(),
        categories: entry.categories.clone(),
    };
    let feed_entry = FeedEntry {
        feed_id,
        syndication_format: "atom".to_string(),
        guid: entry.id,
        published_at: entry.published.map(|d| d.to_utc().timestamp()),
        title: entry.title.value,
        url: entry.links.into_iter().next().map(|l| l.href),
        content: entry.content.and_then(|c| c.value),
        tags: vec![],
    };
    (feed_entry, ingest)
}

/// Extract both a [`FeedEntry`] and the RSS-specific sub-object from an
/// RSS item.
pub(super) fn rss_item_to_parts(feed_id: i64, item: rss::Item) -> (FeedEntry, RssEntryIngestData) {
    let rss::Item {
        pub_date,
        guid,
        title,
        link,
        description,
        author,
        comments,
        enclosure,
        categories,
        ..
    } = item;

    let timestamp = pub_date
        .as_deref()
        .and_then(|d| chrono::DateTime::parse_from_rfc2822(d).ok())
        .map(|d| d.timestamp())
        .unwrap_or_else(|| Utc::now().timestamp());

    let guid = guid.map(|g| g.value).unwrap_or_else(|| {
        format!(
            "rss-{}-{}",
            timestamp,
            title.as_deref().unwrap_or("no-title")
        )
    });

    let (enclosure_url, enclosure_length, enclosure_mime_type) = match enclosure {
        Some(e) => (Some(e.url), e.length.parse::<i64>().ok(), Some(e.mime_type)),
        None => (None, None, None),
    };

    let ingest = RssEntryIngestData {
        description: description.clone(),
        comments,
        author,
        enclosure_url,
        enclosure_length,
        enclosure_mime_type,
        categories,
    };

    let feed_entry = FeedEntry {
        feed_id,
        syndication_format: "rss".to_string(),
        guid,
        published_at: Some(timestamp),
        title: title.unwrap_or_default(),
        url: link,
        content: description,
        tags: vec![],
    };
    (feed_entry, ingest)
}

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
/// directly on `entries.id` and are cleared by the `INSERT OR REPLACE INTO
/// entries` CASCADE; we also delete defensively in case we're called on an
/// entry that wasn't replaced (e.g. a script-filtered re-ingest).
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
pub(super) fn insert_rss_entry_data(
    tx: &rusqlite::Transaction,
    entry_id: i64,
    data: &RssEntryIngestData,
) -> Result<()> {
    tx.execute("DELETE FROM rss_entry_data WHERE entry_id = ?1", [entry_id])?;
    tx.execute(
        "INSERT INTO rss_entry_data (
            entry_id, description, comments, author,
            enclosure_url, enclosure_length, enclosure_mime_type
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            entry_id,
            data.description,
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
